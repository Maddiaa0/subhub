//! Secret storage: the macOS Keychain or a 0600 credential-store file on
//! Linux. Nothing outside this module touches raw vault payloads.

use crate::provider::Provider;
use crate::{Error, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(target_os = "linux")]
use std::collections::BTreeMap;
use std::env;
use std::fs;
#[cfg(target_os = "linux")]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
#[cfg(target_os = "linux")]
use std::path::PathBuf;
#[cfg(target_os = "macos")]
use std::process::Command;

pub(crate) const ACTIVE_SERVICE: &str = "Claude Code-credentials";
pub(crate) const VAULT_SERVICE: &str = "subhub-credentials";
const LEGACY_VAULT_SERVICE: &str = "sub-manager-credentials";

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct VaultEntry {
    #[serde(default = "claude_provider")]
    pub(crate) provider: Provider,
    pub(crate) credential: Value,
    #[serde(rename = "oauthAccount")]
    pub(crate) oauth_account: Value,
    #[serde(
        default,
        rename = "refreshError",
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) refresh_error: Option<String>,
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

/// Exclusive lease for the refresh-token store. Every vault mutation takes
/// this lease, while a refresh keeps it from the final pre-request read until
/// the rotated token has been persisted. This makes the gateway the sole
/// writer even if two Subhub processes are accidentally started.
pub(crate) struct RefreshOwnerLease {
    _file: fs::File,
}

pub(crate) fn acquire_refresh_owner() -> Result<RefreshOwnerLease> {
    let file = open_lock_file("refresh-owner.lock")?;
    file.lock_exclusive()
        .map_err(|error| Error::Message(format!("could not lock OAuth token store: {error}")))?;
    Ok(RefreshOwnerLease { _file: file })
}

/// Read while [`RefreshOwnerLease`] is already held.
pub(crate) fn vault_read_owned(name: &str, _lease: &RefreshOwnerLease) -> Result<String> {
    match credential_read(VAULT_SERVICE, name) {
        Ok(stored) => Ok(stored),
        Err(current_error) => match credential_read(LEGACY_VAULT_SERVICE, name) {
            Ok(stored) => {
                credential_write_owned(VAULT_SERVICE, name, &stored, _lease)?;
                Ok(stored)
            }
            Err(_) => Err(current_error),
        },
    }
}

fn open_lock_file(name: &str) -> Result<fs::File> {
    let directory = crate::config_base_path()?.join(if cfg!(target_os = "macos") {
        ".subhub"
    } else {
        "subhub"
    });
    open_lock_file_in(&directory, name)
}

fn open_lock_file_in(directory: &Path, name: &str) -> Result<fs::File> {
    fs::create_dir_all(directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
    }
    let path = directory.join(name);
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options
        .open(&path)
        .map_err(|error| Error::Message(format!("could not open {}: {error}", path.display())))
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
pub(crate) fn active_credential_read() -> Result<Option<String>> {
    let account = current_user()?;
    let output = Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            ACTIVE_SERVICE,
            "-a",
            &account,
            "-w",
        ])
        .output()
        .map_err(|error| Error::Message(format!("could not run `security`: {error}")))?;
    if output.status.success() {
        return String::from_utf8(output.stdout)
            .map(|value| Some(value.trim_end_matches(['\r', '\n']).to_owned()))
            .map_err(|_| Error::Message("Keychain returned a non-UTF-8 credential".into()));
    }
    if output.status.code() == Some(44) {
        return Ok(None);
    }
    let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    Err(Error::Message(if detail.is_empty() {
        "could not inspect Claude Code credential in Keychain".into()
    } else {
        detail
    }))
}

/// Remove only Claude's OAuth object from the active Claude Code credential,
/// preserving any unrelated secrets that share the same JSON container.
pub(crate) fn clear_active_claude_oauth() -> Result<bool> {
    let Some(raw) = active_credential_read()? else {
        return Ok(false);
    };
    let mut credential: Value = serde_json::from_str(&raw).map_err(|error| {
        Error::Message(format!(
            "Claude Code's active credential is not valid JSON: {error}"
        ))
    })?;
    if !remove_claude_oauth(&mut credential)? {
        return Ok(false);
    }
    let account = current_user()?;
    if credential
        .as_object()
        .is_some_and(serde_json::Map::is_empty)
    {
        credential_delete(ACTIVE_SERVICE, &account)?;
    } else {
        credential_write(
            ACTIVE_SERVICE,
            &account,
            &serde_json::to_string(&credential)?,
        )?;
    }
    Ok(true)
}

fn remove_claude_oauth(credential: &mut Value) -> Result<bool> {
    credential
        .as_object_mut()
        .ok_or_else(|| {
            Error::Message("Claude Code's active credential is not a JSON object".into())
        })
        .map(|object| object.remove("claudeAiOauth").is_some())
}

#[cfg(target_os = "macos")]
pub(crate) fn credential_write(service: &str, account: &str, credential: &str) -> Result<()> {
    let lease = (service == VAULT_SERVICE)
        .then(acquire_refresh_owner)
        .transpose()?;
    credential_write_owned_maybe(service, account, credential, lease.as_ref())
}

#[cfg(target_os = "macos")]
fn credential_write_owned_maybe(
    service: &str,
    account: &str,
    credential: &str,
    _lease: Option<&RefreshOwnerLease>,
) -> Result<()> {
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
pub(crate) fn credential_write_owned(
    service: &str,
    account: &str,
    credential: &str,
    lease: &RefreshOwnerLease,
) -> Result<()> {
    credential_write_owned_maybe(service, account, credential, Some(lease))
}

#[cfg(target_os = "macos")]
pub(crate) fn credential_delete(service: &str, account: &str) -> Result<()> {
    let _lease = (service == VAULT_SERVICE)
        .then(acquire_refresh_owner)
        .transpose()?;
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
pub(crate) fn active_credential_read() -> Result<Option<String>> {
    let path = claude_credentials_path()?;
    if !path.exists() {
        return Ok(None);
    }
    fs::read_to_string(&path)
        .map(Some)
        .map_err(|error| Error::Message(format!("could not read {}: {error}", path.display())))
}

#[cfg(target_os = "linux")]
pub(crate) fn credential_write(service: &str, account: &str, credential: &str) -> Result<()> {
    let lease = (service == VAULT_SERVICE)
        .then(acquire_refresh_owner)
        .transpose()?;
    credential_write_owned_maybe(service, account, credential, lease.as_ref())
}

#[cfg(target_os = "linux")]
fn credential_write_owned_maybe(
    service: &str,
    account: &str,
    credential: &str,
    _lease: Option<&RefreshOwnerLease>,
) -> Result<()> {
    if service == ACTIVE_SERVICE {
        return write_private_bytes(&claude_credentials_path()?, credential.as_bytes());
    }
    let store_lock = open_lock_file("credential-store.lock")?;
    store_lock.lock_exclusive().map_err(|error| {
        Error::Message(format!(
            "could not lock credential store for writing: {error}"
        ))
    })?;
    let path = credential_store_path()?;
    let mut store = read_credential_store(&path)?;
    store
        .entry(service.to_owned())
        .or_default()
        .insert(account.to_owned(), credential.to_owned());
    write_credential_store(&path, &store)
}

#[cfg(target_os = "linux")]
pub(crate) fn credential_write_owned(
    service: &str,
    account: &str,
    credential: &str,
    lease: &RefreshOwnerLease,
) -> Result<()> {
    credential_write_owned_maybe(service, account, credential, Some(lease))
}

#[cfg(target_os = "linux")]
pub(crate) fn credential_delete(service: &str, account: &str) -> Result<()> {
    if service == ACTIVE_SERVICE {
        let path = claude_credentials_path()?;
        return if path.exists() {
            fs::remove_file(&path).map_err(Into::into)
        } else {
            Err(Error::Message("credential was not found".into()))
        };
    }
    let _lease = (service == VAULT_SERVICE)
        .then(acquire_refresh_owner)
        .transpose()?;
    let store_lock = open_lock_file("credential-store.lock")?;
    store_lock.lock_exclusive().map_err(|error| {
        Error::Message(format!(
            "could not lock credential store for writing: {error}"
        ))
    })?;
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
            refresh_error: None,
        };
        let stored = serde_json::to_string(&entry).unwrap();
        let (credential, account) = decode_vault_entry(&stored).unwrap();
        assert!(validate_credential(&credential).is_ok());
        assert_eq!(account["emailAddress"], "person@example.com");
    }

    #[test]
    fn retiring_claude_oauth_preserves_unrelated_active_secrets() {
        let mut active = serde_json::json!({
            "claudeAiOauth": {"accessToken": "a", "refreshToken": "r"},
            "mcpOauth": {"example": "keep-me"}
        });
        assert!(remove_claude_oauth(&mut active).unwrap());
        assert!(active.get("claudeAiOauth").is_none());
        assert_eq!(active["mcpOauth"]["example"], "keep-me");
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

    #[cfg(target_os = "linux")]
    #[test]
    fn refresh_owner_lock_is_exclusive_across_file_handles() {
        use std::time::{SystemTime, UNIX_EPOCH};
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = env::temp_dir().join(format!(
            "subhub-owner-lock-test-{}-{unique}",
            std::process::id()
        ));
        let first = open_lock_file_in(&directory, "owner.lock").unwrap();
        let second = open_lock_file_in(&directory, "owner.lock").unwrap();

        first.lock_exclusive().unwrap();
        assert!(second.try_lock_exclusive().is_err());
        drop(first);
        second.try_lock_exclusive().unwrap();

        drop(second);
        fs::remove_dir_all(directory).unwrap();
    }
}
