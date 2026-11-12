//! Durable, single-owner motion intent. No device identity or payload is stored.
use crate::control::CommandProfile;
use serde::{Deserialize, Serialize};
use std::{
    ffi::CString,
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
};

const MAX_RECORD: u64 = 4096;
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Record {
    schema: u8,
    profile_sha256: String,
    #[serde(deserialize_with = "required_pending")]
    pending: Option<Pending>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    operator_recovery_used: bool,
}
fn required_pending<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Pending>, D::Error> {
    Option::<Pending>::deserialize(deserializer)
}
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Pending {
    command: String,
}
pub(crate) struct Store {
    path: PathBuf,
    directory: File,
    name: CString,
    lock_name: CString,
    lock: File,
    record: Record,
    failed: bool,
}
fn error() -> String {
    "motion_persistence_failed".into()
}
fn profile_hash(profile: &CommandProfile) -> Result<String, String> {
    let encoded = serde_json::to_vec(profile).map_err(|_| error())?;
    Ok(ring::digest::digest(&ring::digest::SHA256, &encoded)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}
fn validate_record(record: &Record, profile: &CommandProfile) -> Result<(), String> {
    if record.schema != 1
        || record.profile_sha256 != profile_hash(profile)?
        || record
            .pending
            .as_ref()
            .is_some_and(|pending| !crate::control::eligible(profile, &pending.command))
    {
        return Err(error());
    }
    Ok(())
}
/// Read-only deployment preflight. Never creates the store's lock or record.
pub(crate) fn validate_existing(path: &Path, profile: &CommandProfile) -> Result<(), String> {
    let parent = path.parent().ok_or_else(error)?;
    let meta = std::fs::symlink_metadata(parent).map_err(|_| error())?;
    let file = std::fs::symlink_metadata(path).map_err(|_| error())?;
    if meta.mode() & 0o7777 != 0o700 || file.mode() & 0o7777 != 0o600 {
        return Err(error());
    }
    let bytes = crate::runtime::private_read(path)?;
    if bytes.len() as u64 > MAX_RECORD {
        return Err(error());
    }
    let record: Record = serde_json::from_slice(&bytes).map_err(|_| error())?;
    validate_record(&record, profile)
}
fn private_file(file: &File) -> Result<(), String> {
    let meta = file.metadata().map_err(|_| error())?;
    if !meta.is_file()
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.mode() & 0o7777 != 0o600
        || meta.nlink() != 1
    {
        return Err(error());
    }
    Ok(())
}
fn open_at(directory: &File, name: &CString, flags: i32) -> Result<File, String> {
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            0o600,
        )
    };
    if fd < 0 {
        return Err(error());
    }
    let file = unsafe { File::from_raw_fd(fd) };
    private_file(&file)?;
    Ok(file)
}
impl Store {
    pub(crate) fn backup_pending(&self, id: &str) -> Result<(), String> {
        self.validate_paths()?;
        if self.failed || self.read()? != self.record || self.record.pending.is_none() {
            return Err(error());
        }
        crate::runtime::backup_pending(&self.path, id)
    }
    fn locked(path: &Path, profile: &CommandProfile) -> Result<Self, String> {
        let parent = path.parent().ok_or_else(error)?;
        let name = path.file_name().ok_or_else(error)?;
        if !path.is_absolute()
            || path.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::ParentDir | std::path::Component::CurDir
                )
            })
            || std::fs::canonicalize(parent).map_err(|_| error())? != parent
        {
            return Err(error());
        }
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC)
            .open(parent)
            .map_err(|_| error())?;
        let meta = directory.metadata().map_err(|_| error())?;
        if !meta.is_dir()
            || meta.uid() != unsafe { libc::geteuid() }
            || meta.mode() & 0o7777 != 0o700
        {
            return Err(error());
        }
        let name = CString::new(name.as_encoded_bytes()).map_err(|_| error())?;
        let mut lock_bytes = name.as_bytes().to_vec();
        lock_bytes.extend_from_slice(b".lock");
        let lock_name = CString::new(lock_bytes).map_err(|_| error())?;
        let lock = open_at(&directory, &lock_name, libc::O_RDWR | libc::O_CREAT)?;
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err("motion_persistence_already_owned".into());
        }
        let store = Self {
            path: path.to_path_buf(),
            directory,
            name,
            lock_name,
            lock,
            record: Record {
                schema: 1,
                profile_sha256: profile_hash(profile)?,
                pending: None,
                operator_recovery_used: false,
            },
            failed: false,
        };
        store.validate_paths()?;
        Ok(store)
    }
    fn validate_paths(&self) -> Result<(), String> {
        private_file(&self.lock)?;
        let parent = self.path.parent().ok_or_else(error)?;
        let visible = std::fs::symlink_metadata(parent).map_err(|_| error())?;
        let held = self.directory.metadata().map_err(|_| error())?;
        if !visible.is_dir()
            || visible.uid() != unsafe { libc::geteuid() }
            || visible.mode() & 0o7777 != 0o700
            || (visible.dev(), visible.ino()) != (held.dev(), held.ino())
            || std::fs::canonicalize(parent).map_err(|_| error())? != parent
        {
            return Err(error());
        }
        let visible = open_at(&self.directory, &self.lock_name, libc::O_RDONLY)?
            .metadata()
            .map_err(|_| error())?;
        let held = self.lock.metadata().map_err(|_| error())?;
        if (visible.dev(), visible.ino()) != (held.dev(), held.ino()) {
            return Err(error());
        }
        Ok(())
    }
    fn read(&self) -> Result<Record, String> {
        let file = open_at(&self.directory, &self.name, libc::O_RDONLY)?;
        let before = file.metadata().map_err(|_| error())?;
        if before.len() > MAX_RECORD {
            return Err(error());
        }
        let mut bytes = Vec::new();
        file.take(MAX_RECORD + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| error())?;
        if bytes.len() as u64 != before.len() || bytes.len() as u64 > MAX_RECORD {
            return Err(error());
        }
        serde_json::from_slice(&bytes).map_err(|_| error())
    }
    pub(crate) fn open(path: &Path, profile: &CommandProfile) -> Result<Self, String> {
        let mut store = Self::locked(path, profile)?;
        let record = store.read()?;
        validate_record(&record, profile)?;
        store.record = record;
        Ok(store)
    }
    pub(crate) fn initialize(path: &Path, profile: &CommandProfile) -> Result<(), String> {
        let store = Self::locked(path, profile)?;
        // O_EXCL is deliberate: initialization is never an implicit reset.
        let mut file = open_at(
            &store.directory,
            &store.name,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
        )?;
        let bytes = serde_json::to_vec(&store.record).map_err(|_| error())?;
        file.write_all(&bytes).map_err(|_| error())?;
        file.sync_all().map_err(|_| error())?;
        store.directory.sync_all().map_err(|_| error())?;
        if store.read()? != store.record {
            return Err(error());
        }
        Ok(())
    }
    pub(crate) fn pending(&self) -> Option<&str> {
        self.record
            .pending
            .as_ref()
            .map(|pending| pending.command.as_str())
    }
    pub(crate) fn healthy(&self) -> bool {
        !self.failed
    }
    pub(crate) fn operator_recovery_available(&self) -> bool {
        self.healthy() && !self.record.operator_recovery_used && self.record.pending.is_none()
    }
    pub(crate) fn consume_operator_recovery(&mut self) -> Result<(), String> {
        if !self.operator_recovery_available() {
            return Err("operator_recovery_unavailable".into());
        }
        let mut next = self.record.clone();
        next.operator_recovery_used = true;
        self.persist(next)
    }
    pub(crate) fn set_pending(&mut self, command: Option<&str>) -> Result<(), String> {
        let mut next = self.record.clone();
        next.pending = command.map(|command| Pending {
            command: command.into(),
        });
        self.persist(next)
    }
    fn persist(&mut self, next: Record) -> Result<(), String> {
        if self.failed {
            return Err(error());
        }
        let result = self.replace(next);
        if result.is_err() {
            // An error after rename can leave either durable version. Never
            // assume a failed update made no change or resume this process.
            self.failed = true;
        }
        result
    }
    fn replace(&mut self, next: Record) -> Result<(), String> {
        self.validate_paths()?;
        if self.read()? != self.record {
            return Err(error());
        }
        let bytes = serde_json::to_vec(&next).map_err(|_| error())?;
        if bytes.len() as u64 > MAX_RECORD {
            return Err(error());
        }
        let mut nonce = [0u8; 16];
        ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut nonce)
            .map_err(|_| error())?;
        let suffix: String = nonce.iter().map(|byte| format!("{byte:02x}")).collect();
        let temp_name = CString::new(format!(".narwal-intent-{suffix}.tmp")).unwrap();
        let mut created = false;
        let result = (|| {
            let mut temp = open_at(
                &self.directory,
                &temp_name,
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
            )?;
            created = true;
            temp.write_all(&bytes).map_err(|_| error())?;
            temp.sync_all().map_err(|_| error())?;
            self.validate_paths()?;
            if self.read()? != self.record {
                return Err(error());
            }
            if unsafe {
                libc::renameat(
                    self.directory.as_raw_fd(),
                    temp_name.as_ptr(),
                    self.directory.as_raw_fd(),
                    self.name.as_ptr(),
                )
            } != 0
            {
                return Err(error());
            }
            self.directory.sync_all().map_err(|_| error())?;
            if self.read()? != next {
                return Err(error());
            }
            self.record = next;
            Ok(())
        })();
        // A crash may leave a private orphan temp file, which is never loaded.
        if created {
            unsafe {
                libc::unlinkat(self.directory.as_raw_fd(), temp_name.as_ptr(), 0);
            }
        }
        result
    }
}
