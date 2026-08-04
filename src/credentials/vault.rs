use crate::provider::Provider;
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(target_os = "linux")]
use std::collections::BTreeMap;
use std::env;
#[cfg(target_os = "linux")]
use std::fs;
#[cfg(target_os = "linux")]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
#[cfg(target_os = "linux")]
use std::path::Path;
#[cfg(target_os = "linux")]
use std::path::PathBuf;
#[cfg(target_os = "macos")]
use std::process::Command;

pub(crate) const ACTIVE_SERVICE: &str = "Claude Code-credentials";
pub(crate) const VAULT_SERVICE: &str = "subhub-credentials";
const LEGACY_VAULT_SERVICE: &str = "sub-manager-credentials";

#[derive(Deserialize, Serialize)]
pub(crate) struct VaultEntry {
    #[serde(default = "claude_provider")]
    pub(crate) provider: Provider,
    pub(crate) credential: Value,
    #[serde(rename = "oauthAccount")]
    pub(crate) oauth_account: Value,
}

fn claude_provider() -> Provider {
    Provider::Claude
}

pub(crate) fn validate_credential(raw: &str) -> Result<()> {
    let parsed: Value = serde_json::from_str(raw)
        .map_err(|_| Error::Message("credential is not valid JSON".into()))?;
    let oauth = parsed
        .get("claudeAiOauth")
        .and_then(Value::as_object)
        .ok_or_else(|| Error::Message("credential has no claudeAiOauth object".into()))?;
    for key in ["accessToken", "refreshToken"] {
        if oauth.get(key).and_then(Value::as_str).is_none() {
            return Err(Error::Message(format!(
                "credential has no valid claudeAiOauth.{key}"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn decode_vault_entry(stored: &str) -> Result<(String, Value)> {
    let parsed: Value = serde_json::from_str(stored)
        .map_err(|_| Error::Message("saved Keychain entry is not valid JSON".into()))?;
    let entry: VaultEntry = serde_json::from_value(parsed)
        .map_err(|_| Error::Message("saved credential predates account metadata support".into()))?;
    Ok((
        serde_json::to_string(&entry.credential)?,
        entry.oauth_account,
    ))
}

pub(crate) fn vault_read(name: &str) -> Result<String> {
    match credential_read(VAULT_SERVICE, name) {
        Ok(stored) => Ok(stored),
        Err(current_error) => match credential_read(LEGACY_VAULT_SERVICE, name) {
            Ok(stored) => {
                credential_write(VAULT_SERVICE, name, &stored)?;
                Ok(stored)
            }
            Err(_) => Err(current_error),
        },
    }
}

pub(crate) fn current_user() -> Result<String> {
    env::var("USER")
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::Message("USER is not set".into()))
}

#[cfg(target_os = "macos")]
pub(crate) fn credential_read(service: &str, account: &str) -> Result<String> {
    let output = Command::new("security")
        .args(["find-generic-password", "-s", service, "-a", account, "-w"])
        .output()
        .map_err(|error| Error::Message(format!("could not run `security`: {error}")))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(Error::Message(if detail.is_empty() {
            "Keychain item was not found".into()
        } else {
            detail
        }));
    }
    String::from_utf8(output.stdout)
        .map(|value| value.trim_end_matches(['\r', '\n']).to_owned())
        .map_err(|_| Error::Message("Keychain returned a non-UTF-8 credential".into()))
}

#[cfg(target_os = "macos")]
pub(crate) fn credential_write(service: &str, account: &str, credential: &str) -> Result<()> {
    let output = Command::new("security")
        .args([
            "add-generic-password",
            "-s",
            service,
            "-a",
            account,
            "-w",
            credential,
            "-U",
        ])
        .output()
        .map_err(|error| Error::Message(format!("could not run `security`: {error}")))?;
    if output.status.success() {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    Err(Error::Message(format!(
        "could not update Keychain: {detail}"
    )))
}

#[cfg(target_os = "macos")]
pub(crate) fn credential_delete(service: &str, account: &str) -> Result<()> {
    let output = Command::new("security")
        .args(["delete-generic-password", "-s", service, "-a", account])
        .output()
        .map_err(|error| Error::Message(format!("could not run `security`: {error}")))?;
    if output.status.success() {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    Err(Error::Message(if detail.is_empty() {
        "Keychain item was not found".into()
    } else {
        detail
    }))
}

#[cfg(target_os = "linux")]
pub(crate) fn credential_read(service: &str, account: &str) -> Result<String> {
    if service == ACTIVE_SERVICE {
        return fs::read_to_string(claude_credentials_path()?).map_err(|error| {
            Error::Message(format!(
                "Claude Code credential file was not readable: {error}"
            ))
        });
    }
    let path = credential_store_path()?;
    let store = read_credential_store(&path)?;
    store
        .get(service)
        .and_then(|accounts| accounts.get(account))
        .cloned()
        .ok_or_else(|| Error::Message("credential was not found".into()))
}

#[cfg(target_os = "linux")]
pub(crate) fn credential_write(service: &str, account: &str, credential: &str) -> Result<()> {
    if service == ACTIVE_SERVICE {
        return write_private_bytes(&claude_credentials_path()?, credential.as_bytes());
    }
    let path = credential_store_path()?;
    let mut store = read_credential_store(&path)?;
    store
        .entry(service.to_owned())
        .or_default()
        .insert(account.to_owned(), credential.to_owned());
    write_credential_store(&path, &store)
}

#[cfg(target_os = "linux")]
pub(crate) fn credential_delete(service: &str, account: &str) -> Result<()> {
    let path = credential_store_path()?;
    let mut store = read_credential_store(&path)?;
    let removed = store
        .get_mut(service)
        .and_then(|accounts| accounts.remove(account));
    if removed.is_none() {
        return Err(Error::Message("credential was not found".into()));
    }
    if store.get(service).is_some_and(BTreeMap::is_empty) {
        store.remove(service);
    }
    write_credential_store(&path, &store)
}

#[cfg(target_os = "linux")]
type CredentialStore = BTreeMap<String, BTreeMap<String, String>>;

#[cfg(target_os = "linux")]
fn read_credential_store(path: &Path) -> Result<CredentialStore> {
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let raw = fs::read_to_string(path)
        .map_err(|error| Error::Message(format!("could not read {}: {error}", path.display())))?;
    serde_json::from_str(&raw).map_err(|error| {
        Error::Message(format!(
            "invalid credential store {}: {error}",
            path.display()
        ))
    })
}

#[cfg(target_os = "linux")]
fn write_credential_store(path: &Path, store: &CredentialStore) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(store)?;
    write_private_bytes(path, &bytes)
}

#[cfg(target_os = "linux")]
fn write_private_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::Message("credential path has no parent directory".into()))?;
    fs::create_dir_all(parent)?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    let temporary = path.with_extension("json.tmp");
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true).mode(0o600);
    let mut file = options.open(&temporary)?;
    std::io::Write::write_all(&mut file, bytes)?;
    file.sync_all()?;
    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
    fs::rename(temporary, path)?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn credential_store_path() -> Result<PathBuf> {
    Ok(crate::paths::config_base_path()?
        .join("subhub")
        .join("credentials.json"))
}

#[cfg(target_os = "linux")]
fn claude_credentials_path() -> Result<PathBuf> {
    env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude")))
        .map(|directory| directory.join(".credentials.json"))
        .ok_or_else(|| Error::Message("CLAUDE_CONFIG_DIR and HOME are not set".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_expected_claude_credential() {
        let raw = r#"{"claudeAiOauth":{"accessToken":"a","refreshToken":"r"}}"#;
        assert!(validate_credential(raw).is_ok());
        assert!(validate_credential("{}").is_err());
    }

    #[test]
    fn vault_entry_round_trips_credential_and_account() {
        let entry = VaultEntry {
            provider: Provider::Claude,
            credential: serde_json::json!({
                "claudeAiOauth": {"accessToken": "a", "refreshToken": "r"}
            }),
            oauth_account: serde_json::json!({"emailAddress": "person@example.com"}),
        };
        let stored = serde_json::to_string(&entry).unwrap();
        let (credential, account) = decode_vault_entry(&stored).unwrap();
        assert!(validate_credential(&credential).is_ok());
        assert_eq!(account["emailAddress"], "person@example.com");
    }

    #[test]
    fn legacy_vault_entry_requires_refresh() {
        let raw = r#"{"claudeAiOauth":{"accessToken":"a","refreshToken":"r"}}"#;
        assert!(decode_vault_entry(raw).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_credential_store_is_private_and_round_trips() {
        use std::time::{SystemTime, UNIX_EPOCH};
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = env::temp_dir().join(format!(
            "subhub-credential-test-{}-{unique}",
            std::process::id()
        ));
        let path = directory.join("subhub").join("credentials.json");
        let mut store = CredentialStore::new();
        store
            .entry(VAULT_SERVICE.into())
            .or_default()
            .insert("personal".into(), "secret".into());

        write_credential_store(&path, &store).unwrap();

        assert_eq!(read_credential_store(&path).unwrap(), store);
        assert_eq!(
            fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_dir_all(directory).unwrap();
    }
}
