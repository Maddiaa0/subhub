//! Saved credentials: OS-vault storage ([`vault`]), the name index
//! ([`index`]), the Claude OAuth refresh protocol ([`oauth`]), and loading
//! [`StoredCredential`]s for the gateway.

pub(crate) mod index;
pub(crate) mod oauth;
pub(crate) mod vault;

use crate::provider::{Provider, StoredCredential};
use crate::{Error, Result};
use index::{Index, index_path, legacy_index_path, load_or_migrate_index};
use serde_json::Value;
use vault::{
    ACTIVE_SERVICE, VaultEntry, active_credential_read, clear_active_claude_oauth,
    credential_write, current_user, vault_read,
};

/// Fresh vault snapshot for the gateway's `/_subhub/reload` endpoint.
pub(crate) fn gateway_credentials() -> Result<Vec<StoredCredential>> {
    let path = index_path()?;
    let index = load_or_migrate_index(&path, &legacy_index_path()?)?;
    stored_credentials(&index)
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
            Provider::Claude => oauth.and_then(|value| value.get("accessToken")),
            Provider::Codex => credential.pointer("/tokens/access_token"),
        }
        .and_then(Value::as_str)
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
        credentials.push(StoredCredential {
            name: name.clone(),
            access_token: access_token.to_owned(),
            expires_at: oauth
                .and_then(|value| value.get("expiresAt"))
                .and_then(Value::as_i64),
            scopes,
            provider,
            account_id: credential
                .pointer("/tokens/account_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
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
        Provider::Claude => oauth.and_then(|value| value.get("accessToken")),
        Provider::Codex => entry.credential.pointer("/tokens/access_token"),
    }
    .and_then(Value::as_str)
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
    Ok(StoredCredential {
        name: name.to_owned(),
        access_token: access_token.to_owned(),
        expires_at: oauth
            .and_then(|value| value.get("expiresAt"))
            .and_then(Value::as_i64),
        scopes,
        provider: entry.provider,
        account_id: entry
            .credential
            .pointer("/tokens/account_id")
            .and_then(Value::as_str)
            .map(str::to_owned),
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
            "Claude Code has an active OAuth credential that is not saved in Subhub; run `subhub add <name>` before installing the gateway".into(),
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
}
