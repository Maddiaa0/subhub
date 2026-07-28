use crate::lifecycle;
use crate::usage::{UsageClient, UsageSnapshot};
use crate::{AppError, Result, StoredCredential, claude_version};
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use axum::routing::any;
use rand::distr::{Alphanumeric, SampleString};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex, RwLock};

const UPSTREAM: &str = "https://api.anthropic.com";

pub(crate) struct ServeOptions {
    pub listen: String,
    pub client_token: Option<String>,
    pub reserve_percent: f64,
    pub audit_interval: u64,
    pub background: bool,
    pub initial_selected: Option<String>,
    pub credentials: Vec<StoredCredential>,
}

#[derive(Clone, Debug, Serialize)]
struct CredentialHealth {
    usage: Option<UsageSnapshot>,
    error: Option<String>,
    checked_at: u64,
}

#[derive(Clone)]
struct ProxyState {
    client: reqwest::Client,
    usage_client: UsageClient,
    credentials: Arc<Vec<StoredCredential>>,
    health: Arc<RwLock<HashMap<String, CredentialHealth>>>,
    selected: Arc<Mutex<Option<String>>>,
    client_token: Arc<String>,
    reserve_percent: f64,
}

pub(crate) async fn serve(options: ServeOptions) -> Result<()> {
    if options.credentials.is_empty() {
        return Err(AppError(
            "no credentials saved; run `subhub add <name>`".into(),
        ));
    }
    if !(0.0..100.0).contains(&options.reserve_percent) {
        return Err(AppError("reserve-percent must be between 0 and 100".into()));
    }
    let address: std::net::SocketAddr = options
        .listen
        .parse()
        .map_err(|error| AppError(format!("invalid listen address: {error}")))?;
    if !address.ip().is_loopback() {
        return Err(AppError(
            "refusing non-loopback listen address; the MVP is local-only".into(),
        ));
    }

    let client_token = match options
        .client_token
        .or_else(|| lifecycle::read_gateway_token().ok())
    {
        Some(token) => token,
        None if options.background => {
            return Err(AppError(
                "background gateway token is missing; run `subhub gateway install` again".into(),
            ));
        }
        None => Alphanumeric.sample_string(&mut rand::rng(), 32),
    };
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| AppError(format!("could not build HTTP client: {error}")))?;
    let initial_selected = options.initial_selected.filter(|name| {
        options
            .credentials
            .iter()
            .any(|credential| credential.name == *name)
    });
    let state = ProxyState {
        usage_client: UsageClient::new(client.clone(), claude_version().as_deref()),
        client,
        credentials: Arc::new(options.credentials),
        health: Arc::default(),
        selected: Arc::new(Mutex::new(initial_selected)),
        client_token: Arc::new(client_token.clone()),
        reserve_percent: options.reserve_percent,
    };
    let audit_state = state.clone();
    let interval = options.audit_interval.max(30);
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(interval));
        loop {
            ticker.tick().await;
            audit_all(&audit_state).await;
        }
    });

    let app = Router::new()
        .route("/_subhub/status", axum::routing::get(status))
        .route("/_subhub/select", axum::routing::post(select_account))
        .route("/{*path}", any(proxy))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .map_err(|error| AppError(format!("could not listen on {address}: {error}")))?;

    if !options.background {
        println!("subhub proxy listening on http://{address}");
        println!("export ANTHROPIC_BASE_URL=http://{address}");
        println!("export ANTHROPIC_AUTH_TOKEN={client_token}");
        println!("Press Ctrl-C to stop.");
    }
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .map_err(|error| AppError(format!("proxy server failed: {error}")))
}

#[derive(Deserialize)]
struct SelectAccountRequest {
    name: String,
}

async fn select_account(
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

async fn set_selected_account(state: &ProxyState, name: &str) -> bool {
    if !state
        .credentials
        .iter()
        .any(|credential| credential.name == name)
    {
        return false;
    }
    *state.selected.lock().await = Some(name.to_owned());
    true
}

async fn audit_all(state: &ProxyState) {
    for credential in state.credentials.iter() {
        let result = if !credential.scopes.is_empty()
            && !credential
                .scopes
                .iter()
                .any(|scope| scope == "user:profile")
        {
            Err(AppError("OAuth token lacks user:profile scope".into()))
        } else {
            state.usage_client.fetch(&credential.access_token).await
        };
        let health = match result {
            Ok(usage) => CredentialHealth {
                usage: Some(usage),
                error: None,
                checked_at: now(),
            },
            Err(error) => CredentialHealth {
                usage: None,
                error: Some(error.to_string()),
                checked_at: now(),
            },
        };
        state
            .health
            .write()
            .await
            .insert(credential.name.clone(), health);
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

async fn status(State(state): State<ProxyState>, headers: HeaderMap) -> Response {
    if !authorized(&state, &headers) {
        return error_response(StatusCode::UNAUTHORIZED, "invalid local proxy token");
    }
    json_response(
        StatusCode::OK,
        serde_json::json!({
            "selected": state.selected.lock().await.clone(),
            "credentials": state.health.read().await.clone()
        }),
    )
}

async fn proxy(State(state): State<ProxyState>, request: Request) -> Response {
    if !authorized(&state, request.headers()) {
        return error_response(StatusCode::UNAUTHORIZED, "invalid local proxy token");
    }
    let (parts, body) = request.into_parts();
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

    let first = match select_credential(&state, model.as_deref(), None).await {
        Some(credential) => credential,
        None => {
            return error_response(
                StatusCode::TOO_MANY_REQUESTS,
                "no audited credential currently has usage available",
            );
        }
    };
    match forward(&state, &parts, bytes.clone(), &first).await {
        Ok(response)
            if matches!(
                response.status(),
                StatusCode::UNAUTHORIZED | StatusCode::TOO_MANY_REQUESTS
            ) =>
        {
            let status = response.status();
            mark_failed(&state, &first.name, status).await;
            if let Some(second) =
                select_credential(&state, model.as_deref(), Some(&first.name)).await
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

async fn select_credential(
    state: &ProxyState,
    model: Option<&str>,
    exclude: Option<&str>,
) -> Option<StoredCredential> {
    let health = state.health.read().await;
    let current = state.selected.lock().await.clone();
    let eligible = |credential: &&StoredCredential| {
        exclude != Some(credential.name.as_str())
            && health
                .get(&credential.name)
                .and_then(|entry| entry.usage.as_ref())
                .is_some_and(|usage| usage.eligible(model, state.reserve_percent))
    };
    if let Some(name) = current
        && let Some(credential) = state
            .credentials
            .iter()
            .find(|credential| credential.name == name)
            .filter(eligible)
    {
        return Some(credential.clone());
    }
    let selected = state
        .credentials
        .iter()
        .filter(eligible)
        .min_by(|a, b| {
            utilization(&health, &a.name, model).total_cmp(&utilization(&health, &b.name, model))
        })
        .cloned();
    if let Some(credential) = &selected {
        *state.selected.lock().await = Some(credential.name.clone());
    }
    selected
}

fn utilization(health: &HashMap<String, CredentialHealth>, name: &str, model: Option<&str>) -> f64 {
    health[name]
        .usage
        .as_ref()
        .and_then(|usage| usage.tightest_utilization(model))
        .unwrap_or(0.0)
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
    let mut request = state
        .client
        .request(parts.method.clone(), format!("{UPSTREAM}{path}"))
        .bearer_auth(&credential.access_token)
        .header("anthropic-beta", oauth_beta_header(&parts.headers))
        .body(body);
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
        ) {
            request = request.header(name, value);
        }
    }
    request
        .send()
        .await
        .map_err(|error| AppError(format!("upstream request failed: {error}")))
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

async fn mark_failed(state: &ProxyState, name: &str, status: StatusCode) {
    if let Some(health) = state.health.write().await.get_mut(name) {
        health.error = Some(format!("inference request returned {status}"));
        if status == StatusCode::UNAUTHORIZED {
            health.usage = None;
        } else if let Some(window) = health
            .usage
            .as_mut()
            .and_then(|usage| usage.five_hour.as_mut())
        {
            window.utilization = Some(100.0);
        }
    }
    let mut selected = state.selected.lock().await;
    if selected.as_deref() == Some(name) {
        *selected = None;
    }
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
    use super::*;

    fn test_state() -> ProxyState {
        let client = reqwest::Client::new();
        ProxyState {
            usage_client: UsageClient::new(client.clone(), None),
            client,
            credentials: Arc::new(vec![
                StoredCredential {
                    name: "full".into(),
                    access_token: "secret-a".into(),
                    scopes: vec!["user:profile".into()],
                },
                StoredCredential {
                    name: "ready".into(),
                    access_token: "secret-b".into(),
                    scopes: vec!["user:profile".into()],
                },
            ]),
            health: Arc::new(RwLock::new(HashMap::from([
                (
                    "full".into(),
                    CredentialHealth {
                        usage: Some(
                            serde_json::from_value(serde_json::json!({
                                "five_hour": {"utilization": 100.0, "resets_at": null}
                            }))
                            .unwrap(),
                        ),
                        error: None,
                        checked_at: 1,
                    },
                ),
                (
                    "ready".into(),
                    CredentialHealth {
                        usage: Some(
                            serde_json::from_value(serde_json::json!({
                                "five_hour": {"utilization": 25.0, "resets_at": null}
                            }))
                            .unwrap(),
                        ),
                        error: None,
                        checked_at: 1,
                    },
                ),
            ]))),
            selected: Arc::default(),
            client_token: Arc::new("local-secret".into()),
            reserve_percent: 1.0,
        }
    }

    #[tokio::test]
    async fn selection_skips_exhausted_and_remains_sticky() {
        let state = test_state();
        let selected = select_credential(&state, Some("claude-sonnet"), None)
            .await
            .unwrap();
        assert_eq!(selected.name, "ready");
        assert_eq!(state.selected.lock().await.as_deref(), Some("ready"));
        let selected_again = select_credential(&state, Some("claude-sonnet"), None)
            .await
            .unwrap();
        assert_eq!(selected_again.name, "ready");
    }

    #[tokio::test]
    async fn manual_selection_updates_known_account_only() {
        let state = test_state();
        assert!(set_selected_account(&state, "ready").await);
        assert_eq!(state.selected.lock().await.as_deref(), Some("ready"));
        assert!(!set_selected_account(&state, "missing").await);
        assert_eq!(state.selected.lock().await.as_deref(), Some("ready"));
    }

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
