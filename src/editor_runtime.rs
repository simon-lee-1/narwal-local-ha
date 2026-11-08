//! Draft lifecycle and crash-safe local map writes. Raw maps and backups stay
//! in private storage; the derived catalog and preview use local MQTT.
use super::{Engine, Inner};
use crate::{
    maps::DisplayTransform,
    room_editor::{DisplayLine, RoomDraft},
    session::{CommandObservation, Operation},
};
use ring::{
    digest,
    rand::{SecureRandom, SystemRandom},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::Write,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::Instant,
};

const MAX_MAP_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum DraftEdit {
    Rename {
        map_id: String,
        revision: String,
        rooms: Vec<u64>,
    },
    Merge {
        map_id: String,
        revision: String,
        rooms: Vec<u64>,
    },
    Split {
        map_id: String,
        revision: String,
        line: [[f64; 2]; 2],
    },
}
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum EditorInput {
    Draft { edit: DraftEdit },
    Save { draft_id: String },
    Cancel { draft_id: String },
    Refresh,
}
#[derive(Clone)]
struct Draft {
    id: String,
    operation: &'static str,
    summary: String,
    catalog_revision: String,
    generation: u64,
    transform: DisplayTransform,
    map: RoomDraft,
}
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Pending {
    draft_id: String,
    map_id: String,
    before: Value,
    candidate: Value,
    aliases: BTreeMap<u64, String>,
}
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Saved {
    version: u8,
    map_id: Option<String>,
    aliases: BTreeMap<u64, String>,
    pending: Option<Pending>,
    last_backup: Option<Value>,
}
impl Default for Saved {
    fn default() -> Self {
        Self {
            version: 1,
            map_id: None,
            aliases: BTreeMap::new(),
            pending: None,
            last_backup: None,
        }
    }
}
struct Store {
    path: PathBuf,
    saved: Saved,
    healthy: bool,
    _lock: File,
}
fn random_id() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| "editor_randomness_failed")?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}
fn private_parent(path: &Path) -> Result<&Path, String> {
    let parent = path.parent().ok_or("editor_private_path_required")?;
    let meta = std::fs::symlink_metadata(parent).map_err(|_| "editor_private_path_required")?;
    if !path.is_absolute()
        || !meta.is_dir()
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.mode() & 0o777 != 0o700
        || std::fs::canonicalize(parent).ok().as_deref() != Some(parent)
    {
        return Err("editor_private_path_required".into());
    }
    Ok(parent)
}
fn parsed_saved(bytes: &[u8]) -> Result<Saved, String> {
    if bytes.len() > 7 * 1024 * 1024 {
        return Err("editor_store_invalid".into());
    }
    let saved: Saved = serde_json::from_slice(bytes).map_err(|_| "editor_store_invalid")?;
    if saved.version != 1
        || saved.aliases.len() > 32
        || saved.aliases.iter().any(|(id, name)| {
            *id > 62
                || name.trim().is_empty()
                || name.len() > 128
                || name.chars().any(char::is_control)
        })
    {
        return Err("editor_store_invalid".into());
    }
    if let Some(pending) = &saved.pending {
        RoomDraft::new(pending.before.clone(), BTreeMap::new())?;
        RoomDraft::new(pending.candidate.clone(), pending.aliases.clone())?;
        if pending.draft_id.len() != 32
            || !pending
                .draft_id
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
            || map_id(&pending.candidate)? != pending.map_id
            || map_id(&pending.before)? != pending.map_id
        {
            return Err("editor_store_invalid".into());
        }
    }
    if let Some(backup) = &saved.last_backup {
        RoomDraft::new(backup.clone(), BTreeMap::new())?;
    }
    Ok(saved)
}
pub(crate) fn validate_editor_bytes(bytes: &[u8]) -> Result<(), String> {
    parsed_saved(bytes).map(|_| ())
}
impl Store {
    fn open(path: &Path) -> Result<Self, String> {
        private_parent(path)?;
        let lock_path = path.with_extension("lock");
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(lock_path)
            .map_err(|_| "editor_store_lock_failed")?;
        let meta = lock.metadata().map_err(|_| "editor_store_lock_failed")?;
        if !meta.is_file()
            || meta.uid() != unsafe { libc::geteuid() }
            || meta.mode() & 0o777 != 0o600
            || meta.nlink() != 1
            || unsafe {
                libc::flock(
                    std::os::fd::AsRawFd::as_raw_fd(&lock),
                    libc::LOCK_EX | libc::LOCK_NB,
                )
            } != 0
        {
            return Err("editor_store_lock_failed".into());
        }
        match std::fs::symlink_metadata(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                    .open(path)
                    .map_err(|_| "editor_store_initialize_failed")?;
                file.write_all(&serde_json::to_vec(&Saved::default()).unwrap())
                    .and_then(|_| file.sync_all())
                    .map_err(|_| "editor_store_initialize_failed")?;
                File::open(private_parent(path)?)
                    .and_then(|file| file.sync_all())
                    .map_err(|_| "editor_store_initialize_failed")?;
            }
            Err(_) => return Err("editor_store_unavailable".into()),
            Ok(_) => {}
        }
        let saved = parsed_saved(&crate::runtime::private_read(path)?)?;
        Ok(Self {
            path: path.into(),
            saved,
            healthy: true,
            _lock: lock,
        })
    }
    fn replace(&mut self, next: Saved) -> Result<(), String> {
        if !self.healthy {
            return Err("editor_persistence_failed".into());
        }
        let result = self.persist(&next);
        if result.is_ok() {
            self.saved = next;
        } else {
            self.healthy = false;
        }
        result
    }
    fn persist(&self, next: &Saved) -> Result<(), String> {
        let failure = || "editor_persistence_failed".to_string();
        let bytes = serde_json::to_vec(next).map_err(|_| failure())?;
        if bytes.len() > 7 * 1024 * 1024 {
            return Err(failure());
        }
        let current = || -> Result<Saved, String> {
            serde_json::from_slice(&crate::runtime::private_read(&self.path)?)
                .map_err(|_| failure())
        };
        if current()? != self.saved {
            return Err(failure());
        }
        let parent = private_parent(&self.path)?;
        let temporary = parent.join(format!(".room-editor-{}.tmp", random_id()?));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temporary)
            .map_err(|_| failure())?;
        let result = (|| {
            file.write_all(&bytes)
                .and_then(|_| file.sync_all())
                .map_err(|_| failure())?;
            if current()? != self.saved {
                return Err(failure());
            }
            std::fs::rename(&temporary, &self.path).map_err(|_| failure())?;
            File::open(parent)
                .and_then(|file| file.sync_all())
                .map_err(|_| failure())?;
            if current()? != *next {
                return Err(failure());
            }
            Ok(())
        })();
        let _ = std::fs::remove_file(temporary);
        result
    }
}

#[derive(Default)]
pub(super) struct EditorState {
    store: Option<Store>,
    draft: Option<Draft>,
    pub(super) catalog: Value,
    name: String,
    room: Option<u64>,
    message: Option<String>,
    confirmed: bool,
    catalog_ready: bool,
    geometry_ready: bool,
}
fn map_id(raw: &Value) -> Result<String, String> {
    let id = raw.get("map_id").ok_or("editor_map_id_missing")?;
    match id {
        Value::String(id) if !id.is_empty() && id.len() <= 128 => Ok(id.clone()),
        Value::Number(id) => Ok(id.to_string()),
        _ => Err("editor_map_id_invalid".into()),
    }
}
fn digest_value(value: &Value) -> String {
    digest::digest(&digest::SHA256, &serde_json::to_vec(value).unwrap())
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
impl EditorState {
    fn aliases(&self, raw: &Value) -> BTreeMap<u64, String> {
        self.store
            .as_ref()
            .filter(|store| store.saved.map_id == map_id(raw).ok())
            .map(|store| {
                store
                    .saved
                    .aliases
                    .iter()
                    .filter(|(id, _)| {
                        raw.pointer("/room_attr/attr")
                            .and_then(Value::as_array)
                            .is_some_and(|attrs| {
                                attrs
                                    .iter()
                                    .any(|attr| attr["room_id"].as_u64() == Some(**id))
                            })
                    })
                    .map(|(id, name)| (*id, name.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }
    pub(super) fn motion_ready(&self) -> bool {
        self.store.as_ref().is_none_or(|store| {
            store.healthy && store.saved.pending.is_none() && self.catalog_ready
        })
    }
    fn writable(&self) -> bool {
        self.store
            .as_ref()
            .is_some_and(|store| store.healthy && store.saved.pending.is_none())
    }
}
impl Inner {
    pub(super) fn check_room_binding(
        &self,
        binding: Option<&(String, String)>,
    ) -> Result<(), String> {
        if let Some((id, revision)) = binding
            && (self.editor.catalog["map_id"].as_str() != Some(id.as_str())
                || self.editor.catalog["revision"].as_str() != Some(revision.as_str())
                || self.map_generation != Some(self.state.generation)
                || !self.state.fresh(Instant::now()))
        {
            return Err("room_catalog_changed".into());
        }
        Ok(())
    }
    pub(super) fn editor_snapshot(&self) -> Value {
        let editor = &self.editor;
        let fresh =
            self.state.fresh(Instant::now()) && self.map_generation == Some(self.state.generation);
        let pending = editor
            .store
            .as_ref()
            .and_then(|store| store.saved.pending.as_ref());
        let draft = editor.draft.as_ref();
        let current = draft.is_some_and(|draft| {
            editor.catalog["revision"].as_str() == Some(&draft.catalog_revision)
                && draft.generation == self.state.generation
                && draft.transform == self.map_display.transform
        });
        let state = if editor.store.is_none() || !fresh {
            "unavailable"
        } else if pending.is_some() {
            "saving"
        } else if draft.is_some() && current && editor.writable() {
            "draft_ready"
        } else if draft.is_some() {
            "draft_blocked"
        } else if editor.message.is_some() {
            "refused"
        } else if editor.confirmed {
            "confirmed"
        } else {
            "idle"
        };
        let selected_name = editor.catalog["rooms"]
            .as_array()
            .and_then(|rooms| rooms.iter().find(|room| room["id"].as_u64() == editor.room))
            .and_then(|room| room["name"].as_str())
            .unwrap_or("");
        json!({
            "state":state, "draft_id":draft.map(|draft|&draft.id),
            "operation":draft.map(|draft|draft.operation),
            "summary":draft.map(|draft|&draft.summary),
            "message":editor.message,
            "can_save":fresh && current && editor.writable() && editor.catalog_ready,
            "name":editor.name,"selected_room":selected_name,
            "capabilities":{"rename":editor.store.is_some(),"split":editor.store.is_some() && editor.geometry_ready,
                "merge":editor.store.is_some() && editor.geometry_ready,"names_are_local":true},
            "unresolved_write":pending.is_some()
        })
    }
    pub(super) fn accept_editor_map(
        &mut self,
        raw: Value,
        fresh_reply: bool,
    ) -> Result<(), String> {
        self.map = Some(crate::maps::parse(&raw)?);
        self.raw_map = Some(raw.clone());
        self.editor.catalog_ready = false;
        self.editor.geometry_ready = false;
        let result = self.refresh_editor_catalog(raw, fresh_reply);
        if let Err(error) = &result {
            self.editor.catalog = Value::Null;
            self.editor.message = Some(error.clone());
        }
        // The editor's stricter schema does not remove an otherwise readable map.
        if self.editor.store.is_some() {
            result
        } else {
            Ok(())
        }
    }
    fn refresh_editor_catalog(&mut self, raw: Value, fresh_reply: bool) -> Result<(), String> {
        if serde_json::to_vec(&raw)
            .map_err(|_| "editor_map_encoding")?
            .len()
            > MAX_MAP_BYTES
        {
            self.editor.catalog_ready = false;
            return Err("editor_map_too_large".into());
        }
        // An uncertain write is reconciled by observation only, never replayed.
        let pending = self
            .editor
            .store
            .as_ref()
            .and_then(|store| store.saved.pending.clone());
        if fresh_reply
            && let Some(pending) = pending
            && crate::room_editor::map_write_matches(&pending.candidate, &raw)?
        {
            let store = self
                .editor
                .store
                .as_mut()
                .ok_or("editor_store_unavailable")?;
            store.replace(Saved {
                version: 1,
                map_id: Some(pending.map_id),
                aliases: pending.aliases,
                pending: None,
                last_backup: Some(pending.before),
            })?;
            self.editor.draft = None;
            self.editor.confirmed = true;
            self.editor.message = None;
        }
        let aliases = self.editor.aliases(&raw);
        if fresh_reply
            && let Some(store) = self.editor.store.as_mut()
            && store.saved.pending.is_none()
            && store.saved.map_id == map_id(&raw).ok()
            && aliases != store.saved.aliases
        {
            let mut next = store.saved.clone();
            next.aliases = aliases.clone();
            store.replace(next)?;
        }
        let draft = RoomDraft::new(raw.clone(), aliases)?;
        let preview = draft.preview(self.map_display.transform)?;
        self.editor.geometry_ready = preview.geometry_available
            && selectable_rooms(&preview)
            && crate::room_editor::validated_set_params(&raw).is_ok();
        let rooms: Vec<_> = preview
            .rooms
            .iter()
            .map(|room| {
                json!({
                    "id":room.id, "name":room.name, "outline":room.outline,
                    "default_state":"unselected",
                    "label":{"text":room.name,"x":room.label.map(|point|point[0]),
                        "y":room.label.map(|point|point[1]),"offset_y":0},
                })
            })
            .collect();
        let revision = digest_value(&json!({
            "map":draft.revision(),"aliases":draft.aliases(),"transform":self.map_display.transform
        }));
        self.editor.catalog = json!({"map_id":map_id(&raw)?,"revision":revision,
            "coordinate_space":"display_pixels","rooms":rooms});
        self.editor.catalog_ready = true;
        if !preview
            .rooms
            .iter()
            .any(|room| Some(room.id) == self.editor.room)
        {
            self.editor.room = preview.rooms.first().map(|room| room.id);
        }
        if let Some(store) = &mut self.mop_settings {
            store.replace_catalog(
                preview
                    .rooms
                    .iter()
                    .map(|room| crate::plan::Room {
                        id: room.id,
                        name: room.name.clone(),
                    })
                    .collect(),
            )?;
        }
        self.map = Some(crate::maps::parse(&draft.preview_raw())?);
        self.raw_map = Some(raw);
        Ok(())
    }
}
fn idle(observation: &CommandObservation) -> bool {
    observation.state
        == crate::control::StatePredicate {
            robot_status: 100,
            work_status: 0,
            station_contact: true,
            error_code: 0,
        }
        && observation
            .mop_context
            .as_ref()
            .is_some_and(|context| context.idle_mop() || context.idle_vacuum())
        && observation.clean_order.as_ref().is_some_and(Vec::is_empty)
}
impl Engine {
    pub async fn configure_editor(&self, path: &Path) -> Result<(), String> {
        let store = Store::open(path)?;
        let mut inner = self.inner.lock().await;
        inner.editor.store = Some(store);
        if let Some(raw) = inner.raw_map.clone() {
            inner.accept_editor_map(raw, false)?;
        }
        drop(inner);
        self.publish().await;
        Ok(())
    }
    pub async fn editor_name(&self, name: String) -> Result<(), String> {
        if name.len() > 128 || name.chars().any(char::is_control) {
            return Err("editor_name_invalid".into());
        }
        self.inner.lock().await.editor.name = name;
        self.publish().await;
        Ok(())
    }
    pub async fn editor_room(&self, name: String) -> Result<(), String> {
        let mut inner = self.inner.lock().await;
        let id = inner.editor.catalog["rooms"]
            .as_array()
            .and_then(|rooms| {
                rooms
                    .iter()
                    .find(|room| room["name"].as_str() == Some(name.as_str()))
            })
            .and_then(|room| room["id"].as_u64())
            .ok_or("editor_room_invalid")?;
        inner.editor.room = Some(id);
        drop(inner);
        self.publish().await;
        Ok(())
    }
    pub async fn room_edit_preview(&self) -> Result<Vec<u8>, String> {
        self.edit_images
            .borrow()
            .clone()
            .ok_or("editor_preview_unavailable".into())
    }
    pub async fn room_editor(&self, input: EditorInput) -> Result<Value, String> {
        let _serial = self.serial.lock().await;
        let result = match input {
            EditorInput::Refresh => self.editor_refresh().await,
            EditorInput::Draft { edit } => self.editor_draft(edit).await,
            EditorInput::Save { draft_id } => self.editor_save(&draft_id).await,
            EditorInput::Cancel { draft_id } => {
                let mut inner = self.inner.lock().await;
                if !inner.editor.writable() {
                    Err("editor_store_unavailable_or_pending".into())
                } else if inner
                    .editor
                    .draft
                    .as_ref()
                    .is_none_or(|draft| draft.id != draft_id)
                {
                    Err("editor_draft_changed".into())
                } else {
                    inner.editor.draft = None;
                    inner.editor.message = None;
                    inner.editor.confirmed = false;
                    self.edit_images.send_replace(None);
                    Ok(json!({"cancelled":true}))
                }
            }
        };
        if let Err(error) = &result {
            self.inner.lock().await.editor.message = Some(error.clone());
        }
        self.publish().await;
        result
    }
    async fn editor_refresh(&self) -> Result<Value, String> {
        let generation = {
            let inner = self.inner.lock().await;
            if !inner.state.connected {
                return Err("robot_disconnected".into());
            }
            inner.state.generation
        };
        let raw = self.request_for(Some(generation), Operation::Map).await?;
        let mut inner = self.inner.lock().await;
        if !inner.state.connected || inner.state.generation != generation {
            return Err("connection_changed".into());
        }
        let unresolved_before = inner
            .editor
            .store
            .as_ref()
            .is_some_and(|store| store.saved.pending.is_some());
        inner.accept_editor_map(raw, true)?;
        inner.map_generation = Some(generation);
        self.images.send_replace(inner.map_png(Instant::now()).ok());
        if inner.editor.draft.is_none() {
            self.edit_images.send_replace(None);
        }
        let unresolved_after = inner
            .editor
            .store
            .as_ref()
            .is_some_and(|store| store.saved.pending.is_some());
        Ok(
            json!({"refreshed":true,"write_reconciled":unresolved_before && !unresolved_after,
            "robot_map_write":false}),
        )
    }
    async fn editor_draft(&self, edit: DraftEdit) -> Result<Value, String> {
        let mut inner = self.inner.lock().await;
        if !inner.editor.writable()
            || !inner.state.fresh(Instant::now())
            || inner.map_generation != Some(inner.state.generation)
        {
            return Err("editor_fresh_local_map_required".into());
        }
        if inner.editor.draft.is_some() {
            return Err("editor_save_or_cancel_first".into());
        }
        let (id, revision) = match &edit {
            DraftEdit::Rename {
                map_id, revision, ..
            }
            | DraftEdit::Merge {
                map_id, revision, ..
            }
            | DraftEdit::Split {
                map_id, revision, ..
            } => (map_id, revision),
        };
        if inner.editor.catalog["map_id"].as_str() != Some(id.as_str())
            || inner.editor.catalog["revision"].as_str() != Some(revision.as_str())
        {
            return Err("editor_catalog_changed".into());
        }
        let raw = inner.raw_map.clone().ok_or("editor_map_unavailable")?;
        let mut draft = RoomDraft::new(raw.clone(), inner.editor.aliases(&raw))?;
        if !matches!(&edit, DraftEdit::Rename { .. }) && !inner.editor.geometry_ready {
            return Err("editor_geometry_unavailable".into());
        }
        let (operation, summary) = match edit {
            DraftEdit::Rename { rooms, .. } if rooms.len() == 1 => {
                draft.rename_alias(rooms[0], &inner.editor.name)?;
                (
                    "rename",
                    format!("Rename the selected room to {}", inner.editor.name.trim()),
                )
            }
            DraftEdit::Merge { rooms, .. } if rooms.len() == 2 && rooms[0] != rooms[1] => {
                draft.merge(rooms[0], rooms[1])?;
                ("merge", "Merge the two selected rooms".to_string())
            }
            DraftEdit::Split { line, .. } => {
                let room = inner.editor.room.ok_or("editor_room_required")?;
                draft.split(
                    room,
                    DisplayLine {
                        start: line[0],
                        end: line[1],
                    },
                    inner.map_display.transform,
                )?;
                (
                    "split",
                    "Split the selected room along the drawn line".to_string(),
                )
            }
            _ => return Err("editor_selection_invalid".into()),
        };
        if draft.action_count() == 0 {
            return Err("editor_no_change".into());
        }
        if draft.geometry_changed() {
            crate::room_editor::validated_set_params(draft.raw_candidate())?;
            if !selectable_rooms(&draft.preview(inner.map_display.transform)?) {
                return Err("editor_room_outline_unavailable".into());
            }
        }
        let png =
            crate::maps::parse(&draft.preview_raw())?.png_display(None, &[], inner.map_display)?;
        let id = random_id()?;
        inner.editor.draft = Some(Draft {
            id: id.clone(),
            operation,
            summary,
            catalog_revision: inner.editor.catalog["revision"].as_str().unwrap().into(),
            generation: inner.state.generation,
            transform: inner.map_display.transform,
            map: draft,
        });
        inner.editor.message = None;
        inner.editor.confirmed = false;
        self.edit_images.send_replace(Some(png));
        Ok(json!({"draft_id":id,"operation":operation,"saved":false}))
    }
    async fn editor_save(&self, id: &str) -> Result<Value, String> {
        let draft = {
            let inner = self.inner.lock().await;
            if !inner.editor.writable() || inner.pending_intent.is_some() {
                return Err("editor_write_unresolved_or_robot_busy".into());
            }
            let draft = inner
                .editor
                .draft
                .as_ref()
                .filter(|draft| draft.id == id)
                .ok_or("editor_draft_changed")?;
            if draft.generation != inner.state.generation
                || draft.transform != inner.map_display.transform
                || inner.editor.catalog["revision"].as_str() != Some(&draft.catalog_revision)
            {
                return Err("editor_catalog_changed".into());
            }
            draft.clone()
        };
        let fresh_map = self
            .request_for(Some(draft.generation), Operation::Map)
            .await?;
        draft.map.validate_base(&fresh_map)?;
        let params = draft
            .map
            .geometry_changed()
            .then(|| crate::room_editor::validated_set_params(draft.map.raw_candidate()))
            .transpose()?;
        let observation = self.command_observation(Some(draft.generation)).await?;
        if !idle(&observation) {
            return Err("editor_idle_docked_robot_required".into());
        }
        {
            let mut inner = self.inner.lock().await;
            if inner.state.generation != draft.generation
                || inner
                    .editor
                    .draft
                    .as_ref()
                    .is_none_or(|current| current.id != id)
                || inner.editor.catalog["revision"].as_str() != Some(&draft.catalog_revision)
                || inner.pending_intent.is_some()
            {
                return Err("editor_catalog_changed".into());
            }
            let store = inner
                .editor
                .store
                .as_mut()
                .ok_or("editor_store_unavailable")?;
            if !draft.map.geometry_changed() {
                store.replace(Saved {
                    version: 1,
                    map_id: Some(map_id(&fresh_map)?),
                    aliases: draft.map.aliases().clone(),
                    pending: None,
                    last_backup: store.saved.last_backup.clone(),
                })?;
                inner.editor.draft = None;
                inner.editor.confirmed = true;
                inner.editor.message = None;
                inner.accept_editor_map(fresh_map, true)?;
                self.images.send_replace(inner.map_png(Instant::now()).ok());
                self.edit_images.send_replace(None);
                return Ok(json!({"saved":true,"local_names":true,"robot_map_write":false}));
            }
            // Persist the exact original and candidate before any possible send.
            let pending = Pending {
                draft_id: id.into(),
                map_id: map_id(&fresh_map)?,
                before: fresh_map.clone(),
                candidate: draft.map.raw_candidate().clone(),
                aliases: draft.map.aliases().clone(),
            };
            let mut next = store.saved.clone();
            next.pending = Some(pending);
            store.replace(next)?;
        }
        self.publish().await;
        let sent = self
            .request_tracked(
                Some(draft.generation),
                Operation::GuardedMapSet {
                    observation,
                    params: params.ok_or("editor_no_geometry_change")?,
                },
            )
            .await;
        let before_send = match &sent {
            Ok(receipt) => receipt["transport_attempted"] == false,
            Err(failure) => failure.before_send,
        };
        if before_send {
            let mut inner = self.inner.lock().await;
            let store = inner
                .editor
                .store
                .as_mut()
                .ok_or("editor_store_unavailable")?;
            let mut next = store.saved.clone();
            next.pending = None;
            store.replace(next)?;
            return Err("editor_write_refused_before_send".into());
        }
        // ACK is not persistence proof. A matching fresh getter is required.
        let actual = self
            .request_for(Some(draft.generation), Operation::Map)
            .await
            .map_err(|_| "editor_write_unresolved_waiting_for_map")?;
        if !crate::room_editor::map_write_matches(draft.map.raw_candidate(), &actual)? {
            return Err("editor_write_unresolved_map_differs".into());
        }
        let mut inner = self.inner.lock().await;
        if inner.state.generation != draft.generation {
            return Err("connection_changed".into());
        }
        inner.accept_editor_map(actual, true)?;
        inner.editor.draft = None;
        inner.editor.confirmed = true;
        inner.editor.message = None;
        self.images.send_replace(inner.map_png(Instant::now()).ok());
        self.edit_images.send_replace(None);
        Ok(json!({"saved":true,"robot_map_write":true,"readback_verified":true}))
    }
}
fn selectable_rooms(preview: &crate::room_editor::DraftPreview) -> bool {
    !preview.rooms.is_empty()
        && preview.rooms.iter().all(|room| {
            room.label.is_some() && room.outline_error.is_none() && room.outline.len() >= 4
        })
}
