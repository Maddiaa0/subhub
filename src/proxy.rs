use crate::lifecycle;
use crate::usage::{UsageClient, UsageSnapshot};
use crate::{AppError, Provider, Result, StoredCredential, claude_version, codex};
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
    pub initial_selected: Vec<String>,
    pub credentials: Vec<StoredCredential>,
}

#[derive(Clone, Debug, Default, Serialize)]
struct SelectedAccounts {
    claude: Option<String>,
    codex: Option<String>,
}

impl SelectedAccounts {
    fn get(&self, provider: Provider) -> Option<String> {
        self.slot_ref(provider).clone()
    }

    fn slot_ref(&self, provider: Provider) -> &Option<String> {
        match provider {
            Provider::Claude => &self.claude,
            Provider::Codex => &self.codex,
        }
    }

    fn slot(&mut self, provider: Provider) -> &mut Option<String> {
        match provider {
            Provider::Claude => &mut self.claude,
            Provider::Codex => &mut self.codex,
        }
    }

    fn clear_name(&mut self, name: &str) {
        for slot in [&mut self.claude, &mut self.codex] {
            if slot.as_deref() == Some(name) {
                *slot = None;
            }
        }
    }

    fn retain_names(&mut self, names: &[String]) {
        for slot in [&mut self.claude, &mut self.codex] {
            if slot.as_ref().is_some_and(|name| !names.contains(name)) {
                *slot = None;
            }
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CredentialHealth {
    usage: Option<CredentialUsage>,
    error: Option<String>,
    checked_at: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
enum CredentialUsage {
    Claude(UsageSnapshot),
    Codex(codex::UsageSnapshot),
}

impl CredentialUsage {
    fn eligible(&self, model: Option<&str>, reserve: f64) -> bool {
        match self {
            Self::Claude(usage) => usage.eligible(model, reserve),
            Self::Codex(usage) => usage.eligible(reserve),
        }
    }
    fn utilization(&self, model: Option<&str>) -> f64 {
        match self {
            Self::Claude(usage) => usage.tightest_utilization(model).unwrap_or(0.0),
            Self::Codex(usage) => usage.tightest_utilization(),
        }
    }
}

#[derive(Clone)]
struct ProxyState {
    client: reqwest::Client,
    usage_client: UsageClient,
    credentials: Arc<RwLock<Vec<StoredCredential>>>,
    health: Arc<RwLock<HashMap<String, CredentialHealth>>>,
    selected: Arc<Mutex<SelectedAccounts>>,
    refresh_lock: Arc<Mutex<()>>,
    refresh_backoff: Arc<Mutex<HashMap<String, RefreshBackoff>>>,
    client_token: Arc<String>,
    reserve_percent: f64,
}

#[derive(Clone, Copy, Debug)]
struct RefreshBackoff {
    failures: u32,
    retry_at: u64,
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
    let mut initial_selected = SelectedAccounts::default();
    for name in options.initial_selected {
        if let Some(credential) = options
            .credentials
            .iter()
            .find(|credential| credential.name == name)
        {
            let slot = initial_selected.slot(credential.provider);
            if slot.is_none() {
                *slot = Some(name);
            }
        }
    }
    let state = ProxyState {
        usage_client: UsageClient::new(client.clone(), claude_version().as_deref()),
        client,
        credentials: Arc::new(RwLock::new(options.credentials)),
        health: Arc::default(),
        selected: Arc::new(Mutex::new(initial_selected)),
        refresh_lock: Arc::default(),
        refresh_backoff: Arc::default(),
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
    let refresh_state = state.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(15));
        loop {
            ticker.tick().await;
            refresh_due_credentials(&refresh_state).await;
        }
    });

    let app = Router::new()
        .route("/_subhub/status", axum::routing::get(status))
        .route("/_subhub/select", axum::routing::post(select_account))
        .route("/_subhub/reload", axum::routing::post(reload_accounts))
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

async fn refresh_due_credentials(state: &ProxyState) {
    let deadline = chrono::Utc::now().timestamp_millis() + 60_000;
    let credentials = state.credentials.read().await.clone();
    for credential in credentials {
        if credential.provider == Provider::Claude
            && credential
                .expires_at
                .is_some_and(|expires_at| expires_at <= deadline)
            && refresh_retry_ready(state, &credential.name).await
        {
            let _ = refresh_credential(state, &credential.name, false, None).await;
        }
    }
}

async fn refresh_retry_ready(state: &ProxyState, name: &str) -> bool {
    state
        .refresh_backoff
        .lock()
        .await
        .get(name)
        .is_none_or(|backoff| backoff.retry_at <= now())
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
    let Some(provider) = state
        .credentials
        .read()
        .await
        .iter()
        .find(|credential| credential.name == name)
        .map(|credential| credential.provider)
    else {
        return false;
    };
    *state.selected.lock().await.slot(provider) = Some(name.to_owned());
    true
}

/// Re-read the index and vault so logins performed while the gateway is
/// running become routable without a restart.
async fn reload_accounts(State(state): State<ProxyState>, headers: HeaderMap) -> Response {
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

async fn audit_all(state: &ProxyState) {
    let credentials = state.credentials.read().await.clone();
    for credential in &credentials {
        let credential = if credential.provider == Provider::Claude
            && credential.expires_at.is_some_and(|expires_at| {
                expires_at <= chrono::Utc::now().timestamp_millis() + 60_000
            }) {
            match refresh_credential(state, &credential.name, false, None).await {
                Ok(credential) => credential,
                Err(error) => {
                    record_audit(state, credential, Err(error)).await;
                    continue;
                }
            }
        } else {
            credential.clone()
        };
        let mut result = if credential.provider == Provider::Codex {
            match credential.account_id.as_deref() {
                Some(account) => {
                    codex::fetch_usage(&state.client, &credential.access_token, account)
                        .await
                        .map(CredentialUsage::Codex)
                }
                None => Err(AppError("Codex credential has no account id".into())),
            }
        } else if !credential.scopes.is_empty()
            && !credential
                .scopes
                .iter()
                .any(|scope| scope == "user:profile")
        {
            Err(AppError("OAuth token lacks user:profile scope".into()))
        } else {
            state
                .usage_client
                .fetch(&credential.access_token)
                .await
                .map(CredentialUsage::Claude)
        };
        if credential.provider == Provider::Claude
            && result
                .as_ref()
                .is_err_and(|error| error.to_string().contains("unauthorized"))
            && let Ok(refreshed) = refresh_credential(
                state,
                &credential.name,
                true,
                Some(&credential.access_token),
            )
            .await
        {
            result = state
                .usage_client
                .fetch(&refreshed.access_token)
                .await
                .map(CredentialUsage::Claude);
        }
        record_audit(state, &credential, result).await;
    }
}

async fn record_audit(
    state: &ProxyState,
    credential: &StoredCredential,
    result: Result<CredentialUsage>,
) {
    crate::observability::event(
        if result.is_ok() {
            "audit_succeeded"
        } else {
            "audit_failed"
        },
        serde_json::json!({
            "credential": credential.name,
            "provider": credential.provider,
            "error": result.as_ref().err().map(|error| safe_error(error.to_string()))
        }),
    );
    let mut health = state.health.write().await;
    let previous_usage = health
        .get(&credential.name)
        .and_then(|entry| entry.usage.clone());
    health.insert(
        credential.name.clone(),
        audit_health(previous_usage, result),
    );
}

fn safe_error(error: String) -> String {
    error.chars().take(300).collect()
}

async fn refresh_credential(
    state: &ProxyState,
    name: &str,
    force: bool,
    expected_access_token: Option<&str>,
) -> Result<StoredCredential> {
    let _guard = state.refresh_lock.lock().await;
    // Another task may have completed the refresh while this one waited.
    if let Some(current) = state
        .credentials
        .read()
        .await
        .iter()
        .find(|credential| credential.name == name)
        .cloned()
        && ((!force
            && current.expires_at.is_none_or(|expires_at| {
                expires_at > chrono::Utc::now().timestamp_millis() + 60_000
            }))
            || expected_access_token.is_some_and(|expected| current.access_token != expected))
    {
        return Ok(current);
    }
    let refreshed = match crate::refresh_claude_credential(&state.client, name).await {
        Ok(refreshed) => {
            state.refresh_backoff.lock().await.remove(name);
            crate::observability::event(
                "refresh_succeeded",
                serde_json::json!({"credential": name, "provider": "claude"}),
            );
            refreshed
        }
        Err(error) => {
            let mut backoffs = state.refresh_backoff.lock().await;
            let failures = backoffs
                .get(name)
                .map_or(1, |backoff| backoff.failures.saturating_add(1));
            let delay = 30_u64.saturating_mul(2_u64.saturating_pow(failures.min(6) - 1));
            let jitter = rand::random_range(0..=15);
            backoffs.insert(
                name.to_owned(),
                RefreshBackoff {
                    failures,
                    retry_at: now() + delay.min(1_800) + jitter,
                },
            );
            crate::observability::event(
                "refresh_failed",
                serde_json::json!({
                    "credential": name,
                    "provider": "claude",
                    "retry_at": now() + delay.min(1_800) + jitter,
                    "error": safe_error(error.to_string())
                }),
            );
            return Err(error);
        }
    };
    if let Some(current) = state
        .credentials
        .write()
        .await
        .iter_mut()
        .find(|credential| credential.name == name)
    {
        *current = refreshed.clone();
    }
    Ok(refreshed)
}

fn audit_health(
    previous_usage: Option<CredentialUsage>,
    result: Result<CredentialUsage>,
) -> CredentialHealth {
    match result {
        Ok(usage) => CredentialHealth {
            usage: Some(usage),
            error: None,
            checked_at: now(),
        },
        Err(error) => CredentialHealth {
            // A usage audit is advisory. In particular, the usage endpoint can
            // rate-limit independently of inference, so a transient audit
            // failure must not make a working credential unroutable.
            usage: previous_usage,
            error: Some(error.to_string()),
            checked_at: now(),
        },
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
    let mut credentials = serde_json::to_value(state.health.read().await.clone())
        .unwrap_or_else(|_| serde_json::json!({}));
    if let Some(entries) = credentials.as_object_mut() {
        let now = chrono::Utc::now().timestamp_millis();
        for credential in state.credentials.read().await.iter() {
            if let Some(entry) = entries
                .get_mut(&credential.name)
                .and_then(serde_json::Value::as_object_mut)
            {
                let token_state = match credential.expires_at {
                    Some(expires_at) if expires_at <= now => "expired",
                    Some(expires_at) if expires_at <= now + 60_000 => "refresh_due",
                    Some(_) => "valid",
                    None => "unknown",
                };
                entry.insert("token_state".into(), token_state.into());
                entry.insert("token_expires_at".into(), credential.expires_at.into());
                entry.insert(
                    "provider".into(),
                    serde_json::to_value(credential.provider).unwrap_or_default(),
                );
            }
        }
    }
    json_response(
        StatusCode::OK,
        serde_json::json!({
            "selected": state.selected.lock().await.clone(),
            "credentials": credentials
        }),
    )
}

async fn proxy(State(state): State<ProxyState>, request: Request) -> Response {
    if !authorized(&state, request.headers()) {
        return error_response(StatusCode::UNAUTHORIZED, "invalid local proxy token");
    }
    let (parts, body) = request.into_parts();
    let provider = if parts.uri.path().starts_with("/openai/") {
        Provider::Codex
    } else {
        Provider::Claude
    };
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
            if first.provider == Provider::Claude
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

async fn routing_error_message(state: &ProxyState, provider: Provider) -> String {
    let credentials = state.credentials.read().await;
    let health = state.health.read().await;
    let relevant: Vec<_> = credentials
        .iter()
        .filter(|credential| credential.provider == provider)
        .collect();
    if relevant.is_empty() {
        return match provider {
            Provider::Claude => "no Claude credential configured; run `subhub add <name>`".into(),
            Provider::Codex => "no Codex credential configured; run `subhub add <name>`".into(),
        };
    }
    if let Some((credential, error)) = relevant.iter().find_map(|credential| {
        health
            .get(&credential.name)
            .and_then(|entry| entry.error.as_deref())
            .filter(|error| error.contains("refresh"))
            .map(|error| (*credential, error))
    }) {
        return format!(
            "credential `{}` could not refresh: {}; run `subhub add {} --force` if its refresh token was revoked",
            credential.name, error, credential.name
        );
    }
    if relevant.iter().all(|credential| {
        health
            .get(&credential.name)
            .is_some_and(|entry| entry.usage.is_some())
    }) {
        return "all credentials are at their configured usage limit; check `subhub audit` for reset times".into();
    }
    let detail = relevant.iter().find_map(|credential| {
        health
            .get(&credential.name)
            .and_then(|entry| entry.error.as_deref())
    });
    match detail {
        Some(error) => {
            format!("credential usage is unknown because its latest audit failed: {error}")
        }
        None => "credentials are still being audited; retry shortly".into(),
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
    provider: Provider,
) -> Option<StoredCredential> {
    let credentials = state.credentials.read().await.clone();
    let health = state.health.read().await;
    let current = state.selected.lock().await.get(provider);
    let eligible = |credential: &&StoredCredential| {
        credential.provider == provider
            && exclude != Some(credential.name.as_str())
            && health
                .get(&credential.name)
                .and_then(|entry| entry.usage.as_ref())
                .is_some_and(|usage| usage.eligible(model, state.reserve_percent))
    };
    if let Some(name) = current
        && let Some(credential) = credentials
            .iter()
            .find(|credential| credential.name == name)
            .filter(eligible)
    {
        return Some(credential.clone());
    }
    let selected = credentials
        .iter()
        .filter(eligible)
        .min_by(|a, b| {
            utilization(&health, &a.name, model).total_cmp(&utilization(&health, &b.name, model))
        })
        .cloned();
    let selected = selected.or_else(|| {
        credentials
            .iter()
            .filter(|credential| {
                credential.provider == provider
                    && exclude != Some(credential.name.as_str())
                    && health.get(&credential.name).is_some_and(|entry| {
                        entry.usage.is_none()
                            && entry.error.as_deref().is_some_and(advisory_audit_error)
                    })
            })
            .min_by_key(|credential| health[&credential.name].checked_at)
            .cloned()
    });
    if let Some(credential) = &selected {
        *state.selected.lock().await.slot(provider) = Some(credential.name.clone());
    }
    selected
}

fn advisory_audit_error(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    ![
        "unauthorized",
        "forbidden",
        "lacks user",
        "refresh",
        "no account id",
    ]
    .iter()
    .any(|blocked| error.contains(blocked))
}

fn utilization(health: &HashMap<String, CredentialHealth>, name: &str, model: Option<&str>) -> f64 {
    health[name]
        .usage
        .as_ref()
        .map(|usage| usage.utilization(model))
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
    let (upstream, path) = if credential.provider == Provider::Codex {
        (
            codex::RESPONSES_UPSTREAM,
            path.strip_prefix("/openai").unwrap_or(path),
        )
    } else {
        (UPSTREAM, path)
    };
    let mut request = state
        .client
        .request(parts.method.clone(), format!("{upstream}{path}"))
        .bearer_auth(&credential.access_token)
        .body(body);
    if credential.provider == Provider::Codex {
        request = request.header("openai-beta", "codex-1");
    } else {
        request = request.header("anthropic-beta", oauth_beta_header(&parts.headers));
    }
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
        } else if let Some(usage) = health.usage.as_mut() {
            match usage {
                CredentialUsage::Claude(usage) => {
                    if let Some(window) = usage.five_hour.as_mut() {
                        window.utilization = Some(100.0);
                    }
                }
                CredentialUsage::Codex(usage) => {
                    if let Some(window) = usage.rate_limit.primary_window.as_mut() {
                        window.used_percent = Some(100.0);
                    }
                }
            }
        }
    }
    state.selected.lock().await.clear_name(name);
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
            credentials: Arc::new(RwLock::new(vec![
                StoredCredential {
                    name: "full".into(),
                    access_token: "secret-a".into(),
                    expires_at: None,
                    scopes: vec!["user:profile".into()],
                    provider: Provider::Claude,
                    account_id: None,
                },
                StoredCredential {
                    name: "ready".into(),
                    access_token: "secret-b".into(),
                    expires_at: None,
                    scopes: vec!["user:profile".into()],
                    provider: Provider::Claude,
                    account_id: None,
                },
            ])),
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
            refresh_lock: Arc::default(),
            refresh_backoff: Arc::default(),
            client_token: Arc::new("local-secret".into()),
            reserve_percent: 1.0,
        }
    }

    #[tokio::test]
    async fn selection_skips_exhausted_and_remains_sticky() {
        let state = test_state();
        let selected = select_credential(&state, Some("claude-sonnet"), None, Provider::Claude)
            .await
            .unwrap();
        assert_eq!(selected.name, "ready");
        assert_eq!(state.selected.lock().await.claude.as_deref(), Some("ready"));
        let selected_again =
            select_credential(&state, Some("claude-sonnet"), None, Provider::Claude)
                .await
                .unwrap();
        assert_eq!(selected_again.name, "ready");
    }

    #[tokio::test]
    async fn routing_error_explains_refresh_recovery() {
        let state = test_state();
        let mut health = state.health.write().await;
        health.get_mut("ready").unwrap().usage = None;
        health.get_mut("ready").unwrap().error =
            Some("Claude OAuth refresh returned 400 Bad Request".into());
        drop(health);

        let message = routing_error_message(&state, Provider::Claude).await;
        assert!(message.contains("could not refresh"));
        assert!(message.contains("subhub add ready --force"));
        assert!(!message.contains("access_token"));
    }

    #[tokio::test]
    async fn transient_audit_failure_can_fall_back_to_inference() {
        let state = test_state();
        let mut health = state.health.write().await;
        health.get_mut("full").unwrap().usage = None;
        health.get_mut("full").unwrap().error = Some("usage request timed out".into());
        health.get_mut("ready").unwrap().usage = None;
        health.get_mut("ready").unwrap().error = Some("OAuth token is unauthorized".into());
        drop(health);

        let selected = select_credential(&state, None, None, Provider::Claude)
            .await
            .unwrap();
        assert_eq!(selected.name, "full");
    }

    #[tokio::test]
    async fn refresh_backoff_suppresses_retries_until_due() {
        let state = test_state();
        state.refresh_backoff.lock().await.insert(
            "ready".into(),
            RefreshBackoff {
                failures: 3,
                retry_at: now() + 300,
            },
        );
        assert!(!refresh_retry_ready(&state, "ready").await);
        state
            .refresh_backoff
            .lock()
            .await
            .get_mut("ready")
            .unwrap()
            .retry_at = now();
        assert!(refresh_retry_ready(&state, "ready").await);
    }

    #[tokio::test]
    async fn manual_selection_updates_known_account_only() {
        let state = test_state();
        assert!(set_selected_account(&state, "ready").await);
        assert_eq!(state.selected.lock().await.claude.as_deref(), Some("ready"));
        assert!(!set_selected_account(&state, "missing").await);
        assert_eq!(state.selected.lock().await.claude.as_deref(), Some("ready"));
    }

    #[tokio::test]
    async fn provider_selection_never_crosses_subscription_types() {
        let state = test_state();
        state.credentials.write().await.push(StoredCredential {
            name: "codex".into(),
            access_token: "secret-c".into(),
            expires_at: None,
            scopes: Vec::new(),
            provider: Provider::Codex,
            account_id: Some("account-c".into()),
        });
        state.health.write().await.insert(
            "codex".into(),
            CredentialHealth {
                usage: Some(CredentialUsage::Codex(
                    serde_json::from_value(serde_json::json!({
                        "rate_limit": {
                            "primary_window": {"used_percent": 10.0, "reset_at": 1},
                            "secondary_window": {"used_percent": 20.0, "reset_at": 2}
                        }
                    }))
                    .unwrap(),
                )),
                error: None,
                checked_at: 1,
            },
        );
        let selected = select_credential(&state, None, None, Provider::Codex)
            .await
            .unwrap();
        assert_eq!(selected.name, "codex");
    }

    #[tokio::test]
    async fn each_provider_keeps_its_own_selection() {
        let state = test_state();
        state.credentials.write().await.push(StoredCredential {
            name: "codex".into(),
            access_token: "secret-c".into(),
            expires_at: None,
            scopes: Vec::new(),
            provider: Provider::Codex,
            account_id: Some("account-c".into()),
        });
        state.health.write().await.insert(
            "codex".into(),
            CredentialHealth {
                usage: Some(CredentialUsage::Codex(
                    serde_json::from_value(serde_json::json!({
                        "rate_limit": {
                            "primary_window": {"used_percent": 10.0, "reset_at": 1}
                        }
                    }))
                    .unwrap(),
                )),
                error: None,
                checked_at: 1,
            },
        );
        select_credential(&state, Some("claude-sonnet"), None, Provider::Claude).await;
        select_credential(&state, None, None, Provider::Codex).await;
        let selected = state.selected.lock().await.clone();
        assert_eq!(selected.claude.as_deref(), Some("ready"));
        assert_eq!(selected.codex.as_deref(), Some("codex"));

        mark_failed(&state, "codex", StatusCode::UNAUTHORIZED).await;
        let selected = state.selected.lock().await.clone();
        assert_eq!(selected.claude.as_deref(), Some("ready"));
        assert_eq!(selected.codex, None);
    }

    #[test]
    fn reload_drops_selections_for_removed_credentials() {
        let mut selected = SelectedAccounts {
            claude: Some("kept".into()),
            codex: Some("removed".into()),
        };
        selected.retain_names(&["kept".to_string()]);
        assert_eq!(selected.claude.as_deref(), Some("kept"));
        assert_eq!(selected.codex, None);
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

    #[test]
    fn failed_audit_retains_last_known_usage() {
        let previous = CredentialUsage::Claude(
            serde_json::from_value(serde_json::json!({
                "five_hour": {"utilization": 25.0, "resets_at": null}
            }))
            .unwrap(),
        );

        let health = audit_health(
            Some(previous),
            Err(AppError(
                "usage endpoint rate limited; retry after 300".into(),
            )),
        );

        assert_eq!(
            health
                .usage
                .as_ref()
                .and_then(|usage| match usage {
                    CredentialUsage::Claude(usage) => usage.five_hour.as_ref(),
                    CredentialUsage::Codex(_) => None,
                })
                .and_then(|window| window.utilization),
            Some(25.0)
        );
        assert_eq!(
            health.error.as_deref(),
            Some("usage endpoint rate limited; retry after 300")
        );
    }

    #[test]
    fn initial_failed_audit_remains_unroutable() {
        let health = audit_health(None, Err(AppError("usage request failed".into())));

        assert!(health.usage.is_none());
        assert_eq!(health.error.as_deref(), Some("usage request failed"));
    }
}
