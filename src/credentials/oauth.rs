//! Claude OAuth: token refresh against the provider token endpoint, scope
//! validation, and oauthAccount metadata handling.

use crate::credentials::stored_credential_from_entry;
use crate::credentials::vault::{
    VaultEntry, acquire_refresh_owner, credential_write_owned, vault_read_owned,
};
use crate::paths::{claude_config_path, save_json_file};
use crate::provider::{Provider, StoredCredential};
use crate::{Error, Result};
use serde::Deserialize;
use serde_json::Value;
use std::fs;
use std::process::Command;
use std::time::Duration;

pub(crate) const CLAUDE_OAUTH_SCOPES_ENV: &str = "CLAUDE_CODE_OAUTH_SCOPES";
pub(crate) const REQUESTED_OAUTH_SCOPES: [&str; 5] = [
    "user:inference",
    "user:profile",
    "user:sessions:claude_code",
    "user:mcp_servers",
    "user:file_upload",
];
const REQUIRED_OAUTH_SCOPES: [&str; 2] = ["user:inference", "user:profile"];

const CLAUDE_TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const CLAUDE_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

#[derive(Debug, Deserialize)]
struct ClaudeRefreshResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: i64,
    #[serde(default)]
    refresh_token_expires_in: Option<i64>,
    #[serde(default)]
    scope: Option<String>,
}

pub(crate) async fn refresh_claude_credential(
    client: &reqwest::Client,
    name: &str,
    expected_access_token: Option<&str>,
) -> Result<StoredCredential> {
    // The lease spans the final read, provider request, and durable write. A
    // second gateway waits, re-reads, and observes the winner instead of
    // reusing a rotating refresh token.
    let lease = tokio::task::spawn_blocking(acquire_refresh_owner)
        .await
        .map_err(|error| Error::refresh_transient(format!("OAuth owner lock failed: {error}")))?
        .map_err(|error| Error::refresh_transient(error.to_string()))?;
    let stored = vault_read_owned(name, &lease)
        .map_err(|error| Error::refresh_terminal(error.to_string()))?;
    let mut entry: VaultEntry = serde_json::from_str(&stored).map_err(|error| {
        Error::refresh_terminal(format!("credential \"{name}\" is invalid: {error}"))
    })?;
    if entry.provider != Provider::Claude {
        return Err(Error::refresh_terminal(format!(
            "credential \"{name}\" is not a Claude credential"
        )));
    }
    let current = stored_credential_from_entry(name, &entry)
        .map_err(|error| Error::refresh_terminal(error.to_string()))?;
    if expected_access_token.is_some_and(|expected| expected != current.access_token) {
        return Ok(current);
    }
    let oauth = entry
        .credential
        .get("claudeAiOauth")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            Error::refresh_terminal(format!("credential \"{name}\" has no Claude OAuth data"))
        })?;
    let refresh_token = oauth
        .get("refreshToken")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| {
            Error::refresh_terminal(format!("credential \"{name}\" has no refresh token"))
        })?
        .to_owned();
    let client_id = oauth
        .get("clientId")
        .or_else(|| oauth.get("client_id"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .unwrap_or(CLAUDE_CLIENT_ID)
        .to_owned();
    let scopes = oauth
        .get("scopes")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| REQUESTED_OAUTH_SCOPES.join(" "));
    let refreshed = match request_claude_refresh(
        client,
        CLAUDE_TOKEN_URL,
        &refresh_token,
        &client_id,
        &scopes,
    )
    .await
    {
        Ok(refreshed) => refreshed,
        Err(error) if error.refresh_is_terminal() => {
            entry.refresh_error = Some(error.to_string().chars().take(300).collect());
            let encoded = serde_json::to_string(&entry)?;
            credential_write_owned(
                crate::credentials::vault::VAULT_SERVICE,
                name,
                &encoded,
                &lease,
            )
            .map_err(|persist_error| {
                Error::refresh_terminal(format!(
                    "{error}; Subhub also could not persist the terminal refresh state: {persist_error}"
                ))
            })?;
            return Err(error);
        }
        Err(error) => return Err(error),
    };
    let now = chrono::Utc::now().timestamp_millis();
    entry.refresh_error = None;
    let oauth = entry
        .credential
        .get_mut("claudeAiOauth")
        .and_then(Value::as_object_mut)
        .expect("Claude OAuth object was validated above");
    oauth.insert("accessToken".into(), Value::String(refreshed.access_token));
    if let Some(refresh_token) = refreshed.refresh_token.filter(|token| !token.is_empty()) {
        oauth.insert("refreshToken".into(), Value::String(refresh_token));
    }
    let expires_at = now + refreshed.expires_in * 1000;
    oauth.insert("expiresAt".into(), Value::Number(expires_at.into()));
    if let Some(expires_in) = refreshed
        .refresh_token_expires_in
        .filter(|expires_in| *expires_in > 0)
    {
        oauth.insert(
            "refreshTokenExpiresAt".into(),
            Value::Number((now + expires_in * 1000).into()),
        );
    }
    if let Some(scopes) = refreshed.scope.filter(|scope| !scope.is_empty()) {
        oauth.insert(
            "scopes".into(),
            Value::Array(
                scopes
                    .split_whitespace()
                    .map(|scope| Value::String(scope.to_owned()))
                    .collect(),
            ),
        );
    }

    let encoded = serde_json::to_string(&entry)?;
    credential_write_owned(
        crate::credentials::vault::VAULT_SERVICE,
        name,
        &encoded,
        &lease,
    )
    .map_err(|error| {
        Error::refresh_terminal(format!(
            "Claude rotated credential \"{name}\", but Subhub could not persist it: {error}; the old refresh token will not be retried"
        ))
    })?;
    stored_credential_from_entry(name, &entry)
}

async fn request_claude_refresh(
    client: &reqwest::Client,
    token_url: &str,
    refresh_token: &str,
    client_id: &str,
    scopes: &str,
) -> Result<ClaudeRefreshResponse> {
    let response = client
        .post(token_url)
        .json(&serde_json::json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "client_id": client_id,
            "scope": scopes
        }))
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|error| {
            Error::refresh_transient(format!("Claude OAuth refresh request failed: {error}"))
        })?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        let oauth_error = oauth_error_fields(&body);
        let detail = oauth_error
            .as_ref()
            .map(|(code, description)| {
                description.as_ref().map_or_else(
                    || code.clone(),
                    |description| format!("{code}: {description}"),
                )
            })
            .unwrap_or_else(|| body.clone());
        let message = format!(
            "Claude OAuth refresh returned {status}: {}",
            detail.chars().take(300).collect::<String>()
        );
        if refresh_failure_is_transient(status, oauth_error.as_ref().map(|(code, _)| code.as_str()))
        {
            return Err(Error::refresh_transient(message));
        }
        if oauth_error.is_some() {
            return Err(Error::refresh_terminal(message));
        }
        if status.is_client_error()
            && status != reqwest::StatusCode::REQUEST_TIMEOUT
            && status != reqwest::StatusCode::TOO_MANY_REQUESTS
        {
            return Err(Error::refresh_terminal(format!(
                "Claude OAuth refresh returned unrecoverable {status}: {}",
                body.chars().take(300).collect::<String>()
            )));
        }
        return Err(Error::refresh_transient(format!(
            "Claude OAuth refresh returned {status} without an OAuth error: {}",
            body.chars().take(300).collect::<String>()
        )));
    }
    let refreshed: ClaudeRefreshResponse = response.json().await.map_err(|error| {
        Error::refresh_transient(format!("invalid Claude OAuth refresh response: {error}"))
    })?;
    if refreshed.access_token.is_empty() || refreshed.expires_in <= 0 {
        return Err(Error::refresh_transient(
            "Claude OAuth refresh returned invalid token data",
        ));
    }
    Ok(refreshed)
}

fn refresh_failure_is_transient(status: reqwest::StatusCode, oauth_code: Option<&str>) -> bool {
    status == reqwest::StatusCode::REQUEST_TIMEOUT
        || status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
        || matches!(
            oauth_code,
            Some("server_error" | "temporarily_unavailable" | "rate_limit_error")
        )
}

fn oauth_error_fields(body: &str) -> Option<(String, Option<String>)> {
    let value: Value = serde_json::from_str(body).ok()?;
    let error = value.get("error")?;
    if let Some(code) = error.as_str().filter(|code| !code.is_empty()) {
        let description = value
            .get("error_description")
            .and_then(Value::as_str)
            .filter(|description| !description.is_empty())
            .map(str::to_owned);
        return Some((code.to_owned(), description));
    }
    let error = error.as_object()?;
    let code = ["type", "code", "error"]
        .into_iter()
        .find_map(|key| error.get(key).and_then(Value::as_str))
        .filter(|code| !code.is_empty())?;
    let description = ["message", "description", "error_description"]
        .into_iter()
        .find_map(|key| error.get(key).and_then(Value::as_str))
        .filter(|description| !description.is_empty())
        .map(str::to_owned);
    Some((code.to_owned(), description))
}

#[cfg(test)]
fn oauth_error_detail(body: &str) -> Option<String> {
    oauth_error_fields(body).map(|(code, description)| {
        description.map_or_else(
            || code.clone(),
            |description| format!("{code}: {description}"),
        )
    })
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
        refresh_server_with_status(axum::http::StatusCode::OK, response).await
    }

    async fn refresh_server_with_status(
        status: axum::http::StatusCode,
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
                        (status, Json(response))
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

        let refreshed = request_claude_refresh(
            &reqwest::Client::new(),
            &url,
            "old-refresh",
            "stored-client",
            "user:inference user:profile",
        )
        .await
        .unwrap();
        let (body, headers) = received.await.unwrap();

        assert_eq!(body["grant_type"], "refresh_token");
        assert_eq!(body["refresh_token"], "old-refresh");
        assert_eq!(body["client_id"], "stored-client");
        assert_eq!(body["scope"], "user:inference user:profile");
        assert!(!headers.contains_key("anthropic-beta"));
        assert_eq!(refreshed.access_token, "new-access");
        assert_eq!(refreshed.refresh_token.as_deref(), Some("new-refresh"));
        assert_eq!(refreshed.expires_in, 3600);
    }

    #[tokio::test]
    async fn invalid_grant_is_terminal_but_provider_failures_are_retryable() {
        let (url, _) = refresh_server_with_status(
            axum::http::StatusCode::BAD_REQUEST,
            serde_json::json!({
                "error": "invalid_grant",
                "error_description": "refresh token was already used"
            }),
        )
        .await;
        let error = request_claude_refresh(
            &reqwest::Client::new(),
            &url,
            "old-refresh",
            CLAUDE_CLIENT_ID,
            "user:inference",
        )
        .await
        .unwrap_err();
        assert!(error.refresh_is_terminal());
        assert!(error.to_string().contains("invalid_grant"));

        assert_eq!(
            oauth_error_detail(
                r#"{"type":"error","error":{"type":"invalid_grant","message":"token reused"}}"#
            )
            .as_deref(),
            Some("invalid_grant: token reused")
        );

        for (status, response) in [
            (
                axum::http::StatusCode::TOO_MANY_REQUESTS,
                serde_json::json!({
                    "error": {"type": "rate_limit_error", "message": "try later"}
                }),
            ),
            (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                serde_json::json!({
                    "error": "temporarily_unavailable",
                    "error_description": "maintenance"
                }),
            ),
        ] {
            let (url, _) = refresh_server_with_status(status, response).await;
            let error = request_claude_refresh(
                &reqwest::Client::new(),
                &url,
                "old-refresh",
                CLAUDE_CLIENT_ID,
                "user:inference",
            )
            .await
            .unwrap_err();
            assert!(!error.refresh_is_terminal());
        }

        let (url, _) = refresh_server_with_status(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            serde_json::json!({"message": "temporarily unavailable"}),
        )
        .await;
        let error = request_claude_refresh(
            &reqwest::Client::new(),
            &url,
            "old-refresh",
            CLAUDE_CLIENT_ID,
            "user:inference",
        )
        .await
        .unwrap_err();
        assert!(!error.refresh_is_terminal());
    }

    #[test]
    fn requested_scopes_preserve_extras_and_add_required_scopes() {
        let scopes =
            requested_oauth_scopes(Some(std::ffi::OsStr::new("custom:scope user:profile")));
        assert_eq!(
            scopes,
            "custom:scope user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload"
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
