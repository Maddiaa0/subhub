//! Iron response-retry HTTP callbacks. These authorize at most one exact
//! replay and report its outcome without ever accepting a destination change.

use super::attempts::Attempt;
use crate::gateway::refresh::refresh_credential;
use crate::gateway::routes::{error_response, json_response};
use crate::gateway::routing::{retry_header_overrides, rotate_after_failure};
use crate::gateway::state::ProxyState;
use crate::provider::{Provider, StoredCredential};
use axum::Json;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Response, StatusCode};
use rand::distr::{Alphanumeric, SampleString};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Deserialize)]
pub(crate) struct DecisionRequest {
    scheme: String,
    host: String,
    method: String,
    path: String,
    replayable: bool,
    status: u16,
    #[serde(default)]
    response_headers: HashMap<String, Vec<String>>,
    sandbox_id: String,
    #[serde(default)]
    traceparent: String,
}

#[derive(Debug, Serialize)]
struct DecisionResponse {
    retry: bool,
    headers: HashMap<String, String>,
    attempt_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    traceparent: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct CompletionRequest {
    attempt_id: String,
    replay_status: Option<u16>,
    #[serde(default)]
    response_headers: HashMap<String, Vec<String>>,
    #[serde(default)]
    transport_error: String,
    #[serde(default)]
    traceparent: String,
    #[serde(default)]
    replay_duration_ms: i64,
    #[serde(default)]
    charge_duration_ms: i64,
}

pub(crate) async fn authorize(
    State(state): State<ProxyState>,
    headers: HeaderMap,
    Json(request): Json<DecisionRequest>,
) -> Response<Body> {
    if !authorized(&state, &headers) {
        return error_response(StatusCode::UNAUTHORIZED, "invalid Iron retry token");
    }
    if !request.replayable {
        return decision_response(None, None);
    }
    let status = match StatusCode::from_u16(request.status) {
        Ok(StatusCode::UNAUTHORIZED) => StatusCode::UNAUTHORIZED,
        Ok(StatusCode::TOO_MANY_REQUESTS) => StatusCode::TOO_MANY_REQUESTS,
        _ => return decision_response(None, None),
    };
    if request.sandbox_id != state.iron_sandbox_id.as_str()
        || request.scheme != "https"
        || request.traceparent.is_empty()
    {
        return decision_response(None, None);
    }
    // Response headers are intentionally accepted for provider-specific
    // policy later, but never logged because they may contain sensitive data.
    let _ = &request.response_headers;
    let attempt = {
        let mut attempts = state.iron_attempts.lock().await;
        let Some(attempt) = attempts.get(&request.traceparent) else {
            return decision_response(None, None);
        };
        if !matches_attempt(&attempt, &request) {
            return decision_response(None, None);
        }
        let Some(attempt) = attempts.claim_retry(&request.traceparent) else {
            return decision_response(None, None);
        };
        attempt
    };
    let Some(initial) =
        credential_by_name(&state, &attempt.credential_name, attempt.provider).await
    else {
        return decision_response(None, None);
    };
    let selected = if status == StatusCode::TOO_MANY_REQUESTS {
        rotate_after_failure(&state, &initial, attempt.model.as_deref(), status).await
    } else if initial.provider == Provider::Codex {
        // Codex refresh is not currently owned by SubHub. Mark this identity
        // unusable so the next request moves to another account, but do not
        // change identities within this 401 response.
        let _ = rotate_after_failure(
            &state,
            &initial,
            attempt.model.as_deref(),
            StatusCode::UNAUTHORIZED,
        )
        .await;
        None
    } else {
        refresh_credential(&state, &initial.name, true, Some(&initial.access_token))
            .await
            .ok()
    };
    let Some(selected) = selected else {
        return decision_response(None, None);
    };
    if selected.provider != attempt.provider {
        return decision_response(None, None);
    }
    let overrides = match retry_header_overrides(&selected) {
        Ok(headers) => headers,
        Err(_) => return decision_response(None, None),
    };
    let attempt_id = format!(
        "subhub-{}",
        Alphanumeric.sample_string(&mut rand::rng(), 24)
    );
    if !state.iron_attempts.lock().await.authorize_retry(
        &request.traceparent,
        attempt_id.clone(),
        selected.name.clone(),
    ) {
        return decision_response(None, None);
    }
    crate::observability::event(
        "iron_retry_authorized",
        serde_json::json!({
            "provider": attempt.provider,
            "initial_credential": attempt.credential_name,
            "retry_credential": selected.name,
            "status": status.as_u16()
        }),
    );
    decision_response(
        Some(DecisionResponse {
            retry: true,
            headers: overrides,
            attempt_id,
            traceparent: Some(request.traceparent),
        }),
        None,
    )
}

pub(crate) async fn complete(
    State(state): State<ProxyState>,
    headers: HeaderMap,
    Json(request): Json<CompletionRequest>,
) -> Response<Body> {
    if !authorized(&state, &headers) {
        return error_response(StatusCode::UNAUTHORIZED, "invalid Iron retry token");
    }
    // Completion is idempotent. Iron retries this callback once on a 5xx, and
    // a duplicate after successful cleanup should remain successful.
    let attempt = state
        .iron_attempts
        .lock()
        .await
        .complete(&request.attempt_id);
    if let Some(attempt) = attempt {
        if matches!(request.replay_status, Some(401 | 429))
            && let Some(retry_name) = &attempt.retry_credential_name
            && let Some(retry_credential) =
                credential_by_name(&state, retry_name, attempt.provider).await
        {
            let status = StatusCode::from_u16(request.replay_status.unwrap())
                .expect("401 and 429 are valid HTTP status codes");
            let _ =
                rotate_after_failure(&state, &retry_credential, attempt.model.as_deref(), status)
                    .await;
        }
        crate::observability::event(
            "iron_retry_completed",
            serde_json::json!({
                "provider": attempt.provider,
                "initial_credential": attempt.credential_name,
                "retry_credential": attempt.retry_credential_name,
                "status": request.replay_status,
                "transport_error": !request.transport_error.is_empty(),
                "replay_duration_ms": request.replay_duration_ms,
                "charge_duration_ms": request.charge_duration_ms
            }),
        );
    }
    let _ = (&request.response_headers, &request.traceparent);
    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(Body::empty())
        .expect("static completion response is valid")
}

fn authorized(state: &ProxyState, headers: &HeaderMap) -> bool {
    if state.iron_retry_token.is_empty() {
        return false;
    }
    let expected = format!("Bearer {}", state.iron_retry_token);
    headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == expected)
}

fn matches_attempt(attempt: &Attempt, request: &DecisionRequest) -> bool {
    normalize_host(&request.host).as_deref() == Some(attempt.host.as_str())
        && request.method.eq_ignore_ascii_case(&attempt.method)
        && request.path == attempt.path
}

fn normalize_host(authority: &str) -> Option<String> {
    let authority = authority.trim().to_ascii_lowercase();
    if let Some((host, port)) = authority.rsplit_once(':') {
        (port == "443" && !host.is_empty() && !host.contains(':')).then(|| host.into())
    } else if authority.is_empty() || authority.contains('@') {
        None
    } else {
        Some(authority)
    }
}

async fn credential_by_name(
    state: &ProxyState,
    name: &str,
    provider: Provider,
) -> Option<StoredCredential> {
    state
        .credentials
        .read()
        .await
        .iter()
        .find(|credential| credential.name == name && credential.provider == provider)
        .cloned()
}

fn decision_response(
    response: Option<DecisionResponse>,
    traceparent: Option<String>,
) -> Response<Body> {
    let response = response.unwrap_or_else(|| DecisionResponse {
        retry: false,
        headers: HashMap::new(),
        attempt_id: String::new(),
        traceparent,
    });
    json_response(
        StatusCode::OK,
        serde_json::to_value(response).unwrap_or_default(),
    )
}

#[cfg(test)]
mod tests {
    use super::super::super::protocol::CredentialUsage;
    use super::super::super::state::test_state;
    use super::super::proto::transform_service_server::TransformService;
    use super::super::proto::{
        HeaderValues, HttpRequest, TransformContext, TransformRequestRequest,
    };
    use super::super::transform::IronTransform;
    use super::*;
    use axum::body::to_bytes;
    use tonic::Request;

    async fn seed_attempt(state: &ProxyState) -> String {
        if let CredentialUsage::Claude(usage) = state
            .health
            .write()
            .await
            .get_mut("full")
            .unwrap()
            .usage
            .as_mut()
            .unwrap()
            && let Some(window) = usage.five_hour.as_mut()
        {
            window.utilization = Some(10.0);
        }
        let service = IronTransform::new(state.clone());
        let response = service
            .transform_request(Request::new(TransformRequestRequest {
                context: Some(TransformContext {
                    sni: "api.anthropic.com".into(),
                    client_cert_der: Vec::new(),
                    tunnel: None,
                }),
                request: Some(HttpRequest {
                    method: "POST".into(),
                    url: "/v1/messages".into(),
                    headers: HashMap::from([(
                        "authorization".into(),
                        HeaderValues {
                            values: vec!["Bearer placeholder".into()],
                        },
                    )]),
                    body: br#"{"model":"claude-sonnet","messages":[]}"#.to_vec(),
                    host: "api.anthropic.com".into(),
                    remote_addr: String::new(),
                }),
            }))
            .await
            .unwrap()
            .into_inner();
        response.annotations["traceparent"].clone()
    }

    fn auth_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer iron-retry-secret".parse().unwrap());
        headers
    }

    #[tokio::test]
    async fn rate_limit_selects_a_different_credential_once() {
        let state = test_state();
        let traceparent = seed_attempt(&state).await;
        let response = authorize(
            State(state.clone()),
            auth_headers(),
            Json(DecisionRequest {
                scheme: "https".into(),
                host: "api.anthropic.com".into(),
                method: "POST".into(),
                path: "/v1/messages".into(),
                replayable: true,
                status: 429,
                response_headers: HashMap::new(),
                sandbox_id: "local-user".into(),
                traceparent: traceparent.clone(),
            }),
        )
        .await;
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let decision: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(decision["retry"], true);
        assert_eq!(decision["headers"]["Authorization"], "Bearer secret-b");
        assert_eq!(state.selected.lock().await.claude.as_deref(), Some("ready"));
        let completion = complete(
            State(state.clone()),
            auth_headers(),
            Json(CompletionRequest {
                attempt_id: decision["attempt_id"].as_str().unwrap().into(),
                replay_status: Some(200),
                response_headers: HashMap::new(),
                transport_error: String::new(),
                traceparent: traceparent.clone(),
                replay_duration_ms: 10,
                charge_duration_ms: 12,
            }),
        )
        .await;
        assert_eq!(completion.status(), StatusCode::NO_CONTENT);
        assert_eq!(state.iron_attempts.lock().await.len(), 0);

        let duplicate = authorize(
            State(state),
            auth_headers(),
            Json(DecisionRequest {
                scheme: "https".into(),
                host: "api.anthropic.com".into(),
                method: "POST".into(),
                path: "/v1/messages".into(),
                replayable: true,
                status: 429,
                response_headers: HashMap::new(),
                sandbox_id: "local-user".into(),
                traceparent,
            }),
        )
        .await;
        let body = to_bytes(duplicate.into_body(), 64 * 1024).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap()["retry"],
            false
        );
    }

    #[tokio::test]
    async fn retry_callback_requires_its_dedicated_token() {
        let state = test_state();
        let response = authorize(
            State(state),
            HeaderMap::new(),
            Json(DecisionRequest {
                scheme: "https".into(),
                host: "api.anthropic.com".into(),
                method: "POST".into(),
                path: "/v1/messages".into(),
                replayable: true,
                status: 429,
                response_headers: HashMap::new(),
                sandbox_id: "local-user".into(),
                traceparent: "missing".into(),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn retry_refuses_a_same_name_credential_from_another_provider() {
        let state = test_state();
        let traceparent = seed_attempt(&state).await;
        let mut credentials = state.credentials.write().await;
        let replaced = credentials
            .iter_mut()
            .find(|credential| credential.name == "full")
            .unwrap();
        replaced.provider = Provider::Codex;
        replaced.access_token = "codex-secret".into();
        replaced.account_id = Some("codex-account".into());
        drop(credentials);

        let response = authorize(
            State(state),
            auth_headers(),
            Json(DecisionRequest {
                scheme: "https".into(),
                host: "api.anthropic.com".into(),
                method: "POST".into(),
                path: "/v1/messages".into(),
                replayable: true,
                status: 429,
                response_headers: HashMap::new(),
                sandbox_id: "local-user".into(),
                traceparent,
            }),
        )
        .await;
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let decision: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(decision["retry"], false);
        assert!(decision["headers"].as_object().unwrap().is_empty());
    }

    #[tokio::test]
    async fn retry_refuses_non_replayable_and_mismatched_requests() {
        let state = test_state();
        let traceparent = seed_attempt(&state).await;
        for (replayable, host, path) in [
            (false, "api.anthropic.com", "/v1/messages"),
            (true, "attacker.example", "/v1/messages"),
            (true, "api.anthropic.com", "/v1/messages?changed=1"),
        ] {
            let response = authorize(
                State(state.clone()),
                auth_headers(),
                Json(DecisionRequest {
                    scheme: "https".into(),
                    host: host.into(),
                    method: "POST".into(),
                    path: path.into(),
                    replayable,
                    status: 429,
                    response_headers: HashMap::new(),
                    sandbox_id: "local-user".into(),
                    traceparent: traceparent.clone(),
                }),
            )
            .await;
            let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&body).unwrap()["retry"],
                false
            );
        }
    }
}
