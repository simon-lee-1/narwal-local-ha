use serde::Deserialize;
use std::{
    io::Read,
    os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunMode {
    Run,
    InitializeIntent,
    ValidateConfig,
}

pub fn parse_arguments(args: &[String]) -> Result<(PathBuf, RunMode), String> {
    let (path, mode) = match args {
        [_, path] if !path.starts_with('-') => (path, RunMode::Run),
        [_, flag, path] if flag == "--config" => (path, RunMode::Run),
        [_, flag, path] if flag == "--initialize-intent-only" => {
            (path, RunMode::InitializeIntent)
        }
        [_, flag, path] if flag == "--validate-config-only" => {
            (path, RunMode::ValidateConfig)
        }
        _ => return Err("usage: narwal-local-service [--config | --initialize-intent-only | --validate-config-only] /absolute/private/config.json".into()),
    };
    if !Path::new(path).is_absolute() {
        return Err("absolute_config_path_required".into());
    }
    Ok((PathBuf::from(path), mode))
}

/// A per-connection readback budget. Every attempted getter consumes a slot,
/// including failures; no setter or command is retried by this policy.
pub struct PendingMapRefresh {
    last_attempt: Instant,
    remaining: u8,
}
impl PendingMapRefresh {
    pub fn new(after_initial_map: Instant) -> Self {
        Self {
            last_attempt: after_initial_map,
            remaining: 10,
        }
    }
    pub fn should_refresh(&mut self, unresolved: bool, now: Instant) -> bool {
        if !unresolved {
            self.remaining = 10;
            return false;
        }
        if self.remaining == 0
            || now.saturating_duration_since(self.last_attempt) < Duration::from_secs(30)
        {
            return false;
        }
        self.remaining -= 1;
        self.last_attempt = now;
        true
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub socket_path: Option<String>,
    pub frontend_uid: u32,
    pub frontend_executable: Option<String>,
    pub http_address: String,
    pub profile_file: Option<String>,
    pub intent_file: Option<String>,
    pub map_file: Option<String>,
    #[serde(default)]
    pub map_display: crate::maps::MapDisplay,
    pub mqtt: Option<crate::mqtt::MqttConfig>,
    pub mop_settings_file: Option<String>,
    pub room_editor_file: Option<String>,
}

fn validate_private_parent(path: &Path) -> Result<(), String> {
    let parent = path.parent().ok_or("private_path_invalid")?;
    let meta = std::fs::symlink_metadata(parent).map_err(|_| "private_directory_unavailable")?;
    if !path.is_absolute()
        || path.file_name().is_none()
        || !meta.is_dir()
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.mode() & 0o7777 != 0o700
        || std::fs::canonicalize(parent).map_err(|_| "private_directory_unavailable")? != parent
    {
        return Err("unsafe_private_directory".into());
    }
    Ok(())
}

/// Validate existing private configuration and state without opening stores,
/// taking locks, creating files, binding listeners, or contacting any peer.
/// A valid pending record remains pending; this is not a readiness assertion.
pub fn validate_config(path: &Path) -> Result<(), String> {
    let config: Config =
        serde_json::from_slice(&private_read(path)?).map_err(|_| "invalid_configuration")?;
    config
        .http_address
        .parse::<std::net::SocketAddr>()
        .map_err(|_| "invalid_http_address")?;
    let profile = match config.profile_file.as_deref() {
        Some(path) => serde_json::from_slice::<crate::control::CommandProfile>(&private_read(
            Path::new(path),
        )?)
        .map_err(|_| "invalid_command_profile")?,
        None => crate::control::CommandProfile::default(),
    };
    if crate::control::mop_settings_required(&profile) && config.mop_settings_file.is_none() {
        return Err("mop_settings_file_required_for_room_profile".into());
    }
    match config.intent_file.as_deref() {
        Some(path) => crate::intent::validate_existing(Path::new(path), &profile)?,
        None if profile
            .commands
            .keys()
            .any(|name| crate::control::eligible(&profile, name)) =>
        {
            return Err("intent_file_required_for_motion_profile".into());
        }
        None => {}
    }
    if let Some(path) = config.mop_settings_file.as_deref() {
        let settings: crate::plan::Settings =
            serde_json::from_slice(&private_read(Path::new(path))?)
                .map_err(|_| "invalid_mop_settings")?;
        settings.validate()?;
    }
    if let Some(path) = config.map_file.as_deref() {
        let raw = serde_json::from_slice(&private_read(Path::new(path))?)
            .map_err(|_| "invalid_map_file")?;
        crate::maps::parse(&raw).map_err(|_| "invalid_map_file")?;
    }
    if let Some(path) = config.room_editor_file.as_deref() {
        let path = Path::new(path);
        validate_private_parent(path)?;
        match std::fs::symlink_metadata(path) {
            Ok(_) => crate::engine::validate_editor_bytes(&private_read(path)?)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("editor_store_unavailable".into()),
        }
    }
    if let Some(path) = config.socket_path.as_deref() {
        if config.frontend_uid != unsafe { libc::geteuid() } {
            return Err("frontend_requires_same_dedicated_uid".into());
        }
        let path = Path::new(path);
        validate_private_parent(path)?;
        match std::fs::symlink_metadata(path) {
            Ok(meta)
                if meta.file_type().is_socket() && meta.uid() == unsafe { libc::geteuid() } => {}
            Ok(_) => return Err("unsafe_existing_socket".into()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("socket_unavailable".into()),
        }
        let executable = config
            .frontend_executable
            .as_deref()
            .ok_or("frontend_executable_required")?;
        let executable = std::fs::canonicalize(executable).map_err(|_| "frontend_unavailable")?;
        let meta = std::fs::metadata(executable).map_err(|_| "frontend_unavailable")?;
        if !meta.is_file() || meta.uid() != 0 || meta.mode() & 0o022 != 0 {
            return Err("frontend_must_be_root_owned".into());
        }
    }
    if let Some(mqtt) = &config.mqtt {
        crate::mqtt::options(mqtt)?;
    }
    Ok(())
}

pub fn private_read(path: &Path) -> Result<Vec<u8>, String> {
    let parent = path.parent().ok_or("private_path_invalid")?;
    let meta = std::fs::symlink_metadata(parent).map_err(|_| "private_directory_unavailable")?;
    let uid = unsafe { libc::geteuid() };
    if !path.is_absolute()
        || !meta.is_dir()
        || meta.uid() != uid
        || meta.mode() & 0o777 != 0o700
        || std::fs::canonicalize(parent).map_err(|_| "private_directory_unavailable")? != parent
    {
        return Err("unsafe_private_directory".into());
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| "private_file_unavailable")?;
    let before = file.metadata().map_err(|_| "private_file_unavailable")?;
    if !before.is_file()
        || before.uid() != uid
        || before.mode() & 0o777 != 0o600
        || before.nlink() != 1
        || before.len() > 8 * 1024 * 1024
    {
        return Err("unsafe_private_file".into());
    }
    let mut result = Vec::new();
    file.take(8 * 1024 * 1024 + 1)
        .read_to_end(&mut result)
        .map_err(|_| "private_read_failed")?;
    if result.len() > 8 * 1024 * 1024 || result.len() != before.len() as usize {
        return Err("private_file_changed_or_too_large".into());
    }
    Ok(result)
}
