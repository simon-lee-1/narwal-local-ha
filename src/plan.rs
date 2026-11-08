//! Qualified local mop choices. Saving a choice never sends a robot command.
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs::OpenOptions,
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub enum Dampness {
    Normal,
    Wet,
}
impl Dampness {
    pub fn wire(self) -> u8 {
        match self {
            Self::Normal => 0,
            Self::Wet => 2,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MopPlan {
    pub rooms: Vec<u64>,
    pub passes: u8,
    pub dampness: Dampness,
}
impl MopPlan {
    pub fn validate(&self, allowed: &[u64]) -> Result<(), String> {
        if self.rooms.is_empty()
            || self.rooms.len() > 32
            || !(1..=3).contains(&self.passes)
            || self.rooms.iter().any(|id| !allowed.contains(id))
            || self.rooms.iter().collect::<BTreeSet<_>>().len() != self.rooms.len()
        {
            return Err("mop_plan_invalid".into());
        }
        Ok(())
    }
    pub fn wire(&self, allowed: &[u64]) -> Result<(Vec<u64>, Vec<u8>), String> {
        self.validate(allowed)?;
        let order: Vec<_> = self
            .rooms
            .iter()
            .flat_map(|id| std::iter::repeat_n(*id, usize::from(self.passes)))
            .collect();
        let floor = vec![self.dampness.wire(); order.len()];
        Ok((order, floor))
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Room {
    pub id: u64,
    pub name: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub version: u8,
    pub rooms: Vec<Room>,
    pub selected: MopPlan,
}
impl Settings {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.version != 1
            || self.rooms.is_empty()
            || self.rooms.len() > 32
            || self.rooms.iter().any(|room| {
                room.id > 255
                    || room.name.trim().is_empty()
                    || room.name.len() > 128
                    || room.name.chars().any(char::is_control)
            })
            || self
                .rooms
                .iter()
                .map(|room| room.id)
                .collect::<BTreeSet<_>>()
                .len()
                != self.rooms.len()
            || self
                .rooms
                .iter()
                .map(|room| &room.name)
                .collect::<BTreeSet<_>>()
                .len()
                != self.rooms.len()
        {
            return Err("mop_settings_invalid".into());
        }
        self.selected.validate(&self.ids())
    }
    pub fn ids(&self) -> Vec<u64> {
        self.rooms.iter().map(|room| room.id).collect()
    }
}
pub enum Change {
    Room(String),
    Passes(u8),
    Dampness(Dampness),
}
pub(crate) struct Store {
    path: PathBuf,
    settings: Settings,
    failed: bool,
}
impl Store {
    pub(crate) fn open(path: &Path) -> Result<Self, String> {
        let settings: Settings = serde_json::from_slice(&crate::runtime::private_read(path)?)
            .map_err(|_| "mop_settings_invalid")?;
        settings.validate()?;
        Ok(Self {
            path: path.into(),
            settings,
            failed: false,
        })
    }
    pub(crate) fn settings(&self) -> &Settings {
        &self.settings
    }
    pub(crate) fn healthy(&self) -> bool {
        !self.failed
    }
    pub(crate) fn replace_catalog(&mut self, rooms: Vec<Room>) -> Result<(), String> {
        if self.failed {
            return Err("mop_settings_persistence_failed".into());
        }
        let mut next = self.settings.clone();
        next.rooms = rooms;
        next.selected
            .rooms
            .retain(|id| next.rooms.iter().any(|room| room.id == *id));
        if next.selected.rooms.is_empty() {
            next.selected.rooms = vec![next.rooms.first().ok_or("room_catalog_empty")?.id];
        }
        next.validate()?;
        if next == self.settings {
            return Ok(());
        }
        if let Err(error) = self.persist(&next) {
            self.failed = true;
            return Err(error);
        }
        self.settings = next;
        Ok(())
    }
    pub(crate) fn update(&mut self, change: Change) -> Result<(), String> {
        if self.failed {
            return Err("mop_settings_persistence_failed".into());
        }
        let mut next = self.settings.clone();
        match change {
            Change::Room(name) => {
                let room = next
                    .rooms
                    .iter()
                    .find(|room| room.name == name)
                    .ok_or("room_selection_invalid")?;
                next.selected.rooms = vec![room.id];
            }
            Change::Passes(passes) => next.selected.passes = passes,
            Change::Dampness(dampness) => next.selected.dampness = dampness,
        }
        next.validate()?;
        if next == self.settings {
            return Ok(());
        }
        let result = self.persist(&next);
        if result.is_err() {
            self.failed = true;
        }
        result?;
        self.settings = next;
        Ok(())
    }
    fn persist(&self, next: &Settings) -> Result<(), String> {
        let failure = || "mop_settings_persistence_failed".to_string();
        // Refuse replacement if another operator changed the catalog or plan.
        let current: Settings = serde_json::from_slice(&crate::runtime::private_read(&self.path)?)
            .map_err(|_| failure())?;
        if current != self.settings {
            return Err(failure());
        }
        let parent = self.path.parent().ok_or_else(failure)?;
        let temporary = parent.join(format!(".mop-settings-{}.tmp", std::process::id()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temporary)
            .map_err(|_| failure())?;
        let result = (|| {
            let bytes = serde_json::to_vec(next).map_err(|_| failure())?;
            file.write_all(&bytes).map_err(|_| failure())?;
            file.sync_all().map_err(|_| failure())?;
            // Recheck the existing private path immediately before replacement.
            let current: Settings =
                serde_json::from_slice(&crate::runtime::private_read(&self.path)?)
                    .map_err(|_| failure())?;
            if current != self.settings {
                return Err(failure());
            }
            std::fs::rename(&temporary, &self.path).map_err(|_| failure())?;
            std::fs::File::open(parent)
                .and_then(|file| file.sync_all())
                .map_err(|_| failure())?;
            let actual: Settings =
                serde_json::from_slice(&crate::runtime::private_read(&self.path)?)
                    .map_err(|_| failure())?;
            if actual != *next {
                return Err(failure());
            }
            Ok(())
        })();
        if temporary.exists() {
            let _ = std::fs::remove_file(&temporary);
        }
        result
    }
}
