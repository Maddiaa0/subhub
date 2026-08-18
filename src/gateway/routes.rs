//! HTTP handlers: the admin endpoints (`/_subhub/*`) and the forwarding
//! proxy, including the 401/429 retry-with-another-credential paths.

use super::audit::audit_all;
use super::protocol::{CredentialReport, GatewayStatus, SelectedReport, TokenState};
use super::refresh::REFRESH_MARGIN_MS;
use super::routing::{
    apply_credential_headers, refresh_after_unauthorized, request_model, rotate_after_failure,
    select_initial,
};
use super::selection::set_selected_account;
use super::state::{ProxyState, persisted_refresh_backoffs};
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
    let refresh_backoff = persisted_refresh_backoffs(&loaded);
    *state.credentials.write().await = loaded;
    *state.refresh_backoff.lock().await = refresh_backoff;
    state.refresh_locks.lock().await.clear();
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
                Some(expires_at) if expires_at <= now + REFRESH_MARGIN_MS => TokenState::RefreshDue,
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
        transport: state.transport,
        outstanding_iron_attempts: state.iron_attempts.lock().await.len(),
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
    let model = request_model(&bytes);

    let first = match select_initial(&state, provider, model.as_deref()).await {
        Ok(credential) => credential,
        Err(error) => {
            return error_response(StatusCode::TOO_MANY_REQUESTS, &error.to_string());
        }
    };
    match forward(&state, &parts, bytes.clone(), &first).await {
        Ok(mut response) if response.status() == StatusCode::UNAUTHORIZED => {
            if let Ok(refreshed) = refresh_after_unauthorized(&state, &first).await {
                match forward(&state, &parts, bytes.clone(), &refreshed).await {
                    Ok(retried) if retried.status() != StatusCode::UNAUTHORIZED => {
                        return into_axum_response(retried);
                    }
                    Ok(still_unauthorized) => response = still_unauthorized,
                    Err(error) => {
                        return error_response(StatusCode::BAD_GATEWAY, &error.to_string());
                    }
                }
            }
            if let Some(second) =
                rotate_after_failure(&state, &first, model.as_deref(), StatusCode::UNAUTHORIZED)
                    .await
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
            if let Some(second) =
                rotate_after_failure(&state, &first, model.as_deref(), status).await
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
    let mut headers = HeaderMap::new();
    for (name, value) in &parts.headers {
        if !matches!(
            name.as_str(),
            "authorization"
                | "x-api-key"
                | "host"
                | "content-length"
                | "connection"
                | "proxy-authorization"
        ) {
            headers.append(name, value.clone());
        }
    }
    apply_credential_headers(&mut headers, credential)?;
    state
        .client
        .request(parts.method.clone(), format!("{upstream}{path}"))
        .headers(headers)
        .body(body)
        .send()
        .await
        .map_err(|error| Error::Message(format!("upstream request failed: {error}")))
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

pub(super) fn error_response(status: StatusCode, message: &str) -> Response {
    json_response(
        status,
        serde_json::json!({
            "type":"error",
            "error":{"type":"subhub_error","message":message}
        }),
    )
}

pub(super) fn json_response(status: StatusCode, value: serde_json::Value) -> Response {
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
}
