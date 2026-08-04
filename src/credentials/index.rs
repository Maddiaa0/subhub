//! The credential name index: which names exist and which is active per
//! provider. Never stores secrets (test-enforced).

use crate::paths::{config_base_path, save_json_file};
use crate::provider::Provider;
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Deserialize, Serialize)]
pub(crate) struct Index {
    version: u8,
    /// Most recently activated credential, kept for older subhub binaries.
    active: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    active_claude: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    active_codex: Option<String>,
    pub(crate) credentials: Vec<String>,
}

impl Index {
    pub(crate) fn new() -> Self {
        Self {
            version: 1,
            active: None,
            active_claude: None,
            active_codex: None,
            credentials: Vec::new(),
        }
    }

    pub(crate) fn contains(&self, name: &str) -> bool {
        self.credentials.iter().any(|item| item == name)
    }

    pub(crate) fn add(&mut self, name: &str, provider: Provider) {
        if !self.contains(name) {
            self.credentials.push(name.to_owned());
            self.credentials.sort();
        }
        self.activate(name, provider);
    }

    pub(crate) fn activate(&mut self, name: &str, provider: Provider) {
        self.active = Some(name.to_owned());
        *match provider {
            Provider::Claude => &mut self.active_claude,
            Provider::Codex => &mut self.active_codex,
        } = Some(name.to_owned());
    }

    /// Active credential for a provider, falling back to the legacy single
    /// slot for index files written before per-provider tracking.
    pub(crate) fn active_for(&self, provider: Provider) -> Option<&str> {
        match provider {
            Provider::Claude => &self.active_claude,
            Provider::Codex => &self.active_codex,
        }
        .as_deref()
        .or(self.active.as_deref())
    }

    pub(crate) fn active_names(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for name in [&self.active_claude, &self.active_codex, &self.active]
            .into_iter()
            .flatten()
        {
            if !names.contains(name) {
                names.push(name.clone());
            }
        }
        names
    }
}

pub(crate) fn validate_name(name: &str) -> Result<()> {
    if name.trim().is_empty() {
        return Err(Error::Message("credential name cannot be empty".into()));
    }
    if name.chars().any(char::is_control) {
        return Err(Error::Message(
            "credential name cannot contain control characters".into(),
        ));
    }
    Ok(())
}

pub(crate) fn index_path() -> Result<PathBuf> {
    #[cfg(target_os = "macos")]
    let directory = ".subhub";
    #[cfg(target_os = "linux")]
    let directory = "subhub";
    Ok(config_base_path()?.join(directory).join("index.json"))
}

pub(crate) fn legacy_index_path() -> Result<PathBuf> {
    Ok(config_base_path()?.join(".sub-manager").join("index.json"))
}

pub(crate) fn load_or_migrate_index(path: &Path, legacy_path: &Path) -> Result<Index> {
    if path.exists() || !legacy_path.exists() {
        return load_index(path);
    }
    let index = load_index(legacy_path)?;
    save_index(path, &index)?;
    Ok(index)
}

pub(crate) fn load_index(path: &Path) -> Result<Index> {
    if !path.exists() {
        return Ok(Index::new());
    }
    let contents = fs::read_to_string(path)
        .map_err(|error| Error::Message(format!("could not read {}: {error}", path.display())))?;
    let index: Index = serde_json::from_str(&contents)
        .map_err(|error| Error::Message(format!("invalid index {}: {error}", path.display())))?;
    if index.version != 1 {
        return Err(Error::Message(format!(
            "unsupported index version {} in {}",
            index.version,
            path.display()
        )));
    }
    Ok(index)
}

pub(crate) fn save_index(path: &Path, index: &Index) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::Message("index path has no parent directory".into()))?;
    fs::create_dir_all(parent)?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;

    save_json_file(path, index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::env;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn adding_names_is_sorted_and_marks_active() {
        let mut index = Index::new();
        index.add("work", Provider::Codex);
        index.add("personal", Provider::Claude);
        assert_eq!(index.credentials, ["personal", "work"]);
        assert_eq!(index.active.as_deref(), Some("personal"));
        assert_eq!(index.active_for(Provider::Claude), Some("personal"));
        assert_eq!(index.active_for(Provider::Codex), Some("work"));
        assert_eq!(index.active_names(), ["personal", "work"]);
    }

    #[test]
    fn legacy_index_active_slot_backs_both_providers() {
        let index: Index = serde_json::from_value(serde_json::json!({
            "version": 1,
            "active": "personal",
            "credentials": ["personal"]
        }))
        .unwrap();
        assert_eq!(index.active_for(Provider::Claude), Some("personal"));
        assert_eq!(index.active_for(Provider::Codex), Some("personal"));
        assert_eq!(index.active_names(), ["personal"]);
    }

    #[test]
    fn index_write_never_stores_sensitive_credential_data() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory =
            env::temp_dir().join(format!("subhub-index-test-{}-{unique}", std::process::id()));
        let path = directory.join("index.json");

        let mut index = Index::new();
        index.add("personal", Provider::Claude);
        save_index(&path, &index).unwrap();

        let written = fs::read_to_string(&path).unwrap();
        let parsed: Value = serde_json::from_str(&written).unwrap();
        assert_eq!(
            parsed,
            serde_json::json!({
                "version": 1,
                "active": "personal",
                "active_claude": "personal",
                "credentials": ["personal"]
            })
        );
        for sensitive_field in [
            "accessToken",
            "refreshToken",
            "claudeAiOauth",
            "oauthAccount",
            "emailAddress",
        ] {
            assert!(
                !written.contains(sensitive_field),
                "index leaked sensitive field {sensitive_field}"
            );
        }

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn legacy_index_is_copied_to_subhub_location() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = env::temp_dir().join(format!(
            "subhub-migration-test-{}-{unique}",
            std::process::id()
        ));
        let legacy_path = directory.join("legacy").join("index.json");
        let subhub_path = directory.join("subhub").join("index.json");
        let mut legacy = Index::new();
        legacy.add("personal", Provider::Claude);
        save_index(&legacy_path, &legacy).unwrap();

        let migrated = load_or_migrate_index(&subhub_path, &legacy_path).unwrap();

        assert_eq!(migrated.credentials, ["personal"]);
        assert_eq!(migrated.active.as_deref(), Some("personal"));
        assert!(subhub_path.exists());
        fs::remove_dir_all(directory).unwrap();
    }
}
