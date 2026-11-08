use narwal_local_service::{
    control::{CommandProfile, LocalInput},
    engine::Engine,
    runtime::{Config, private_read},
    session::{self, Event},
};
use std::{os::unix::fs::MetadataExt, path::Path, time::Duration};
use tokio::{net::UnixListener, sync::mpsc};

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        // Errors are fixed diagnostics, never configuration values or peer data.
        eprintln!("Narwal service: {error}");
        std::process::exit(1);
    }
}
async fn run() -> Result<(), String> {
    unsafe {
        libc::umask(0o077);
    }
    if unsafe { libc::geteuid() } == 0 {
        return Err("run_as_dedicated_unprivileged_user".into());
    }
    let args: Vec<_> = std::env::args().collect();
    let (config_path, initialize_only) = match args.as_slice() {
        [_, config] => (config, false),
        [_, flag, config] if flag == "--initialize-intent-only" => (config, true),
        _ => return Err(
            "usage: narwal-local-service [--initialize-intent-only] /absolute/private/config.json"
                .into(),
        ),
    };
    let config: Config = serde_json::from_slice(&private_read(Path::new(config_path))?)
        .map_err(|_| "invalid_configuration")?;
    let address = config
        .http_address
        .parse()
        .map_err(|_| "invalid_http_address")?;
    let profile = match config.profile_file {
        Some(path) => serde_json::from_slice::<CommandProfile>(&private_read(Path::new(&path))?)
            .map_err(|_| "invalid_command_profile")?,
        None => CommandProfile::default(),
    };
    if narwal_local_service::control::mop_settings_required(&profile)
        && config.mop_settings_file.is_none()
    {
        return Err("mop_settings_file_required_for_room_profile".into());
    }
    if initialize_only {
        let path = config
            .intent_file
            .as_deref()
            .ok_or("intent_file_required")?;
        Engine::initialize_intent(&profile, Path::new(path))?;
        return Ok(());
    }
    let motion_profile = profile
        .commands
        .keys()
        .any(|name| narwal_local_service::control::eligible(&profile, name));
    let engine = match config.intent_file {
        Some(path) => Engine::open_persistent(profile, Path::new(&path))?,
        None if motion_profile => return Err("intent_file_required_for_motion_profile".into()),
        None => Engine::new(profile),
    };
    engine.configure_map_display(config.map_display).await;
    if let Some(path) = config.mop_settings_file {
        engine.configure_mop(Path::new(&path)).await?;
    }
    if let Some(path) = config.map_file {
        let map = serde_json::from_slice(&private_read(Path::new(&path))?)
            .map_err(|_| "invalid_map_file")?;
        engine.load_map(&map).await?;
    }
    let e = engine.clone();
    tokio::spawn(async move {
        let mut clock = tokio::time::interval(Duration::from_secs(1));
        loop {
            clock.tick().await;
            e.publish().await;
        }
    });
    if let Some(mqtt) = config.mqtt {
        let (tx, rx) = mpsc::channel::<LocalInput>(4);
        let e = engine.clone();
        tokio::spawn(async move {
            narwal_local_service::control::dispatch_local(rx, move |input| {
                let e = e.clone();
                async move {
                    let _ = match input {
                        LocalInput::Command(command) => e.command(command).await.map(|_| ()),
                        LocalInput::SelectRoom(room) => {
                            e.change_mop(narwal_local_service::plan::Change::Room(room))
                                .await
                        }
                        LocalInput::SetPasses(passes) => {
                            e.change_mop(narwal_local_service::plan::Change::Passes(passes))
                                .await
                        }
                        LocalInput::SetDampness(dampness) => {
                            e.change_mop(narwal_local_service::plan::Change::Dampness(dampness))
                                .await
                        }
                    };
                }
            })
            .await;
        });
        let state = engine.states.subscribe();
        let images = engine.images.subscribe();
        tokio::spawn(async move {
            if narwal_local_service::mqtt::run(mqtt, state, images, tx)
                .await
                .is_err()
            {
                eprintln!("Narwal service: MQTT bridge stopped");
            }
        });
    }
    let http = narwal_local_service::http::run(engine.clone(), address);
    if let Some(path) = config.socket_path {
        let executable = config
            .frontend_executable
            .ok_or("frontend_executable_required")?;
        tokio::select! {
            result=http=>result,
            result=robot_listener(engine,&path,config.frontend_uid,&executable)=>result,
            _=shutdown()=>Ok(()),
        }
    } else {
        tokio::select! {result=http=>result,_=shutdown()=>Ok(())}
    }
}
async fn shutdown() {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("signal handler");
    tokio::select! {_=terminate.recv()=>{},_=tokio::signal::ctrl_c()=>{}}
}
async fn robot_listener(
    engine: Engine,
    path: &str,
    frontend_uid: u32,
    frontend_executable: &str,
) -> Result<(), String> {
    if frontend_uid != unsafe { libc::geteuid() } {
        return Err("frontend_requires_same_dedicated_uid".into());
    }
    let path = Path::new(path);
    let parent = path.parent().ok_or("socket_path_invalid")?;
    let meta = std::fs::symlink_metadata(parent).map_err(|_| "socket_directory_unavailable")?;
    if !path.is_absolute()
        || !meta.is_dir()
        || meta.mode() & 0o777 != 0o700
        || meta.uid() != unsafe { libc::geteuid() }
        || std::fs::canonicalize(parent).map_err(|_| "socket_directory_unavailable")? != parent
    {
        return Err("unsafe_socket_directory".into());
    }
    let executable =
        std::fs::canonicalize(frontend_executable).map_err(|_| "frontend_unavailable")?;
    let executable_meta = std::fs::metadata(&executable).map_err(|_| "frontend_unavailable")?;
    if executable_meta.uid() != 0 || executable_meta.mode() & 0o022 != 0 {
        return Err("frontend_must_be_root_owned".into());
    }
    // A stale socket is never silently unlinked; systemd RuntimeDirectory owns cleanup.
    let listener = UnixListener::bind(path).map_err(|_| "socket_bind_failed")?;
    let (events, mut rx) = mpsc::channel(16);
    let e = engine.clone();
    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            e.event(event).await;
        }
    });
    let mut generation = 0u64;
    loop {
        let (stream, _) = listener
            .accept()
            .await
            .map_err(|_| "socket_accept_failed")?;
        let credentials = stream
            .peer_cred()
            .map_err(|_| "peer_credentials_unavailable")?;
        let peer = credentials
            .pid()
            .and_then(|pid| std::fs::read_link(format!("/proc/{pid}/exe")).ok());
        if credentials.uid() != frontend_uid || peer.as_ref() != Some(&executable) {
            continue;
        }
        generation = generation.checked_add(1).ok_or("generation_exhausted")?;
        let (requests, receiver) = mpsc::channel(2);
        engine.attach(generation, Some(requests)).await;
        let diagnostics = std::sync::Arc::new(session::Diagnostics::default());
        engine
            .attach_diagnostics(generation, diagnostics.clone())
            .await;
        let e = engine.clone();
        let poll = tokio::spawn(async move {
            // Wait for this session's validated announcement before any requests.
            for _ in 0..80 {
                if e.snapshot().await["connected"] == true {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            if e.refresh().await.is_ok() {
                let _ = e.refresh_map().await;
            }
            let mut interval = tokio::time::interval(Duration::from_secs(10));
            interval.tick().await;
            loop {
                interval.tick().await;
                let _ = e.refresh().await;
            }
        });
        let connection =
            session::serve_observed(stream, generation, receiver, events.clone(), diagnostics);
        tokio::pin!(connection);
        loop {
            tokio::select! {
                _=&mut connection=>break,
                extra=listener.accept()=>{ drop(extra); }
            }
        }
        poll.abort();
        let _ = poll.await;
        engine.attach(generation, None).await;
        engine.event(Event::Disconnected { generation }).await;
    }
}
