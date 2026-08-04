pub(crate) mod index;
pub(crate) mod oauth;
pub(crate) mod vault;

use crate::provider::{Provider, StoredCredential};
use crate::{Error, Result};
use index::{Index, index_path, legacy_index_path, load_or_migrate_index};
use serde_json::Value;
use vault::vault_read;

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
        });
    }
    Ok(credentials)
}
