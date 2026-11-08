use serde::Deserialize;
use std::{
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
};

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


