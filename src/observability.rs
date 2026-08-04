//! Structured gateway event log: append-only JSONL under the config
//! directory, read back by `subhub gateway logs`.

use serde_json::Value;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;

pub(crate) fn log_path() -> crate::Result<PathBuf> {
    Ok(crate::config_base_path()?
        .join("subhub")
        .join("gateway.jsonl"))
}

pub(crate) fn event(name: &str, fields: Value) {
    let Ok(path) = log_path() else { return };
    let Some(parent) = path.parent() else { return };
    if fs::create_dir_all(parent).is_err() {
        return;
    }
    let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
    let record = serde_json::json!({
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "event": name,
        "fields": fields
    });
    let mut options = fs::OpenOptions::new();
    options.create(true).append(true).mode(0o600);
    if let Ok(mut file) = options.open(path) {
        let _ = writeln!(file, "{record}");
    }
}

pub(crate) fn tail(lines: usize) -> crate::Result<Vec<String>> {
    let path = log_path()?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let contents = fs::read_to_string(path)?;
    let records: Vec<_> = contents.lines().collect();
    Ok(records[records.len().saturating_sub(lines)..]
        .iter()
        .map(|line| (*line).to_owned())
        .collect())
}
