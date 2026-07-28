use chrono::{DateTime, Local};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::env;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

mod proxy;
pub mod usage;

const ACTIVE_SERVICE: &str = "Claude Code-credentials";
const VAULT_SERVICE: &str = "subhub-credentials";
const LEGACY_VAULT_SERVICE: &str = "sub-manager-credentials";
const CLAUDE_OAUTH_SCOPES_ENV: &str = "CLAUDE_CODE_OAUTH_SCOPES";
const REQUESTED_OAUTH_SCOPES: [&str; 4] = [
    "user:inference",
    "user:profile",
    "user:sessions:claude_code",
    "user:mcp_servers",
];
const REQUIRED_OAUTH_SCOPES: [&str; 2] = ["user:inference", "user:profile"];

pub type Result<T> = std::result::Result<T, AppError>;

#[derive(Debug)]
pub struct AppError(String);

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for AppError {}

impl From<io::Error> for AppError {
    fn from(value: io::Error) -> Self {
        Self(value.to_string())
    }
}

impl From<serde_json::Error> for AppError {
    fn from(value: serde_json::Error) -> Self {
        Self(value.to_string())
    }
}

#[derive(Parser)]
#[command(
    name = "subhub",
    version,
    about = "Manage named Claude Code subscriptions in the macOS Keychain"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Log in with Claude Code and save the resulting credential under NAME
    Add {
        /// Friendly name for the credential
        name: String,
        /// Replace an existing credential with the same name
        #[arg(long, short)]
        force: bool,
    },
    /// List saved credential names and mark the active one
    List,
    /// Make a saved credential active in Claude Code
    Set {
        /// Friendly name of the saved credential
        name: String,
    },
    /// Query subscription usage for every saved credential
    Audit {
        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,
    },
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
    },
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct Index {
    version: u8,
    active: Option<String>,
    credentials: Vec<String>,
}

#[derive(Deserialize, Serialize)]
struct VaultEntry {
    credential: Value,
    #[serde(rename = "oauthAccount")]
    oauth_account: Value,
}

#[derive(Clone, Debug)]
pub(crate) struct StoredCredential {
    pub name: String,
    pub access_token: String,
    pub scopes: Vec<String>,
}

impl Index {
    fn new() -> Self {
        Self {
            version: 1,
            active: None,
            credentials: Vec::new(),
        }
    }

    fn contains(&self, name: &str) -> bool {
        self.credentials.iter().any(|item| item == name)
    }

    fn add(&mut self, name: &str) {
        if !self.contains(name) {
            self.credentials.push(name.to_owned());
            self.credentials.sort();
        }
        self.active = Some(name.to_owned());
    }
}

pub fn run() -> Result<()> {
    if !cfg!(target_os = "macos") {
        return Err(AppError("subhub currently requires macOS".into()));
    }

    dispatch(Cli::parse())
}

fn dispatch(cli: Cli) -> Result<()> {
    let path = index_path()?;
    let mut index = load_or_migrate_index(&path, &legacy_index_path()?)?;

    match cli.command {
        Commands::Add { name, force } => add(&path, &mut index, &name, force),
        Commands::List => list(&index),
        Commands::Set { name } => set(&path, &mut index, &name),
        Commands::Audit { json } => {
            let credentials = stored_credentials(&index)?;
            runtime()?.block_on(audit(credentials, json))
        }
        Commands::Serve {
            listen,
            client_token,
            reserve_percent,
            audit_interval,
        } => {
            let credentials = stored_credentials(&index)?;
            runtime()?.block_on(proxy::serve(proxy::ServeOptions {
                listen,
                client_token,
                reserve_percent,
                audit_interval,
                credentials,
            }))
        }
    }
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Runtime::new()
        .map_err(|error| AppError(format!("could not start async runtime: {error}")))
}

fn add(path: &Path, index: &mut Index, name: &str, force: bool) -> Result<()> {
    validate_name(name)?;
    if index.contains(name) && !force {
        return Err(AppError(format!(
            "credential \"{name}\" already exists; use --force to replace it"
        )));
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
        .map_err(|error| AppError(format!("could not run `claude auth login`: {error}")))?;

    require_success(status, "`claude auth login` failed")?;

    let account = current_user()?;
    let credential = keychain_read(ACTIVE_SERVICE, &account).map_err(|error| {
        AppError(format!(
            "login completed, but the Claude Code credential could not be read: {error}"
        ))
    })?;
    validate_credential(&credential)?;
    validate_required_scopes(&credential)?;
    let credential_value: Value = serde_json::from_str(&credential)?;
    let oauth_account = read_oauth_account()?;
    let vault_entry = serde_json::to_string(&VaultEntry {
        credential: credential_value,
        oauth_account,
    })?;
    keychain_write(VAULT_SERVICE, name, &vault_entry)?;

    index.add(name);
    save_index(path, index)?;
    println!("Saved \"{name}\" and made it active.");
    Ok(())
}

fn list(index: &Index) -> Result<()> {
    if index.credentials.is_empty() {
        println!("No credentials saved. Run `subhub add <name>`.");
        return Ok(());
    }

    for name in &index.credentials {
        let marker = if index.active.as_deref() == Some(name) {
            "*"
        } else {
            " "
        };
        println!("{marker} {name}");
    }
    Ok(())
}

fn set(path: &Path, index: &mut Index, name: &str) -> Result<()> {
    validate_name(name)?;
    if !index.contains(name) {
        return Err(AppError(format!(
            "credential \"{name}\" is not in the index; run `subhub list`"
        )));
    }

    let stored = vault_read(name).map_err(|error| {
        AppError(format!(
            "credential \"{name}\" is indexed but missing from Keychain: {error}"
        ))
    })?;
    let (credential, oauth_account) = decode_vault_entry(&stored).map_err(|error| {
        AppError(format!(
            "{error}; refresh it with `subhub add {name} --force`"
        ))
    })?;
    validate_credential(&credential)?;
    keychain_write(ACTIVE_SERVICE, &current_user()?, &credential)?;
    write_oauth_account(&oauth_account)?;

    index.active = Some(name.to_owned());
    save_index(path, index)?;
    println!("Active credential set to \"{name}\".");
    Ok(())
}

fn decode_vault_entry(stored: &str) -> Result<(String, Value)> {
    let parsed: Value = serde_json::from_str(stored)
        .map_err(|_| AppError("saved Keychain entry is not valid JSON".into()))?;
    let entry: VaultEntry = serde_json::from_value(parsed)
        .map_err(|_| AppError("saved credential predates account metadata support".into()))?;
    Ok((
        serde_json::to_string(&entry.credential)?,
        entry.oauth_account,
    ))
}

fn stored_credentials(index: &Index) -> Result<Vec<StoredCredential>> {
    let mut credentials = Vec::with_capacity(index.credentials.len());
    for name in &index.credentials {
        let stored = vault_read(name).map_err(|error| {
            AppError(format!(
                "credential \"{name}\" is missing from Keychain: {error}"
            ))
        })?;
        let parsed: Value = serde_json::from_str(&stored)
            .map_err(|_| AppError(format!("credential \"{name}\" is not valid JSON")))?;
        let oauth = parsed
            .get("credential")
            .and_then(|value| value.get("claudeAiOauth"))
            .and_then(Value::as_object)
            .ok_or_else(|| AppError(format!("credential \"{name}\" has no claudeAiOauth")))?;
        let access_token = oauth
            .get("accessToken")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| AppError(format!("credential \"{name}\" has no access token")))?;
        let scopes = oauth
            .get("scopes")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        credentials.push(StoredCredential {
            name: name.clone(),
            access_token: access_token.to_owned(),
            scopes,
        });
    }
    Ok(credentials)
}

#[derive(Serialize)]
struct AuditResult {
    name: String,
    status: &'static str,
    usage: Option<usage::UsageSnapshot>,
    error: Option<String>,
}

async fn audit(credentials: Vec<StoredCredential>, json: bool) -> Result<()> {
    if credentials.is_empty() {
        return Err(AppError(
            "no credentials saved; run `subhub add <name>`".into(),
        ));
    }
    let usage_client = usage::UsageClient::new(reqwest::Client::new(), claude_version().as_deref());
    let mut results = Vec::with_capacity(credentials.len());
    for credential in credentials {
        let result = if !credential.scopes.is_empty()
            && !credential
                .scopes
                .iter()
                .any(|scope| scope == "user:profile")
        {
            AuditResult {
                name: credential.name,
                status: "unavailable",
                usage: None,
                error: Some("OAuth token lacks user:profile scope".into()),
            }
        } else {
            match usage_client.fetch(&credential.access_token).await {
                Ok(usage) => AuditResult {
                    name: credential.name,
                    status: "available",
                    usage: Some(usage),
                    error: None,
                },
                Err(error) => AuditResult {
                    name: credential.name,
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
                Some(usage) => println!(
                    "{}: 5h {}, 7d {}",
                    result.name,
                    format_window(usage.five_hour.as_ref()),
                    format_window(usage.seven_day.as_ref())
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

fn claude_version() -> Option<String> {
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

fn claude_config_path() -> Result<PathBuf> {
    if let Some(directory) = env::var_os("CLAUDE_CONFIG_DIR") {
        return Ok(PathBuf::from(directory).join(".claude.json"));
    }
    env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".claude.json"))
        .ok_or_else(|| AppError("HOME is not set".into()))
}

fn read_oauth_account() -> Result<Value> {
    let path = claude_config_path()?;
    let contents = fs::read_to_string(&path)
        .map_err(|error| AppError(format!("could not read {}: {error}", path.display())))?;
    let config: Value = serde_json::from_str(&contents)
        .map_err(|error| AppError(format!("invalid {}: {error}", path.display())))?;
    config.get("oauthAccount").cloned().ok_or_else(|| {
        AppError(format!(
            "Claude login did not write oauthAccount metadata to {}",
            path.display()
        ))
    })
}

fn write_oauth_account(oauth_account: &Value) -> Result<()> {
    let path = claude_config_path()?;
    let contents = fs::read_to_string(&path)
        .map_err(|error| AppError(format!("could not read {}: {error}", path.display())))?;
    let mut config: Value = serde_json::from_str(&contents)
        .map_err(|error| AppError(format!("invalid {}: {error}", path.display())))?;
    let object = config
        .as_object_mut()
        .ok_or_else(|| AppError(format!("{} is not a JSON object", path.display())))?;
    object.insert("oauthAccount".into(), oauth_account.clone());
    save_json_file(&path, &config)
}

fn validate_name(name: &str) -> Result<()> {
    if name.trim().is_empty() {
        return Err(AppError("credential name cannot be empty".into()));
    }
    if name.chars().any(char::is_control) {
        return Err(AppError(
            "credential name cannot contain control characters".into(),
        ));
    }
    Ok(())
}

fn validate_credential(raw: &str) -> Result<()> {
    let parsed: Value = serde_json::from_str(raw)
        .map_err(|_| AppError("Keychain credential is not valid JSON".into()))?;
    let oauth = parsed
        .get("claudeAiOauth")
        .and_then(Value::as_object)
        .ok_or_else(|| AppError("Keychain credential has no claudeAiOauth object".into()))?;
    for key in ["accessToken", "refreshToken"] {
        if oauth.get(key).and_then(Value::as_str).is_none() {
            return Err(AppError(format!(
                "Keychain credential has no valid claudeAiOauth.{key}"
            )));
        }
    }
    Ok(())
}

fn requested_oauth_scopes(existing: Option<&std::ffi::OsStr>) -> String {
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

fn validate_required_scopes(raw: &str) -> Result<()> {
    let parsed: Value = serde_json::from_str(raw)
        .map_err(|_| AppError("Keychain credential is not valid JSON".into()))?;
    let scopes = parsed
        .get("claudeAiOauth")
        .and_then(|oauth| oauth.get("scopes"))
        .and_then(Value::as_array)
        .ok_or_else(|| {
            AppError(
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
        Err(AppError(format!(
            "Claude login did not grant required OAuth scope(s): {}; \
             update Claude Code and retry `subhub add`",
            missing.join(", ")
        )))
    }
}

fn keychain_read(service: &str, account: &str) -> Result<String> {
    let output = Command::new("security")
        .args(["find-generic-password", "-s", service, "-a", account, "-w"])
        .output()
        .map_err(|error| AppError(format!("could not run `security`: {error}")))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(AppError(if detail.is_empty() {
            "Keychain item was not found".into()
        } else {
            detail
        }));
    }
    String::from_utf8(output.stdout)
        .map(|value| value.trim_end_matches(['\r', '\n']).to_owned())
        .map_err(|_| AppError("Keychain returned a non-UTF-8 credential".into()))
}

fn keychain_write(service: &str, account: &str, credential: &str) -> Result<()> {
    let output = Command::new("security")
        .args([
            "add-generic-password",
            "-s",
            service,
            "-a",
            account,
            "-w",
            credential,
            "-U",
        ])
        .output()
        .map_err(|error| AppError(format!("could not run `security`: {error}")))?;
    if output.status.success() {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    Err(AppError(format!("could not update Keychain: {detail}")))
}

fn vault_read(name: &str) -> Result<String> {
    match keychain_read(VAULT_SERVICE, name) {
        Ok(stored) => Ok(stored),
        Err(current_error) => match keychain_read(LEGACY_VAULT_SERVICE, name) {
            Ok(stored) => {
                keychain_write(VAULT_SERVICE, name, &stored)?;
                Ok(stored)
            }
            Err(_) => Err(current_error),
        },
    }
}

fn require_success(status: ExitStatus, message: &str) -> Result<()> {
    if status.success() {
        Ok(())
    } else {
        Err(AppError(format!("{message} (status {status})")))
    }
}

fn current_user() -> Result<String> {
    env::var("USER")
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AppError("USER is not set".into()))
}

fn config_base_path() -> Result<PathBuf> {
    env::var_os("XDG_CONFIG")
        .or_else(|| env::var_os("XDG_CONFIG_HOME"))
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .ok_or_else(|| AppError("XDG_CONFIG, XDG_CONFIG_HOME, and HOME are not set".into()))
}

fn index_path() -> Result<PathBuf> {
    Ok(config_base_path()?.join(".subhub").join("index.json"))
}

fn legacy_index_path() -> Result<PathBuf> {
    Ok(config_base_path()?.join(".sub-manager").join("index.json"))
}

fn load_or_migrate_index(path: &Path, legacy_path: &Path) -> Result<Index> {
    if path.exists() || !legacy_path.exists() {
        return load_index(path);
    }
    let index = load_index(legacy_path)?;
    save_index(path, &index)?;
    Ok(index)
}

fn load_index(path: &Path) -> Result<Index> {
    if !path.exists() {
        return Ok(Index::new());
    }
    let contents = fs::read_to_string(path)
        .map_err(|error| AppError(format!("could not read {}: {error}", path.display())))?;
    let index: Index = serde_json::from_str(&contents)
        .map_err(|error| AppError(format!("invalid index {}: {error}", path.display())))?;
    if index.version != 1 {
        return Err(AppError(format!(
            "unsupported index version {} in {}",
            index.version,
            path.display()
        )));
    }
    Ok(index)
}

fn save_index(path: &Path, index: &Index) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| AppError("index path has no parent directory".into()))?;
    fs::create_dir_all(parent)?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;

    save_json_file(path, index)
}

fn save_json_file(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| AppError("JSON path has no parent directory".into()))?;
    fs::create_dir_all(parent)?;
    let temporary = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(value)?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true).mode(0o600);
    let mut file = options.open(&temporary)?;
    io::Write::write_all(&mut file, &bytes)?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn adding_names_is_sorted_and_marks_active() {
        let mut index = Index::new();
        index.add("work");
        index.add("personal");
        assert_eq!(index.credentials, ["personal", "work"]);
        assert_eq!(index.active.as_deref(), Some("personal"));
    }

    #[test]
    fn validates_expected_claude_credential() {
        let raw = r#"{"claudeAiOauth":{"accessToken":"a","refreshToken":"r"}}"#;
        assert!(validate_credential(raw).is_ok());
        assert!(validate_credential("{}").is_err());
    }

    #[test]
    fn requested_scopes_preserve_extras_and_add_required_scopes() {
        let scopes =
            requested_oauth_scopes(Some(std::ffi::OsStr::new("custom:scope user:profile")));
        assert_eq!(
            scopes,
            "custom:scope user:profile user:inference user:sessions:claude_code user:mcp_servers"
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

    #[test]
    fn reset_timestamp_is_human_readable() {
        let formatted = format_reset_time("2030-08-01T13:00:00.118915+00:00");
        assert!(!formatted.contains("T13:00"));
        assert!(formatted.contains(" at "));
        assert!(formatted.ends_with("AM") || formatted.ends_with("PM"));
        assert_eq!(format_reset_time("unknown"), "unknown");
    }

    #[test]
    fn vault_entry_round_trips_credential_and_account() {
        let entry = VaultEntry {
            credential: serde_json::json!({
                "claudeAiOauth": {"accessToken": "a", "refreshToken": "r"}
            }),
            oauth_account: serde_json::json!({"emailAddress": "person@example.com"}),
        };
        let stored = serde_json::to_string(&entry).unwrap();
        let (credential, account) = decode_vault_entry(&stored).unwrap();
        assert!(validate_credential(&credential).is_ok());
        assert_eq!(account["emailAddress"], "person@example.com");
    }

    #[test]
    fn legacy_vault_entry_requires_refresh() {
        let raw = r#"{"claudeAiOauth":{"accessToken":"a","refreshToken":"r"}}"#;
        assert!(decode_vault_entry(raw).is_err());
    }

    #[test]
    fn index_write_never_stores_sensitive_credential_data() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory =
            env::temp_dir().join(format!("subhub-index-test-{}-{unique}", std::process::id()));
        let path = directory.join("index.json");

        let mut index = Index::new();
        index.add("personal");
        save_index(&path, &index).unwrap();

        let written = fs::read_to_string(&path).unwrap();
        let parsed: Value = serde_json::from_str(&written).unwrap();
        assert_eq!(
            parsed,
            serde_json::json!({
                "version": 1,
                "active": "personal",
                "credentials": ["personal"]
            })
        );
        for sensitive_field in [
            "accessToken",
            "refreshToken",
            "claudeAiOauth",
            "oauthAccount",
            "emailAddress",
        ] {
            assert!(
                !written.contains(sensitive_field),
                "index leaked sensitive field {sensitive_field}"
            );
        }

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn legacy_index_is_copied_to_subhub_location() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = env::temp_dir().join(format!(
            "subhub-migration-test-{}-{unique}",
            std::process::id()
        ));
        let legacy_path = directory.join("legacy").join("index.json");
        let subhub_path = directory.join("subhub").join("index.json");
        let mut legacy = Index::new();
        legacy.add("personal");
        save_index(&legacy_path, &legacy).unwrap();

        let migrated = load_or_migrate_index(&subhub_path, &legacy_path).unwrap();

        assert_eq!(migrated.credentials, ["personal"]);
        assert_eq!(migrated.active.as_deref(), Some("personal"));
        assert!(subhub_path.exists());
        fs::remove_dir_all(directory).unwrap();
    }
}
