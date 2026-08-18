//! The local credential-routing proxy: serves the Anthropic and Codex
//! surfaces on loopback, audits credential usage in the background, and
//! refreshes OAuth tokens before they expire.

mod audit;
mod iron;
pub(crate) mod protocol;
mod refresh;
mod routes;
mod routing;
mod selection;
mod state;

use crate::provider::StoredCredential;
use crate::usage::UsageClient;
use crate::{Error, Result, claude_version, service};
use axum::Router;
use axum::routing::any;
use rand::distr::{Alphanumeric, SampleString};
use serde::{Deserialize, Serialize};
use state::{ProxyState, SelectedAccounts, persisted_refresh_backoffs};
use std::future::IntoFuture;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, RwLock, watch};

pub(crate) const DEFAULT_LISTEN: &str = "127.0.0.1:7842";
pub(crate) const DEFAULT_IRON_GRPC_LISTEN: &str = "127.0.0.1:7843";
pub(crate) const DEFAULT_IRON_SANDBOX_ID: &str = "local-user";
pub(crate) const MAX_REQUEST_BODY_BYTES: usize = 32 * 1024 * 1024;
/// Iron buffers one extra byte so Subhub can reject oversized bodies instead
/// of accepting a silently truncated prefix.
pub(crate) const IRON_BUFFERED_REQUEST_BODY_BYTES: usize = MAX_REQUEST_BODY_BYTES + 1;
const IRON_GRPC_MAX_MESSAGE_BYTES: usize = IRON_BUFFERED_REQUEST_BODY_BYTES + 2 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum GatewayTransport {
    #[default]
    Direct,
    Iron,
}

impl GatewayTransport {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Iron => "iron",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct IronConfig {
    pub grpc_listen: String,
    pub sandbox_id: String,
}

impl Default for IronConfig {
    fn default() -> Self {
        Self {
            grpc_listen: DEFAULT_IRON_GRPC_LISTEN.into(),
            sandbox_id: DEFAULT_IRON_SANDBOX_ID.into(),
        }
    }
}

pub(crate) enum GatewayMode {
    Direct,
    Iron {
        config: IronConfig,
        retry_token: Option<String>,
    },
}

impl GatewayMode {
    fn transport(&self) -> GatewayTransport {
        match self {
            Self::Direct => GatewayTransport::Direct,
            Self::Iron { .. } => GatewayTransport::Iron,
        }
    }
}

pub(crate) struct ServeOptions {
    pub listen: String,
    pub client_token: Option<String>,
    pub reserve_percent: f64,
    pub audit_interval: u64,
    pub background: bool,
    pub mode: GatewayMode,
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
    let transport = options.mode.transport();
    let iron_config = match &options.mode {
        GatewayMode::Direct => None,
        GatewayMode::Iron { config, .. } => Some(config.clone()),
    };
    let iron_grpc_address = if let Some(config) = &iron_config {
        if config.sandbox_id.is_empty() {
            return Err(Error::Message(
                "Iron sandbox identity must not be empty".into(),
            ));
        }
        let grpc_address: std::net::SocketAddr = config.grpc_listen.parse().map_err(|error| {
            Error::Message(format!("invalid Iron gRPC listen address: {error}"))
        })?;
        if !grpc_address.ip().is_loopback() {
            return Err(Error::Message(
                "refusing non-loopback Iron gRPC listen address; local mode is loopback-only"
                    .into(),
            ));
        }
        if grpc_address == address {
            return Err(Error::Message(
                "Iron gRPC and gateway HTTP listeners must use different addresses".into(),
            ));
        }
        Some(grpc_address)
    } else {
        None
    };

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
    let refresh_backoff = persisted_refresh_backoffs(&options.credentials);
    let iron_retry_token = if let GatewayMode::Iron { retry_token, .. } = options.mode {
        let token = retry_token
            .or_else(|| service::read_iron_retry_token().ok())
            .filter(|token| !token.is_empty())
            .ok_or_else(|| {
                Error::Message(
                    "Iron retry token is missing; run `subhub gateway install --transport iron` or set SUBHUB_IRON_RETRY_TOKEN"
                        .into(),
                )
            })?;
        Arc::new(token)
    } else {
        Arc::default()
    };
    let state = ProxyState {
        usage_client: UsageClient::new(client.clone(), claude_version().as_deref()),
        client,
        credentials: Arc::new(RwLock::new(options.credentials)),
        health: Arc::default(),
        selected: Arc::new(Mutex::new(initial_selected)),
        refresh_locks: Arc::default(),
        refresh_backoff: Arc::new(Mutex::new(refresh_backoff)),
        client_token: Arc::new(client_token.clone()),
        reserve_percent: options.reserve_percent,
        transport,
        iron_attempts: Arc::default(),
        iron_retry_token,
        iron_sandbox_id: Arc::new(
            iron_config
                .as_ref()
                .map(|config| config.sandbox_id.clone())
                .unwrap_or_default(),
        ),
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
        );
    let app = match transport {
        GatewayTransport::Direct => app.route("/{*path}", any(routes::proxy)),
        GatewayTransport::Iron => app
            .route(
                "/_subhub/iron/retry/authorize",
                axum::routing::post(iron::retry::authorize),
            )
            .route(
                "/_subhub/iron/retry/complete",
                axum::routing::post(iron::retry::complete),
            ),
    }
    .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .map_err(|error| Error::Message(format!("could not listen on {address}: {error}")))?;

    if !options.background {
        println!(
            "subhub {} gateway listening on http://{address}",
            transport.name()
        );
        if transport == GatewayTransport::Direct {
            println!("export ANTHROPIC_BASE_URL=http://{address}");
            println!("export ANTHROPIC_AUTH_TOKEN={client_token}");
        } else {
            println!(
                "Iron TransformService listening on {}",
                iron_grpc_address.expect("Iron mode validates its gRPC listener")
            );
            let config = iron_config
                .as_ref()
                .expect("Iron mode retains its configuration");
            println!(
                "Run `subhub gateway iron-config --listen {address} --iron-grpc-listen {} --iron-sandbox-id {}` for the matching Iron configuration.",
                config.grpc_listen,
                shell_quote_argument(&config.sandbox_id)
            );
        }
        println!("Press Ctrl-C to stop.");
    }
    if transport == GatewayTransport::Iron {
        serve_iron(
            listener,
            app,
            iron_grpc_address.expect("Iron mode validates its gRPC listener"),
            state,
        )
        .await
    } else {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = tokio::signal::ctrl_c().await;
            })
            .await
            .map_err(|error| Error::Message(format!("proxy server failed: {error}")))
    }
}

fn shell_quote_argument(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

async fn serve_iron(
    listener: tokio::net::TcpListener,
    app: Router,
    grpc_address: std::net::SocketAddr,
    state: ProxyState,
) -> Result<()> {
    use iron::proto::transform_service_server::TransformServiceServer;
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let http = axum::serve(listener, app)
        .with_graceful_shutdown(wait_for_shutdown(shutdown_rx.clone()))
        .into_future();
    let grpc = tonic::transport::Server::builder()
        .add_service(
            TransformServiceServer::new(iron::IronTransform::new(state))
                .max_decoding_message_size(IRON_GRPC_MAX_MESSAGE_BYTES),
        )
        .serve_with_shutdown(grpc_address, wait_for_shutdown(shutdown_rx));
    tokio::pin!(http);
    tokio::pin!(grpc);
    tokio::select! {
        result = &mut http => {
            let _ = shutdown_tx.send(true);
            result.map_err(|error| Error::Message(format!("gateway HTTP server failed: {error}")))?;
            grpc.await.map_err(|error| Error::Message(format!("Iron gRPC server failed: {error}")))
        }
        result = &mut grpc => {
            let _ = shutdown_tx.send(true);
            result.map_err(|error| Error::Message(format!("Iron gRPC server failed: {error}")))?;
            http.await.map_err(|error| Error::Message(format!("gateway HTTP server failed: {error}")))
        }
        signal = tokio::signal::ctrl_c() => {
            signal.map_err(|error| Error::Message(format!("could not listen for shutdown: {error}")))?;
            let _ = shutdown_tx.send(true);
            let (http_result, grpc_result) = tokio::join!(http, grpc);
            http_result.map_err(|error| Error::Message(format!("gateway HTTP server failed: {error}")))?;
            grpc_result.map_err(|error| Error::Message(format!("Iron gRPC server failed: {error}")))
        }
    }
}

async fn wait_for_shutdown(mut shutdown: watch::Receiver<bool>) {
    while !*shutdown.borrow() {
        if shutdown.changed().await.is_err() {
            break;
        }
    }
}
