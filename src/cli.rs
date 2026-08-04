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
use crate::credentials::stored_credentials;
use crate::credentials::vault::{
    ACTIVE_SERVICE, VAULT_SERVICE, VaultEntry, credential_read, credential_write, current_user,
    validate_credential, vault_read,
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
        /// Use device-code authentication for the Codex login (for remote or headless machines)
        #[arg(long, short = 'd')]
        device_auth: bool,
    },
    /// List saved credential names and mark the active one
    List,
    /// Make a saved credential active in Claude Code
    Set {
        /// Friendly name of the saved credential
        name: String,
        /// Require the credential to belong to this provider
        #[arg(long, value_enum)]
        provider: Option<ProviderArg>,
    },
    /// Query subscription usage for every saved credential
    Audit {
        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,
    },
    /// Manage the local Anthropic credential gateway
    Gateway {
        #[command(subcommand)]
        command: GatewayCommands,
    },
}

#[derive(Subcommand)]
enum GatewayCommands {
    /// Run the local credential-routing Anthropic proxy
    Serve {
        /// Loopback address to listen on
        #[arg(long, default_value = "127.0.0.1:7842")]
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
        /// Internal LaunchAgent mode
        #[arg(long, hide = true)]
        background: bool,
    },
    /// Install and start the background gateway
    Install,
    /// Run uninstall then install in a single command, preserving saved credentials
    Reinstall,
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
        provider: Option<ProviderArg>,
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
    /// Internal Claude Code status-line renderer
    #[command(hide = true)]
    Statusline,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ProviderArg {
    Claude,
    Codex,
}

impl From<ProviderArg> for Provider {
    fn from(provider: ProviderArg) -> Self {
        match provider {
            ProviderArg::Claude => Self::Claude,
            ProviderArg::Codex => Self::Codex,
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
            device_auth,
        } => add(&path, &mut index, &name, force, device_auth),
        Commands::List => list(&index),
        Commands::Set { name, provider } => set(&path, &mut index, &name, provider.map(Into::into)),
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
            background,
        } => {
            let credentials = stored_credentials(index)?;
            runtime()?.block_on(gateway::serve(gateway::ServeOptions {
                listen,
                client_token,
                reserve_percent,
                audit_interval,
                background,
                initial_selected: index.active_names(),
                credentials,
            }))
        }
        GatewayCommands::Install => service::install(),
        GatewayCommands::Reinstall => service::reinstall(),
        GatewayCommands::Uninstall { purge } => service::uninstall(purge),
        GatewayCommands::Start => service::start(),
        GatewayCommands::Stop => service::stop(),
        GatewayCommands::Restart => service::restart(),
        GatewayCommands::Status { provider } => service::status(provider.map(Into::into)),
        GatewayCommands::Logs { lines } => service::logs(lines),
        GatewayCommands::Doctor => service::doctor(),
        GatewayCommands::AuthToken => {
            println!("{}", service::read_gateway_token()?);
            Ok(())
        }
        GatewayCommands::Statusline => service::statusline(),
    }
}

fn add(path: &Path, index: &mut Index, name: &str, force: bool, device_auth: bool) -> Result<()> {
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
        return add_codex(path, index, name, device_auth);
    }
    if device_auth {
        return Err(Error::Message(
            "--device-auth only applies to Codex logins; choose subscription type 2".into(),
        ));
    }
    println!("Opening Claude Code login for credential \"{name}\"...");
    let oauth_scopes = requested_oauth_scopes(env::var_os(CLAUDE_OAUTH_SCOPES_ENV).as_deref());
    let status = Command::new("claude")
        .args(["auth", "login", "--claudeai"])
        .env(CLAUDE_OAUTH_SCOPES_ENV, oauth_scopes)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|error| Error::Message(format!("could not run `claude auth login`: {error}")))?;

    require_success(status, "`claude auth login` failed")?;

    let account = current_user()?;
    let credential = credential_read(ACTIVE_SERVICE, &account).map_err(|error| {
        Error::Message(format!(
            "login completed, but the Claude Code credential could not be read: {error}"
        ))
    })?;
    validate_credential(&credential)?;
    validate_required_scopes(&credential)?;
    let credential_value: Value = serde_json::from_str(&credential)?;
    let oauth_account = read_oauth_account()?;
    let vault_entry = serde_json::to_string(&VaultEntry {
        provider: Provider::Claude,
        credential: credential_value,
        oauth_account,
    })?;
    credential_write(VAULT_SERVICE, name, &vault_entry)?;

    index.add(name, Provider::Claude);
    save_index(path, index)?;
    println!("Saved \"{name}\" and made it active.");
    notify_gateway_reload();
    Ok(())
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
        credential_write(ACTIVE_SERVICE, &current_user()?, &credential)?;
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
    fn reset_timestamp_is_human_readable() {
        let formatted = format_reset_time("2030-08-01T13:00:00.118915+00:00");
        assert!(!formatted.contains("T13:00"));
        assert!(formatted.contains(" at "));
        assert!(formatted.ends_with("AM") || formatted.ends_with("PM"));
        assert_eq!(format_reset_time("unknown"), "unknown");
    }
}
