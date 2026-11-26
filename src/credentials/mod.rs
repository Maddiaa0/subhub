//! Saved credentials: OS-vault storage ([`vault`]), the name index
//! ([`index`]), the Claude OAuth refresh protocol ([`oauth`]), and loading
//! [`StoredCredential`]s for the gateway.

pub(crate) mod index;
pub(crate) mod oauth;
pub(crate) mod vault;

use crate::antigravity;
use crate::provider::{Provider, StoredCredential};
use crate::{Error, Result};
use index::{Index, index_path, legacy_index_path, load_or_migrate_index};
use serde_json::Value;
use std::path::Path;
use vault::{
    ACTIVE_SERVICE, VAULT_SERVICE, VaultEntry, active_credential_read, clear_active_claude_oauth,
    credential_write, current_user, vault_read,
};

/// Fresh vault snapshot for the gateway's `/_subhub/reload` endpoint.
/// Only includes providers routed through the gateway.
pub(crate) fn gateway_credentials() -> Result<Vec<StoredCredential>> {
    let path = index_path()?;
    let index = load_or_migrate_index(&path, &legacy_index_path()?)?;
    Ok(stored_credentials(&index)?
        .into_iter()
        .filter(|credential| credential.provider.uses_gateway())
        .collect())
}

pub(crate) fn stored_credentials(index: &Index) -> Result<Vec<StoredCredential>> {
    let mut credentials = Vec::with_capacity(index.credentials.len());
    for name in &index.credentials {
        let stored = vault_read(name).map_err(|error| {
            Error::Message(format!(
                "credential \"{name}\" is missing from secure storage: {error}"
            ))
        })?;
        let parsed: Value = serde_json::from_str(&stored)
            .map_err(|_| Error::Message(format!("credential \"{name}\" is not valid JSON")))?;
        let provider: Provider = parsed
            .get("provider")
            .cloned()
            .map(serde_json::from_value)
            .transpose()?
            .unwrap_or_default();
        let credential = parsed
            .get("credential")
            .ok_or_else(|| Error::Message(format!("credential \"{name}\" has no credential")))?;
        let oauth = credential.get("claudeAiOauth").and_then(Value::as_object);
        let access_token = match provider {
            Provider::Claude => oauth
                .and_then(|value| value.get("accessToken"))
                .and_then(Value::as_str),
            Provider::Codex => credential
                .pointer("/tokens/access_token")
                .and_then(Value::as_str),
            Provider::Antigravity => antigravity::access_token(credential),
        }
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::Message(format!("credential \"{name}\" has no access token")))?;
        let scopes = oauth
            .and_then(|value| value.get("scopes"))
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let expires_at = match provider {
            Provider::Claude => oauth
                .and_then(|value| value.get("expiresAt"))
                .and_then(Value::as_i64),
            Provider::Codex => None,
            Provider::Antigravity => antigravity::expires_at_millis(credential),
        };
        let account_id = match provider {
            Provider::Claude => None,
            Provider::Codex => credential
                .pointer("/tokens/account_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
            Provider::Antigravity => antigravity::email(credential).map(str::to_owned),
        };
        credentials.push(StoredCredential {
            name: name.clone(),
            access_token: access_token.to_owned(),
            expires_at,
            scopes,
            provider,
            account_id,
            refresh_error: parsed
                .get("refreshError")
                .and_then(Value::as_str)
                .map(str::to_owned),
        });
    }
    Ok(credentials)
}

pub(crate) fn stored_credential_from_entry(
    name: &str,
    entry: &VaultEntry,
) -> Result<StoredCredential> {
    let oauth = entry
        .credential
        .get("claudeAiOauth")
        .and_then(Value::as_object);
    let access_token = match entry.provider {
        Provider::Claude => oauth
            .and_then(|value| value.get("accessToken"))
            .and_then(Value::as_str),
        Provider::Codex => entry
            .credential
            .pointer("/tokens/access_token")
            .and_then(Value::as_str),
        Provider::Antigravity => antigravity::access_token(&entry.credential),
    }
    .filter(|value| !value.is_empty())
    .ok_or_else(|| Error::Message(format!("credential \"{name}\" has no access token")))?;
    let scopes = oauth
        .and_then(|value| value.get("scopes"))
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let expires_at = match entry.provider {
        Provider::Claude => oauth
            .and_then(|value| value.get("expiresAt"))
            .and_then(Value::as_i64),
        Provider::Codex => None,
        Provider::Antigravity => antigravity::expires_at_millis(&entry.credential),
    };
    let account_id = match entry.provider {
        Provider::Claude => None,
        Provider::Codex => entry
            .credential
            .pointer("/tokens/account_id")
            .and_then(Value::as_str)
            .map(str::to_owned),
        Provider::Antigravity => antigravity::email(&entry.credential).map(str::to_owned),
    };
    Ok(StoredCredential {
        name: name.to_owned(),
        access_token: access_token.to_owned(),
        expires_at,
        scopes,
        provider: entry.provider,
        account_id,
        refresh_error: entry.refresh_error.clone(),
    })
}

pub(crate) fn ensure_unique_claude_identity(
    index: &Index,
    replacing_name: &str,
    candidate: &VaultEntry,
) -> Result<()> {
    for name in &index.credentials {
        if name == replacing_name {
            continue;
        }
        let stored = vault_read(name).map_err(|error| {
            Error::Message(format!(
                "could not verify Claude account uniqueness against \"{name}\": {error}"
            ))
        })?;
        let existing = serde_json::from_str::<VaultEntry>(&stored).map_err(|error| {
            Error::Message(format!(
                "could not verify Claude account uniqueness against \"{name}\": {error}"
            ))
        })?;
        if existing.provider == Provider::Claude && same_claude_identity(&existing, candidate) {
            return Err(Error::Message(format!(
                "this Claude account is already saved as \"{name}\"; each OAuth token family must have exactly one Subhub owner"
            )));
        }
    }
    Ok(())
}

fn same_claude_identity(left: &VaultEntry, right: &VaultEntry) -> bool {
    if let (Some(left), Some(right)) = (claude_account_uuid(left), claude_account_uuid(right)) {
        return left == right;
    }
    matches!(
        (claude_refresh_token(left), claude_refresh_token(right)),
        (Some(left), Some(right)) if left == right
    )
}

fn claude_account_uuid(entry: &VaultEntry) -> Option<&str> {
    entry
        .oauth_account
        .get("accountUuid")
        .or_else(|| entry.oauth_account.get("account_uuid"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

fn claude_refresh_token(entry: &VaultEntry) -> Option<&str> {
    entry
        .credential
        .pointer("/claudeAiOauth/refreshToken")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

/// Remove Claude Code's independent refresh-token copy after verifying that
/// the same token is safely represented by a canonical Subhub vault entry.
pub(crate) fn retire_active_claude_credential(index: &Index) -> Result<bool> {
    let Some(active) = active_credential_read()? else {
        return Ok(false);
    };
    let active: Value = serde_json::from_str(&active)
        .map_err(|_| Error::Message("Claude Code's active credential is not valid JSON".into()))?;
    let Some(active_oauth) = active.get("claudeAiOauth") else {
        return Ok(false);
    };
    let active_refresh = active_oauth
        .get("refreshToken")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            Error::Message("Claude Code's active credential has no refresh token".into())
        })?;
    let is_saved = index.credentials.iter().any(|name| {
        vault_read(name)
            .ok()
            .and_then(|stored| serde_json::from_str::<VaultEntry>(&stored).ok())
            .is_some_and(|entry| {
                entry.provider == Provider::Claude
                    && entry
                        .credential
                        .pointer("/claudeAiOauth/refreshToken")
                        .and_then(Value::as_str)
                        == Some(active_refresh)
            })
    });
    if !is_saved {
        return Err(Error::Message(
            "Claude Code has an active OAuth credential that is not saved in Subhub; run `subhub add <name> --capture` to import the current login before installing the gateway".into(),
        ));
    }
    clear_active_claude_oauth()
}

/// Hand the selected Claude credential back to Claude Code after the gateway
/// has stopped and no longer owns refreshes.
pub(crate) fn restore_active_claude_credential(index: &Index) -> Result<bool> {
    let Some(name) = index.active_for(Provider::Claude) else {
        return Ok(false);
    };
    let stored = vault_read(name)?;
    let entry: VaultEntry = serde_json::from_str(&stored)?;
    if entry.provider != Provider::Claude {
        return Ok(false);
    }
    let mut active = active_credential_read()?
        .map(|raw| serde_json::from_str::<Value>(&raw))
        .transpose()?
        .unwrap_or_else(|| Value::Object(Default::default()));
    let active = active
        .as_object_mut()
        .ok_or_else(|| Error::Message("Claude Code's active credential is not an object".into()))?;
    let oauth = entry
        .credential
        .get("claudeAiOauth")
        .cloned()
        .ok_or_else(|| Error::Message(format!("credential \"{name}\" has no Claude OAuth data")))?;
    active.insert("claudeAiOauth".into(), oauth);
    credential_write(
        ACTIVE_SERVICE,
        &current_user()?,
        &serde_json::to_string(&active)?,
    )?;
    Ok(true)
}

pub(crate) fn ensure_unique_antigravity_identity(
    index: &Index,
    replacing_name: &str,
    candidate: &VaultEntry,
) -> Result<()> {
    for name in &index.credentials {
        if name == replacing_name {
            continue;
        }
        let stored = vault_read(name).map_err(|error| {
            Error::Message(format!(
                "could not verify Antigravity account uniqueness against \"{name}\": {error}"
            ))
        })?;
        let existing = serde_json::from_str::<VaultEntry>(&stored).map_err(|error| {
            Error::Message(format!(
                "could not verify Antigravity account uniqueness against \"{name}\": {error}"
            ))
        })?;
        if existing.provider == Provider::Antigravity
            && antigravity::same_identity(&existing.credential, &candidate.credential)
        {
            return Err(Error::Message(format!(
                "this Antigravity account is already saved as \"{name}\"; use `subhub add {name} --force` to replace it"
            )));
        }
    }
    Ok(())
}

/// If the active Antigravity token file on disk still belongs to the currently
/// active saved Antigravity account and `agy` refreshed it since it was saved,
/// persist that fresher token back to the vault before switching away.
pub(crate) fn sync_active_antigravity_credential(index: &Index, token_path: &Path) -> Result<bool> {
    let Some(active_name) = index.active_for(Provider::Antigravity) else {
        return Ok(false);
    };
    let Ok(stored) = vault_read(active_name) else {
        return Ok(false);
    };
    let Ok(mut entry) = serde_json::from_str::<VaultEntry>(&stored) else {
        return Ok(false);
    };
    if entry.provider != Provider::Antigravity {
        return Ok(false);
    }
    let Some(raw) = antigravity::read_active_token(token_path)? else {
        return Ok(false);
    };
    let Ok(on_disk) = antigravity::validate_credential(&raw) else {
        return Ok(false);
    };
    if !should_sync_antigravity_token(&entry.credential, &on_disk) {
        return Ok(false);
    }
    entry.credential = on_disk;
    if let Some(email) = antigravity::email(&entry.credential) {
        entry.oauth_account = serde_json::json!({ "emailAddress": email });
    }
    credential_write(VAULT_SERVICE, active_name, &serde_json::to_string(&entry)?)?;
    Ok(true)
}

fn should_sync_antigravity_token(saved: &Value, on_disk: &Value) -> bool {
    antigravity::same_identity(saved, on_disk) && saved != on_disk
}

/// Activate `name` as the on-disk Antigravity CLI token, syncing any refreshed
/// token from the previously active Antigravity account back to the vault first.
pub(crate) fn activate_antigravity_credential(
    index: &Index,
    name: &str,
    entry: &VaultEntry,
    token_path: &Path,
) -> Result<bool> {
    antigravity::validate_credential_value(&entry.credential)?;
    let synced = sync_active_antigravity_credential(index, token_path)?;
    if index.active_for(Provider::Antigravity) == Some(name)
        && let Some(raw) = antigravity::read_active_token(token_path)?
        && let Ok(on_disk) = antigravity::validate_credential(&raw)
        && antigravity::same_identity(&entry.credential, &on_disk)
    {
        return Ok(synced);
    }
    antigravity::write_active_token(token_path, &entry.credential)?;
    Ok(synced)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claude_entry(account_uuid: Option<&str>, refresh_token: &str) -> VaultEntry {
        VaultEntry {
            provider: Provider::Claude,
            credential: serde_json::json!({
                "claudeAiOauth": {
                    "accessToken": "access",
                    "refreshToken": refresh_token,
                    "expiresAt": 42,
                    "scopes": ["user:profile"]
                }
            }),
            oauth_account: account_uuid
                .map_or(Value::Null, |uuid| serde_json::json!({"accountUuid": uuid})),
            refresh_error: None,
        }
    }

    #[test]
    fn claude_identity_prefers_account_uuid_and_falls_back_to_refresh_token() {
        assert!(same_claude_identity(
            &claude_entry(Some("account-a"), "old-token"),
            &claude_entry(Some("account-a"), "new-token")
        ));
        assert!(!same_claude_identity(
            &claude_entry(Some("account-a"), "same-token"),
            &claude_entry(Some("account-b"), "same-token")
        ));
        assert!(same_claude_identity(
            &claude_entry(None, "same-token"),
            &claude_entry(None, "same-token")
        ));
    }

    #[test]
    fn stored_credential_from_antigravity_entry_extracts_token_email_and_expiry() {
        let entry = VaultEntry {
            provider: Provider::Antigravity,
            credential: serde_json::json!({
                "token": {
                    "access_token": "ya29.access",
                    "token_type": "Bearer",
                    "refresh_token": "1//refresh",
                    "expiry": "2030-08-01T13:00:00Z",
                    "expires_in": 3599
                },
                "auth_method": "oauth",
                "id_token_email": "dev@example.com"
            }),
            oauth_account: serde_json::json!({"emailAddress": "dev@example.com"}),
            refresh_error: None,
        };
        let stored = stored_credential_from_entry("work", &entry).unwrap();
        assert_eq!(stored.provider, Provider::Antigravity);
        assert_eq!(stored.access_token, "ya29.access");
        assert_eq!(stored.account_id.as_deref(), Some("dev@example.com"));
        assert!(stored.expires_at.is_some());
    }

    #[test]
    fn antigravity_sync_only_updates_when_identity_matches_and_token_changed() {
        let saved = serde_json::json!({
            "token": {"access_token": "old", "refresh_token": "r1"},
            "id_token_email": "work@example.com"
        });
        let refreshed = serde_json::json!({
            "token": {"access_token": "new", "refresh_token": "r1"},
            "id_token_email": "work@example.com"
        });
        let other = serde_json::json!({
            "token": {"access_token": "other", "refresh_token": "r2"},
            "id_token_email": "personal@example.com"
        });

        assert!(!should_sync_antigravity_token(&saved, &saved));
        assert!(should_sync_antigravity_token(&saved, &refreshed));
        assert!(!should_sync_antigravity_token(&saved, &other));
    }
}
