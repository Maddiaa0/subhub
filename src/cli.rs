//! Command-line interface: clap argument types, dispatch, and the handlers
//! for `add`, `list`, `set`, and `audit`. Gateway subcommands delegate to
//! [`crate::gateway`] and [`crate::service`].

use crate::credentials::index::{
    Index, index_path, legacy_index_path, load_or_migrate_index, save_index, validate_name,
};
use crate::credentials::oauth::{
    CLAUDE_OAUTH_SCOPES_ENV, claude_version, read_oauth_account, requested_oauth_scopes,
    validate_required_scopes, write_oauth_account,
};
use crate::credentials::vault::{
    VAULT_SERVICE, VaultEntry, active_credential_read, credential_write, validate_credential,
    vault_read,
};
use crate::credentials::{
    ensure_unique_claude_identity, retire_active_claude_credential, stored_credentials,
};
use crate::provider::Provider;
use crate::{Error, Result, codex, gateway, runtime, service, usage};
use chrono::{DateTime, Local};
use clap::{Parser, Subcommand, ValueEnum};
use serde::Serialize;
use serde_json::Value;
use std::env;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};

#[derive(Parser)]
#[command(
    name = "subhub",
    version,
    about = "Manage Claude Code and Codex subscriptions"
)]
pub(crate) struct Cli {
    #[command(subcommand)]
    command: Commands,
}

impl Cli {
    pub(crate) fn parse_args() -> Self {
        Self::parse()
    }
}

#[derive(Subcommand)]
enum Commands {
    /// Choose Claude Code or Codex and save its subscription under NAME
    Add {
        /// Friendly name for the credential
        name: String,
        /// Replace an existing credential with the same name
        #[arg(long, short)]
        force: bool,
        /// Capture Claude Code's current OAuth login instead of opening a new login
        #[arg(long, conflicts_with = "device_auth")]
        capture: bool,
        /// Use device-code authentication for the Codex login (for remote or headless machines)
        #[arg(long, short = 'd', conflicts_with = "capture")]
        device_auth: bool,
    },
    /// List saved credential names and mark the active one
    List,
    /// Select the gateway's preferred saved credential
    Set {
        /// Friendly name of the saved credential
        name: String,
        /// Require the credential to belong to this provider
        #[arg(long, value_enum)]
        provider: Option<Provider>,
    },
    /// Query subscription usage for every saved credential
    Audit {
        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,
    },
    /// Manage the local Claude and Codex credential gateway
    Gateway {
        #[command(subcommand)]
        command: GatewayCommands,
    },
}

#[derive(Subcommand)]
enum GatewayCommands {
    /// Run the local credential-routing gateway
    Serve {
        /// Loopback address to listen on
        #[arg(long, default_value = gateway::DEFAULT_LISTEN)]
        listen: String,
        /// Secret Claude Code must send as ANTHROPIC_AUTH_TOKEN
        #[arg(long, env = "SUBHUB_CLIENT_TOKEN")]
        client_token: Option<String>,
        /// Percentage of capacity to keep in reserve
        #[arg(long, default_value_t = 1.0)]
        reserve_percent: f64,
        /// Seconds between background usage audits
        #[arg(long, default_value_t = 120)]
        audit_interval: u64,
        /// Request transport: direct Subhub proxying or Iron control plane
        #[arg(long, value_enum, default_value = "direct")]
        transport: GatewayTransportArg,
        /// Loopback address for Iron's external TransformService
        #[arg(long, default_value = gateway::DEFAULT_IRON_GRPC_LISTEN)]
        iron_grpc_listen: String,
        /// Dedicated bearer used by Iron's response-retry callbacks
        #[arg(long, env = "SUBHUB_IRON_RETRY_TOKEN")]
        iron_retry_token: Option<String>,
        /// Expected Iron sandbox identity in retry callbacks
        #[arg(
            long,
            env = "SUBHUB_IRON_SANDBOX_ID",
            default_value = gateway::DEFAULT_IRON_SANDBOX_ID
        )]
        iron_sandbox_id: String,
        /// Internal LaunchAgent mode
        #[arg(long, hide = true)]
        background: bool,
    },
    /// Install and start the background gateway
    Install {
        /// Request transport to configure for Claude Code and Codex
        #[arg(long, value_enum, default_value = "direct")]
        transport: GatewayTransportArg,
    },
    /// Run uninstall then install in a single command, preserving saved credentials
    Reinstall {
        /// Override the installed request transport
        #[arg(long, value_enum)]
        transport: Option<GatewayTransportArg>,
    },
    /// Stop the gateway and remove its Claude integration
    Uninstall {
        /// Also remove Subhub credentials, token, and index
        #[arg(long)]
        purge: bool,
    },
    /// Start the installed background gateway
    Start,
    /// Stop the installed background gateway
    Stop,
    /// Restart the installed background gateway
    Restart,
    /// Show installation, process, and gateway health
    Status {
        /// Show credential health only for this provider
        #[arg(long, value_enum)]
        provider: Option<Provider>,
    },
    /// Show recent structured gateway events
    Logs {
        /// Maximum number of events to print
        #[arg(long, default_value_t = 100)]
        lines: usize,
    },
    /// Diagnose gateway installation, routing, and credentials
    Doctor,
    /// Print the local gateway authentication token
    AuthToken,
    /// Print an Iron Proxy configuration fragment without embedding secrets
    IronConfig {
        /// Address of Subhub's HTTP retry callback listener
        #[arg(long, default_value = gateway::DEFAULT_LISTEN)]
        listen: String,
        /// Address of Subhub's external TransformService
        #[arg(long, default_value = gateway::DEFAULT_IRON_GRPC_LISTEN)]
        iron_grpc_listen: String,
        /// Sandbox identity Iron sends to retry callbacks
        #[arg(
            long,
            env = "SUBHUB_IRON_SANDBOX_ID",
            default_value = gateway::DEFAULT_IRON_SANDBOX_ID
        )]
        iron_sandbox_id: String,
    },
    /// Print the dedicated Iron response-retry callback token
    IronToken,
    /// Internal placeholder credential used by clients in Iron mode
    #[command(hide = true)]
    ProxyToken,
    /// Internal Claude Code status-line renderer
    #[command(hide = true)]
    Statusline,
}

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
enum GatewayTransportArg {
    #[default]
    Direct,
    Iron,
}

impl From<GatewayTransportArg> for gateway::GatewayTransport {
    fn from(transport: GatewayTransportArg) -> Self {
        match transport {
            GatewayTransportArg::Direct => Self::Direct,
            GatewayTransportArg::Iron => Self::Iron,
        }
    }
}

pub(crate) fn dispatch(cli: Cli) -> Result<()> {
    let path = index_path()?;
    let mut index = load_or_migrate_index(&path, &legacy_index_path()?)?;

    match cli.command {
        Commands::Add {
            name,
            force,
            capture,
            device_auth,
        } => add(&path, &mut index, &name, force, capture, device_auth),
        Commands::List => list(&index),
        Commands::Set { name, provider } => set(&path, &mut index, &name, provider),
        Commands::Audit { json } => {
            let credentials = stored_credentials(&index)?;
            runtime()?.block_on(audit(credentials, json))
        }
        Commands::Gateway { command } => dispatch_gateway(command, &index),
    }
}

fn dispatch_gateway(command: GatewayCommands, index: &Index) -> Result<()> {
    match command {
        GatewayCommands::Serve {
            listen,
            client_token,
            reserve_percent,
            audit_interval,
            transport,
            iron_grpc_listen,
            iron_retry_token,
            iron_sandbox_id,
            background,
        } => {
            retire_active_claude_credential(index)?;
            let credentials = stored_credentials(index)?;
            let mode = match gateway::GatewayTransport::from(transport) {
                gateway::GatewayTransport::Direct => gateway::GatewayMode::Direct,
                gateway::GatewayTransport::Iron => gateway::GatewayMode::Iron {
                    config: gateway::IronConfig {
                        grpc_listen: iron_grpc_listen,
                        sandbox_id: iron_sandbox_id,
                    },
                    retry_token: Some(match iron_retry_token {
                        Some(token) => token,
                        None => service::ensure_iron_retry_token()?,
                    }),
                },
            };
            runtime()?.block_on(gateway::serve(gateway::ServeOptions {
                listen,
                client_token,
                reserve_percent,
                audit_interval,
                background,
                mode,
                initial_selected: index.active_names(),
                credentials,
            }))
        }
        GatewayCommands::Install { transport } => service::install(transport.into()),
        GatewayCommands::Reinstall { transport } => {
            service::reinstall(transport.map(gateway::GatewayTransport::from))
        }
        GatewayCommands::Uninstall { purge } => service::uninstall(purge),
        GatewayCommands::Start => service::start(),
        GatewayCommands::Stop => service::stop(),
        GatewayCommands::Restart => service::restart(),
        GatewayCommands::Status { provider } => service::status(provider),
        GatewayCommands::Logs { lines } => service::logs(lines),
        GatewayCommands::Doctor => service::doctor(),
        GatewayCommands::AuthToken => {
            println!("{}", service::read_gateway_token()?);
            Ok(())
        }
        GatewayCommands::IronConfig {
            listen,
            iron_grpc_listen,
            iron_sandbox_id,
        } => service::print_iron_config(
            &listen,
            &gateway::IronConfig {
                grpc_listen: iron_grpc_listen,
                sandbox_id: iron_sandbox_id,
            },
        ),
        GatewayCommands::IronToken => {
            println!("{}", service::ensure_iron_retry_token()?);
            Ok(())
        }
        GatewayCommands::ProxyToken => {
            println!("{}", service::ensure_iron_proxy_token()?);
            Ok(())
        }
        GatewayCommands::Statusline => service::statusline(),
    }
}

fn add(
    path: &Path,
    index: &mut Index,
    name: &str,
    force: bool,
    capture: bool,
    device_auth: bool,
) -> Result<()> {
    validate_name(name)?;
    if index.contains(name) && !force {
        return Err(Error::Message(format!(
            "credential \"{name}\" already exists; use --force to replace it"
        )));
    }

    println!("Choose subscription type:\n  1) Claude Code\n  2) Codex");
    let mut choice = String::new();
    io::stdin().read_line(&mut choice)?;
    if choice.trim() == "2" {
        if capture {
            return Err(Error::Message(
                "--capture only applies to Claude Code logins; choose subscription type 1".into(),
            ));
        }
        return add_codex(path, index, name, device_auth);
    }
    if device_auth {
        return Err(Error::Message(
            "--device-auth only applies to Codex logins; choose subscription type 2".into(),
        ));
    }
    let credential = if capture {
        println!("Capturing Claude Code's active login as credential \"{name}\"...");
        active_claude_credential()?.ok_or_else(|| {
            Error::Message(
                "Claude Code has no active OAuth credential to capture; run `subhub add <name>` without `--capture` to log in".into(),
            )
        })?
    } else {
        println!("Opening Claude Code login for credential \"{name}\"...");
        let oauth_scopes = requested_oauth_scopes(env::var_os(CLAUDE_OAUTH_SCOPES_ENV).as_deref());
        let status = Command::new("claude")
            .args(["auth", "login", "--claudeai"])
            .env(CLAUDE_OAUTH_SCOPES_ENV, oauth_scopes)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .map_err(|error| {
                Error::Message(format!("could not run `claude auth login`: {error}"))
            })?;

        require_success(status, "`claude auth login` failed")?;
        active_credential_read()?.ok_or_else(|| {
            Error::Message(
                "login completed, but the Claude Code credential could not be read".into(),
            )
        })?
    };
    validate_credential(&credential)?;
    validate_required_scopes(&credential)?;
    let credential_value: Value = serde_json::from_str(&credential)?;
    let oauth_account = read_oauth_account()?;
    let vault_entry = VaultEntry {
        provider: Provider::Claude,
        credential: credential_value,
        oauth_account,
        refresh_error: None,
    };
    if let Err(error) = ensure_unique_claude_identity(index, name, &vault_entry) {
        // Do not leave the just-created refresh family in Claude Code where it
        // could race the canonical credential already owned by the gateway.
        let cleanup = crate::credentials::vault::clear_active_claude_oauth();
        if let Err(cleanup_error) = cleanup {
            return Err(Error::Message(format!(
                "{error}; also failed to retire Claude Code's duplicate OAuth token: {cleanup_error}"
            )));
        }
        return Err(error);
    }
    credential_write(VAULT_SERVICE, name, &serde_json::to_string(&vault_entry)?)?;

    index.add(name, Provider::Claude);
    save_index(path, index)?;
    retire_active_claude_credential(index).map_err(|error| {
        Error::Message(format!(
            "saved \"{name}\", but could not transfer exclusive token ownership to Subhub: {error}"
        ))
    })?;
    println!("Saved \"{name}\"; the Subhub gateway is now its sole OAuth-token owner.");
    notify_gateway_reload();
    Ok(())
}

fn active_claude_credential() -> Result<Option<String>> {
    let Some(raw) = active_credential_read()? else {
        return Ok(None);
    };
    let parsed: Value = serde_json::from_str(&raw).map_err(|error| {
        Error::Message(format!(
            "Claude Code's active credential is not valid JSON: {error}"
        ))
    })?;
    if parsed.get("claudeAiOauth").is_none() {
        return Ok(None);
    }
    validate_credential(&raw)?;
    validate_required_scopes(&raw)?;
    Ok(Some(raw))
}

/// A running gateway holds an in-memory vault snapshot, so a fresh login is
/// invisible to it until it reloads.
fn notify_gateway_reload() {
    match service::reload_gateway_accounts() {
        Ok(true) => println!("Running gateway reloaded credentials."),
        Ok(false) => {}
        Err(error) => eprintln!(
            "warning: running gateway did not reload credentials: {error}; run `subhub gateway restart` to pick up this login"
        ),
    }
}

fn add_codex(path: &Path, index: &mut Index, name: &str, device_auth: bool) -> Result<()> {
    use rand::distr::{Alphanumeric, SampleString};
    let temporary_home = env::temp_dir().join(format!(
        "subhub-codex-login-{}",
        Alphanumeric.sample_string(&mut rand::rng(), 16)
    ));
    fs::create_dir(&temporary_home)?;
    fs::set_permissions(&temporary_home, fs::Permissions::from_mode(0o700))?;
    let auth_path = temporary_home.join("auth.json");
    println!("Opening Codex ChatGPT login for credential \"{name}\"...");
    let mut login = Command::new("codex");
    login.args(["-c", "cli_auth_credentials_store=\"file\"", "login"]);
    if device_auth {
        login.arg("--device-auth");
    }
    let status = login
        .env("CODEX_HOME", &temporary_home)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|error| Error::Message(format!("could not run `codex login`: {error}")))?;
    let capture = (|| {
        require_success(status, "`codex login` failed")?;
        let raw = fs::read_to_string(&auth_path).map_err(|error| {
            Error::Message(format!("Codex login cache was not readable: {error}"))
        })?;
        let credential: Value = serde_json::from_str(&raw)?;
        for key in ["access_token", "refresh_token", "account_id"] {
            if credential
                .pointer(&format!("/tokens/{key}"))
                .and_then(Value::as_str)
                .is_none()
            {
                return Err(Error::Message(format!(
                    "Codex login did not provide tokens.{key}"
                )));
            }
        }
        let entry = serde_json::to_string(&VaultEntry {
            provider: Provider::Codex,
            credential,
            oauth_account: Value::Null,
            refresh_error: None,
        })?;
        credential_write(VAULT_SERVICE, name, &entry)
    })();
    fs::remove_dir_all(&temporary_home)?;
    capture?;
    index.add(name, Provider::Codex);
    save_index(path, index)?;
    println!("Saved Codex subscription \"{name}\".");
    notify_gateway_reload();
    Ok(())
}

fn list(index: &Index) -> Result<()> {
    if index.credentials.is_empty() {
        println!("No credentials saved. Run `subhub add <name>`.");
        return Ok(());
    }

    for name in &index.credentials {
        let provider = vault_read(name)
            .ok()
            .and_then(|stored| serde_json::from_str::<Value>(&stored).ok())
            .and_then(|entry| entry.get("provider").cloned())
            .and_then(|value| serde_json::from_value::<Provider>(value).ok())
            .unwrap_or_default();
        let marker = if index.active_for(provider) == Some(name) {
            "*"
        } else {
            " "
        };
        println!("{marker} {name} [{}]", provider.name());
    }
    Ok(())
}

fn set(
    path: &Path,
    index: &mut Index,
    name: &str,
    expected_provider: Option<Provider>,
) -> Result<()> {
    validate_name(name)?;
    if !index.contains(name) {
        return Err(Error::Message(format!(
            "credential \"{name}\" is not in the index; run `subhub list`"
        )));
    }

    let stored = vault_read(name).map_err(|error| {
        Error::Message(format!(
            "credential \"{name}\" is indexed but missing from secure storage: {error}"
        ))
    })?;
    let entry: VaultEntry = serde_json::from_str(&stored).map_err(|error| {
        Error::Message(format!(
            "{error}; refresh it with `subhub add {name} --force`"
        ))
    })?;
    if let Some(expected) = expected_provider
        && expected != entry.provider
    {
        return Err(Error::Message(format!(
            "credential \"{name}\" belongs to {}, not {}",
            entry.provider.name(),
            expected.name()
        )));
    }
    if entry.provider == Provider::Claude {
        let credential = serde_json::to_string(&entry.credential)?;
        validate_credential(&credential)?;
        write_oauth_account(&entry.oauth_account)?;
    }

    index.activate(name, entry.provider);
    save_index(path, index)?;
    println!("Active credential set to \"{name}\".");
    match service::select_gateway_account(name) {
        Ok(true) => println!("Running gateway switched to \"{name}\"."),
        Ok(false) => {}
        Err(error) => eprintln!("warning: running gateway was not switched: {error}"),
    }
    Ok(())
}

fn require_success(status: ExitStatus, message: &str) -> Result<()> {
    if status.success() {
        Ok(())
    } else {
        Err(Error::Message(format!("{message} (status {status})")))
    }
}

#[derive(Serialize)]
struct AuditResult {
    name: String,
    provider: Provider,
    status: &'static str,
    usage: Option<AuditUsage>,
    error: Option<String>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum AuditUsage {
    Claude(usage::UsageSnapshot),
    Codex(codex::UsageSnapshot),
}

async fn audit(credentials: Vec<crate::provider::StoredCredential>, json: bool) -> Result<()> {
    if credentials.is_empty() {
        return Err(Error::Message(
            "no credentials saved; run `subhub add <name>`".into(),
        ));
    }
    let usage_client = usage::UsageClient::new(reqwest::Client::new(), claude_version().as_deref());
    let client = reqwest::Client::new();
    let mut results = Vec::with_capacity(credentials.len());
    for credential in credentials {
        let provider = credential.provider;
        let result = if provider == Provider::Codex {
            match credential.account_id.as_deref() {
                Some(account) => {
                    match codex::fetch_usage(&client, &credential.access_token, account).await {
                        Ok(usage) => AuditResult {
                            name: credential.name,
                            provider,
                            status: "available",
                            usage: Some(AuditUsage::Codex(usage)),
                            error: None,
                        },
                        Err(error) => AuditResult {
                            name: credential.name,
                            provider,
                            status: "unavailable",
                            usage: None,
                            error: Some(error.to_string()),
                        },
                    }
                }
                None => AuditResult {
                    name: credential.name,
                    provider,
                    status: "unavailable",
                    usage: None,
                    error: Some("missing Codex account id".into()),
                },
            }
        } else if !credential.scopes.is_empty()
            && !credential
                .scopes
                .iter()
                .any(|scope| scope == "user:profile")
        {
            AuditResult {
                name: credential.name,
                provider,
                status: "unavailable",
                usage: None,
                error: Some("OAuth token lacks user:profile scope".into()),
            }
        } else {
            match usage_client.fetch(&credential.access_token).await {
                Ok(usage) => AuditResult {
                    name: credential.name,
                    provider,
                    status: "available",
                    usage: Some(AuditUsage::Claude(usage)),
                    error: None,
                },
                Err(error) => AuditResult {
                    name: credential.name,
                    provider,
                    status: "unavailable",
                    usage: None,
                    error: Some(error.to_string()),
                },
            }
        };
        results.push(result);
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&results)?);
    } else {
        for result in results {
            match result.usage {
                Some(AuditUsage::Claude(usage)) => println!(
                    "{} [claude]: 5h {}, 7d {}",
                    result.name,
                    format_window(usage.five_hour.as_ref()),
                    format_window(usage.seven_day.as_ref())
                ),
                Some(AuditUsage::Codex(usage)) => println!(
                    "{} [codex]: primary {:.1}%, secondary {:.1}%",
                    result.name,
                    usage
                        .rate_limit
                        .primary_window
                        .and_then(|w| w.used_percent)
                        .unwrap_or(0.0),
                    usage
                        .rate_limit
                        .secondary_window
                        .and_then(|w| w.used_percent)
                        .unwrap_or(0.0)
                ),
                None => println!(
                    "{}: unavailable ({})",
                    result.name,
                    result.error.unwrap_or_else(|| "unknown error".into())
                ),
            }
        }
    }
    Ok(())
}

fn format_window(window: Option<&usage::UsageWindow>) -> String {
    match window {
        Some(window) => {
            let used = window
                .utilization
                .map(|value| format!("{value:.1}% used"))
                .unwrap_or_else(|| "unknown".into());
            window
                .resets_at
                .as_ref()
                .map(|reset| format!("{used}, resets {}", format_reset_time(reset)))
                .unwrap_or(used)
        }
        None => "not reported".into(),
    }
}

fn format_reset_time(reset: &str) -> String {
    let Ok(parsed) = DateTime::parse_from_rfc3339(reset) else {
        return reset.to_owned();
    };
    let reset_local = parsed.with_timezone(&Local);
    let today = Local::now().date_naive();
    let reset_date = reset_local.date_naive();
    let time = reset_local.format("%-I:%M %p");

    if reset_date == today {
        format!("today at {time}")
    } else if reset_date == today.succ_opt().unwrap_or(today) {
        format!("tomorrow at {time}")
    } else {
        reset_local.format("%a %b %-d at %-I:%M %p").to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_flag_is_exposed_for_add_and_conflicts_with_device_auth() {
        let cli = Cli::try_parse_from(["subhub", "add", "personal", "--capture"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Add {
                capture: true,
                device_auth: false,
                ..
            }
        ));
        assert!(
            Cli::try_parse_from(["subhub", "add", "personal", "--capture", "--device-auth"])
                .is_err()
        );
    }

    #[test]
    fn reinstall_transport_is_an_optional_override() {
        let cli = Cli::try_parse_from(["subhub", "gateway", "reinstall"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Gateway {
                command: GatewayCommands::Reinstall { transport: None }
            }
        ));

        let cli =
            Cli::try_parse_from(["subhub", "gateway", "reinstall", "--transport", "iron"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Gateway {
                command: GatewayCommands::Reinstall {
                    transport: Some(GatewayTransportArg::Iron)
                }
            }
        ));
    }

    #[test]
    fn reset_timestamp_is_human_readable() {
        let formatted = format_reset_time("2030-08-01T13:00:00.118915+00:00");
        assert!(!formatted.contains("T13:00"));
        assert!(formatted.contains(" at "));
        assert!(formatted.ends_with("AM") || formatted.ends_with("PM"));
        assert_eq!(format_reset_time("unknown"), "unknown");
    }
}
