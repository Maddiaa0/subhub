mod audit;
pub(crate) mod protocol;
mod refresh;
mod routes;
mod selection;
mod state;

use crate::provider::StoredCredential;
use crate::usage::UsageClient;
use crate::{Error, Result, claude_version, service};
use axum::Router;
use axum::routing::any;
use rand::distr::{Alphanumeric, SampleString};
use state::{ProxyState, SelectedAccounts};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, RwLock};

pub(crate) struct ServeOptions {
    pub listen: String,
    pub client_token: Option<String>,
    pub reserve_percent: f64,
    pub audit_interval: u64,
    pub background: bool,
    pub initial_selected: Vec<String>,
    pub credentials: Vec<StoredCredential>,
}

pub(crate) async fn serve(options: ServeOptions) -> Result<()> {
    if options.credentials.is_empty() {
        return Err(Error::Message(
            "no credentials saved; run `subhub add <name>`".into(),
        ));
    }
    if !(0.0..100.0).contains(&options.reserve_percent) {
        return Err(Error::Message(
            "reserve-percent must be between 0 and 100".into(),
        ));
    }
    let address: std::net::SocketAddr = options
        .listen
        .parse()
        .map_err(|error| Error::Message(format!("invalid listen address: {error}")))?;
    if !address.ip().is_loopback() {
        return Err(Error::Message(
            "refusing non-loopback listen address; the MVP is local-only".into(),
        ));
    }

    let client_token = match options
        .client_token
        .or_else(|| service::read_gateway_token().ok())
    {
        Some(token) => token,
        None if options.background => {
            return Err(Error::Message(
                "background gateway token is missing; run `subhub gateway install` again".into(),
            ));
        }
        None => Alphanumeric.sample_string(&mut rand::rng(), 32),
    };
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| Error::Message(format!("could not build HTTP client: {error}")))?;
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
            audit::audit_all(&audit_state).await;
        }
    });
    let refresh_state = state.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(15));
        loop {
            ticker.tick().await;
            refresh::refresh_due_credentials(&refresh_state).await;
        }
    });

    let app = Router::new()
        .route("/_subhub/status", axum::routing::get(routes::status))
        .route(
            "/_subhub/select",
            axum::routing::post(routes::select_account),
        )
        .route(
            "/_subhub/reload",
            axum::routing::post(routes::reload_accounts),
        )
        .route("/{*path}", any(routes::proxy))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .map_err(|error| Error::Message(format!("could not listen on {address}: {error}")))?;

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
        .map_err(|error| Error::Message(format!("proxy server failed: {error}")))
}
