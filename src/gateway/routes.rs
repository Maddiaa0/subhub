use super::audit::audit_all;
use super::protocol::{CredentialReport, GatewayStatus, SelectedReport, TokenState};
use super::refresh::refresh_credential;
use super::selection::{
    mark_failed, routing_error_message, select_credential, set_selected_account,
};
use super::state::ProxyState;
use crate::provider::{Provider, StoredCredential};
use crate::{Error, Result};
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use serde::Deserialize;

#[derive(Deserialize)]
pub(super) struct SelectAccountRequest {
    name: String,
}

pub(super) async fn select_account(
    State(state): State<ProxyState>,
    headers: HeaderMap,
    axum::Json(request): axum::Json<SelectAccountRequest>,
) -> Response {
    if !authorized(&state, &headers) {
        return error_response(StatusCode::UNAUTHORIZED, "invalid local proxy token");
    }
    if !set_selected_account(&state, &request.name).await {
        return error_response(
            StatusCode::NOT_FOUND,
            "credential is not available to the gateway",
        );
    }
    json_response(
        StatusCode::OK,
        serde_json::json!({"selected": request.name}),
    )
}

/// Re-read the index and vault so logins performed while the gateway is
/// running become routable without a restart.
pub(super) async fn reload_accounts(
    State(state): State<ProxyState>,
    headers: HeaderMap,
) -> Response {
    if !authorized(&state, &headers) {
        return error_response(StatusCode::UNAUTHORIZED, "invalid local proxy token");
    }
    let loaded = match tokio::task::spawn_blocking(crate::gateway_credentials).await {
        Ok(Ok(credentials)) => credentials,
        Ok(Err(error)) => {
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string());
        }
        Err(error) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    };
    if loaded.is_empty() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "no credentials saved; run `subhub add <name>`",
        );
    }
    let names: Vec<String> = loaded
        .iter()
        .map(|credential| credential.name.clone())
        .collect();
    // Each lock is taken and released on its own so this can never deadlock
    // with request handlers that hold another of these locks.
    *state.credentials.write().await = loaded;
    state
        .health
        .write()
        .await
        .retain(|name, _| names.contains(name));
    state.selected.lock().await.retain_names(&names);
    // Audit before replying: a credential without usage data is unroutable,
    // so the caller may treat a successful reload as "ready to serve".
    audit_all(&state).await;
    json_response(StatusCode::OK, serde_json::json!({"credentials": names}))
}

pub(super) async fn status(State(state): State<ProxyState>, headers: HeaderMap) -> Response {
    if !authorized(&state, &headers) {
        return error_response(StatusCode::UNAUTHORIZED, "invalid local proxy token");
    }
    let now = chrono::Utc::now().timestamp_millis();
    let stored = state.credentials.read().await.clone();
    let health = state.health.read().await.clone();
    let credentials = health
        .into_iter()
        .map(|(name, entry)| {
            let credential = stored.iter().find(|credential| credential.name == name);
            let token_state = match credential.and_then(|credential| credential.expires_at) {
                Some(expires_at) if expires_at <= now => TokenState::Expired,
                Some(expires_at) if expires_at <= now + 60_000 => TokenState::RefreshDue,
                Some(_) => TokenState::Valid,
                None => TokenState::Unknown,
            };
            let report = CredentialReport {
                provider: credential.map(|credential| credential.provider),
                token_state,
                token_expires_at: credential.and_then(|credential| credential.expires_at),
                usage: entry.usage,
                error: entry.error,
                checked_at: entry.checked_at,
            };
            (name, report)
        })
        .collect();
    let selected = state.selected.lock().await.clone();
    let status = GatewayStatus {
        selected: SelectedReport {
            claude: selected.claude,
            codex: selected.codex,
        },
        credentials,
    };
    json_response(
        StatusCode::OK,
        serde_json::to_value(&status).unwrap_or_default(),
    )
}

pub(super) async fn proxy(State(state): State<ProxyState>, request: Request) -> Response {
    if !authorized(&state, request.headers()) {
        return error_response(StatusCode::UNAUTHORIZED, "invalid local proxy token");
    }
    let (parts, body) = request.into_parts();
    let provider = Provider::from_request_path(parts.uri.path());
    let bytes = match axum::body::to_bytes(body, 32 * 1024 * 1024).await {
        Ok(bytes) => bytes,
        Err(error) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                &format!("could not read body: {error}"),
            );
        }
    };
    let model = serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|value| {
            value
                .get("model")
                .and_then(|value| value.as_str())
                .map(str::to_owned)
        });

    let first = match select_credential(&state, model.as_deref(), None, provider).await {
        Some(credential) => credential,
        None => {
            return error_response(
                StatusCode::TOO_MANY_REQUESTS,
                &routing_error_message(&state, provider).await,
            );
        }
    };
    match forward(&state, &parts, bytes.clone(), &first).await {
        Ok(response) if response.status() == StatusCode::UNAUTHORIZED => {
            if first.provider.supports_refresh()
                && let Ok(refreshed) =
                    refresh_credential(&state, &first.name, true, Some(&first.access_token)).await
            {
                return forward(&state, &parts, bytes, &refreshed)
                    .await
                    .map(into_axum_response)
                    .unwrap_or_else(|error| {
                        error_response(StatusCode::BAD_GATEWAY, &error.to_string())
                    });
            }
            mark_failed(&state, &first.name, StatusCode::UNAUTHORIZED).await;
            if let Some(second) =
                select_credential(&state, model.as_deref(), Some(&first.name), provider).await
            {
                return forward(&state, &parts, bytes, &second)
                    .await
                    .map(into_axum_response)
                    .unwrap_or_else(|error| {
                        error_response(StatusCode::BAD_GATEWAY, &error.to_string())
                    });
            }
            into_axum_response(response)
        }
        Ok(response) if response.status() == StatusCode::TOO_MANY_REQUESTS => {
            let status = response.status();
            mark_failed(&state, &first.name, status).await;
            if let Some(second) =
                select_credential(&state, model.as_deref(), Some(&first.name), provider).await
            {
                return forward(&state, &parts, bytes, &second)
                    .await
                    .map(into_axum_response)
                    .unwrap_or_else(|error| {
                        error_response(StatusCode::BAD_GATEWAY, &error.to_string())
                    });
            }
            into_axum_response(response)
        }
        Ok(response) => into_axum_response(response),
        Err(error) => error_response(StatusCode::BAD_GATEWAY, &error.to_string()),
    }
}

async fn forward(
    state: &ProxyState,
    parts: &axum::http::request::Parts,
    body: Bytes,
    credential: &StoredCredential,
) -> Result<reqwest::Response> {
    let path = parts
        .uri
        .path_and_query()
        .map(|value| value.as_str())
        .unwrap_or("/");
    let upstream = credential.provider.upstream();
    let path = credential.provider.rewrite_upstream_path(path);
    let mut request = state
        .client
        .request(parts.method.clone(), format!("{upstream}{path}"))
        .bearer_auth(&credential.access_token)
        .body(body);
    request = match credential.provider {
        Provider::Claude => request.header("anthropic-beta", oauth_beta_header(&parts.headers)),
        Provider::Codex => request.header("openai-beta", "codex-1"),
    };
    if let Some(account) = &credential.account_id {
        request = request.header("chatgpt-account-id", account);
    }
    for (name, value) in &parts.headers {
        if !matches!(
            name.as_str(),
            "authorization"
                | "x-api-key"
                | "host"
                | "content-length"
                | "connection"
                | "proxy-authorization"
                | "anthropic-beta"
                | "openai-beta"
        ) {
            request = request.header(name, value);
        }
    }
    request
        .send()
        .await
        .map_err(|error| Error::Message(format!("upstream request failed: {error}")))
}

fn oauth_beta_header(headers: &HeaderMap) -> String {
    const OAUTH_BETA: &str = "oauth-2025-04-20";
    let existing = headers
        .get("anthropic-beta")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if existing
        .split(',')
        .map(str::trim)
        .any(|value| value == OAUTH_BETA)
    {
        existing.to_owned()
    } else if existing.is_empty() {
        OAUTH_BETA.into()
    } else {
        format!("{existing},{OAUTH_BETA}")
    }
}

fn authorized(state: &ProxyState, headers: &HeaderMap) -> bool {
    let expected = format!("Bearer {}", state.client_token);
    headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == expected)
        || headers
            .get("x-api-key")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value == state.client_token.as_str())
}

fn into_axum_response(response: reqwest::Response) -> Response {
    let status = response.status();
    let headers = response.headers().clone();
    let mut builder = Response::builder().status(status);
    for (name, value) in &headers {
        if !matches!(
            name.as_str(),
            "content-length" | "connection" | "transfer-encoding"
        ) {
            builder = builder.header(name, value);
        }
    }
    builder
        .body(Body::from_stream(response.bytes_stream()))
        .unwrap_or_else(|_| error_response(StatusCode::INTERNAL_SERVER_ERROR, "response error"))
}

fn error_response(status: StatusCode, message: &str) -> Response {
    json_response(
        status,
        serde_json::json!({
            "type":"error",
            "error":{"type":"subhub_error","message":message}
        }),
    )
}

fn json_response(status: StatusCode, value: serde_json::Value) -> Response {
    Response::builder()
        .status(status)
        .header("content-type", HeaderValue::from_static("application/json"))
        .body(Body::from(value.to_string()))
        .expect("static response is valid")
}

#[cfg(test)]
mod tests {
    use super::super::state::test_state;
    use super::*;

    #[test]
    fn local_auth_accepts_bearer_or_api_key() {
        let state = test_state();
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer local-secret".parse().unwrap());
        assert!(authorized(&state, &headers));
        headers.clear();
        headers.insert("x-api-key", "local-secret".parse().unwrap());
        assert!(authorized(&state, &headers));
        headers.insert("x-api-key", "wrong".parse().unwrap());
        assert!(!authorized(&state, &headers));
    }

    #[test]
    fn oauth_beta_is_added_without_discarding_client_features() {
        let mut headers = HeaderMap::new();
        assert_eq!(oauth_beta_header(&headers), "oauth-2025-04-20");
        headers.insert("anthropic-beta", "feature-a,feature-b".parse().unwrap());
        assert_eq!(
            oauth_beta_header(&headers),
            "feature-a,feature-b,oauth-2025-04-20"
        );
        headers.insert(
            "anthropic-beta",
            "feature-a,oauth-2025-04-20".parse().unwrap(),
        );
        assert_eq!(oauth_beta_header(&headers), "feature-a,oauth-2025-04-20");
    }
}
