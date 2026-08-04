//! Configuration file locations and atomic, owner-only JSON writes.

use crate::{Error, Result};
use serde::Serialize;
use std::env;
use std::fs;
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

pub(crate) fn config_base_path() -> Result<PathBuf> {
    let path = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")));
    #[cfg(target_os = "macos")]
    let path = env::var_os("XDG_CONFIG").map(PathBuf::from).or(path);
    path.ok_or_else(|| Error::Message("XDG_CONFIG_HOME and HOME are not set".into()))
}

pub(crate) fn claude_config_path() -> Result<PathBuf> {
    if let Some(directory) = env::var_os("CLAUDE_CONFIG_DIR") {
        return Ok(PathBuf::from(directory).join(".claude.json"));
    }
    env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".claude.json"))
        .ok_or_else(|| Error::Message("HOME is not set".into()))
}

/// Atomically write `value` as pretty JSON with owner-only permissions.
pub(crate) fn save_json_file(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::Message("JSON path has no parent directory".into()))?;
    fs::create_dir_all(parent)?;
    let temporary = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(value)?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true).mode(0o600);
    let mut file = options.open(&temporary)?;
    io::Write::write_all(&mut file, &bytes)?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    Ok(())
}
