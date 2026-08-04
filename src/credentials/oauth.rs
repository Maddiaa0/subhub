//! Claude OAuth: token refresh against the provider token endpoint, scope
//! validation, and oauthAccount metadata handling.

use crate::credentials::gateway_credentials;
use crate::credentials::index::{index_path, legacy_index_path, load_or_migrate_index};
use crate::credentials::vault::{
    ACTIVE_SERVICE, VAULT_SERVICE, VaultEntry, credential_write, current_user, vault_read,
};
use crate::paths::{claude_config_path, save_json_file};
use crate::provider::{Provider, StoredCredential};
use crate::{Error, Result};
use serde::Deserialize;
use serde_json::Value;
use std::fs;
use std::process::Command;

pub(crate) const CLAUDE_OAUTH_SCOPES_ENV: &str = "CLAUDE_CODE_OAUTH_SCOPES";
pub(crate) const REQUESTED_OAUTH_SCOPES: [&str; 4] = [
    "user:inference",
    "user:profile",
    "user:sessions:claude_code",
    "user:mcp_servers",
];
const REQUIRED_OAUTH_SCOPES: [&str; 2] = ["user:inference", "user:profile"];

const CLAUDE_TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const CLAUDE_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

#[derive(Deserialize)]
struct ClaudeRefreshResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: i64,
}

pub(crate) async fn refresh_claude_credential(
    client: &reqwest::Client,
    name: &str,
) -> Result<StoredCredential> {
    let stored = vault_read(name)?;
    let mut entry: VaultEntry = serde_json::from_str(&stored)?;
    if entry.provider != Provider::Claude {
        return Err(Error::Message(format!(
            "credential \"{name}\" is not a Claude credential"
        )));
    }
    let oauth = entry
        .credential
        .get_mut("claudeAiOauth")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| Error::Refresh(format!("credential \"{name}\" has no Claude OAuth data")))?;
    let refresh_token = oauth
        .get("refreshToken")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| Error::Refresh(format!("credential \"{name}\" has no refresh token")))?
        .to_owned();
    let refreshed = request_claude_refresh(client, CLAUDE_TOKEN_URL, &refresh_token).await?;
    oauth.insert("accessToken".into(), Value::String(refreshed.access_token));
    if let Some(refresh_token) = refreshed.refresh_token.filter(|token| !token.is_empty()) {
        oauth.insert("refreshToken".into(), Value::String(refresh_token));
    }
    let expires_at = chrono::Utc::now().timestamp_millis() + refreshed.expires_in * 1000;
    oauth.insert("expiresAt".into(), Value::Number(expires_at.into()));

    let encoded = serde_json::to_string(&entry)?;
    credential_write(VAULT_SERVICE, name, &encoded)?;
    let index = load_or_migrate_index(&index_path()?, &legacy_index_path()?)?;
    if index.active_for(Provider::Claude) == Some(name) {
        credential_write(
            ACTIVE_SERVICE,
            &current_user()?,
            &serde_json::to_string(&entry.credential)?,
        )?;
    }
    gateway_credentials()?
        .into_iter()
        .find(|credential| credential.name == name)
        .ok_or_else(|| Error::Message(format!("refreshed credential \"{name}\" disappeared")))
}

async fn request_claude_refresh(
    client: &reqwest::Client,
    token_url: &str,
    refresh_token: &str,
) -> Result<ClaudeRefreshResponse> {
    let response = client
        .post(token_url)
        .header("anthropic-beta", "oauth-2025-04-20")
        .header(
            "user-agent",
            format!(
                "claude-code/{}",
                claude_version().as_deref().unwrap_or("2.1.0")
            ),
        )
        .json(&serde_json::json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "client_id": CLAUDE_CLIENT_ID
        }))
        .send()
        .await
        .map_err(|error| Error::Refresh(format!("Claude OAuth refresh request failed: {error}")))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(Error::Refresh(format!(
            "Claude OAuth refresh returned {status}: {}",
            body.chars().take(300).collect::<String>()
        )));
    }
    let refreshed: ClaudeRefreshResponse = response.json().await.map_err(|error| {
        Error::Refresh(format!("invalid Claude OAuth refresh response: {error}"))
    })?;
    if refreshed.access_token.is_empty() || refreshed.expires_in <= 0 {
        return Err(Error::Refresh(
            "Claude OAuth refresh returned invalid token data".into(),
        ));
    }
    Ok(refreshed)
}

pub(crate) fn claude_version() -> Option<String> {
    let output = Command::new("claude").arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()?
        .split_whitespace()
        .next()
        .map(str::to_owned)
}

pub(crate) fn read_oauth_account() -> Result<Value> {
    let path = claude_config_path()?;
    let contents = fs::read_to_string(&path)
        .map_err(|error| Error::Message(format!("could not read {}: {error}", path.display())))?;
    let config: Value = serde_json::from_str(&contents)
        .map_err(|error| Error::Message(format!("invalid {}: {error}", path.display())))?;
    config.get("oauthAccount").cloned().ok_or_else(|| {
        Error::Message(format!(
            "Claude login did not write oauthAccount metadata to {}",
            path.display()
        ))
    })
}

pub(crate) fn write_oauth_account(oauth_account: &Value) -> Result<()> {
    let path = claude_config_path()?;
    let contents = fs::read_to_string(&path)
        .map_err(|error| Error::Message(format!("could not read {}: {error}", path.display())))?;
    let mut config: Value = serde_json::from_str(&contents)
        .map_err(|error| Error::Message(format!("invalid {}: {error}", path.display())))?;
    let object = config
        .as_object_mut()
        .ok_or_else(|| Error::Message(format!("{} is not a JSON object", path.display())))?;
    object.insert("oauthAccount".into(), oauth_account.clone());
    save_json_file(&path, &config)
}

pub(crate) fn requested_oauth_scopes(existing: Option<&std::ffi::OsStr>) -> String {
    let mut scopes: Vec<String> = existing
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or_default()
        .split_whitespace()
        .map(str::to_owned)
        .collect();
    for requested in REQUESTED_OAUTH_SCOPES {
        if !scopes.iter().any(|scope| scope == requested) {
            scopes.push(requested.to_owned());
        }
    }
    scopes.join(" ")
}

pub(crate) fn validate_required_scopes(raw: &str) -> Result<()> {
    let parsed: Value = serde_json::from_str(raw)
        .map_err(|_| Error::Message("credential is not valid JSON".into()))?;
    let scopes = parsed
        .get("claudeAiOauth")
        .and_then(|oauth| oauth.get("scopes"))
        .and_then(Value::as_array)
        .ok_or_else(|| {
            Error::Message(
                "Claude login returned no OAuth scopes; update Claude Code and log in again".into(),
            )
        })?;
    let missing: Vec<&str> = REQUIRED_OAUTH_SCOPES
        .iter()
        .copied()
        .filter(|required| {
            !scopes
                .iter()
                .filter_map(Value::as_str)
                .any(|scope| scope == *required)
        })
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(Error::Message(format!(
            "Claude login did not grant required OAuth scope(s): {}; \
             update Claude Code and retry `subhub add`",
            missing.join(", ")
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, routing::post};
    use tokio::sync::oneshot;

    async fn refresh_server(
        response: Value,
    ) -> (String, oneshot::Receiver<(Value, axum::http::HeaderMap)>) {
        let (sent, received) = oneshot::channel();
        let sent = std::sync::Arc::new(std::sync::Mutex::new(Some(sent)));
        let app = Router::new().route(
            "/token",
            post({
                let sent = sent.clone();
                move |headers, Json(body): Json<Value>| {
                    let sent = sent.clone();
                    let response = response.clone();
                    async move {
                        if let Some(sent) = sent.lock().unwrap().take() {
                            let _ = sent.send((body, headers));
                        }
                        Json(response)
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{address}/token"), received)
    }

    #[tokio::test]
    async fn claude_refresh_matches_provider_protocol_and_accepts_rotation() {
        let (url, received) = refresh_server(serde_json::json!({
            "access_token": "new-access",
            "refresh_token": "new-refresh",
            "expires_in": 3600
        }))
        .await;

        let refreshed = request_claude_refresh(&reqwest::Client::new(), &url, "old-refresh")
            .await
            .unwrap();
        let (body, headers) = received.await.unwrap();

        assert_eq!(body["grant_type"], "refresh_token");
        assert_eq!(body["refresh_token"], "old-refresh");
        assert_eq!(body["client_id"], CLAUDE_CLIENT_ID);
        assert_eq!(headers["anthropic-beta"], "oauth-2025-04-20");
        assert_eq!(refreshed.access_token, "new-access");
        assert_eq!(refreshed.refresh_token.as_deref(), Some("new-refresh"));
        assert_eq!(refreshed.expires_in, 3600);
    }

    #[test]
    fn requested_scopes_preserve_extras_and_add_required_scopes() {
        let scopes =
            requested_oauth_scopes(Some(std::ffi::OsStr::new("custom:scope user:profile")));
        assert_eq!(
            scopes,
            "custom:scope user:profile user:inference user:sessions:claude_code user:mcp_servers"
        );
    }

    #[test]
    fn required_scope_validation_rejects_inference_only_tokens() {
        let complete = serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "a",
                "refreshToken": "r",
                "scopes": REQUESTED_OAUTH_SCOPES
            }
        })
        .to_string();
        let inference_only = serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "a",
                "refreshToken": "r",
                "scopes": ["user:inference"]
            }
        })
        .to_string();
        assert!(validate_required_scopes(&complete).is_ok());
        let error = validate_required_scopes(&inference_only).unwrap_err();
        assert!(error.to_string().contains("user:profile"));
    }
}
