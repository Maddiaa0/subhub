use crate::{
    AppError, Result, VAULT_SERVICE, credential_delete, credential_read, credential_write,
    index_path, load_index, save_json_file,
};
use rand::distr::{Alphanumeric, SampleString};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::env;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use toml_edit::{DocumentMut, Item, Table, value};

const GATEWAY_SERVICE: &str = "subhub-gateway";
const GATEWAY_TOKEN_ACCOUNT: &str = "local-client-token";
#[cfg(target_os = "macos")]
const LAUNCH_AGENT_LABEL: &str = "com.subhub.gateway";
#[cfg(target_os = "linux")]
const SYSTEMD_UNIT_NAME: &str = "subhub-gateway.service";
const BASE_URL: &str = "http://127.0.0.1:7842";

#[derive(Debug, Deserialize, Serialize)]
struct PreviousValue {
    present: bool,
    value: Option<Value>,
}

#[derive(Debug, Deserialize, Serialize)]
struct InstallState {
    version: u8,
    binary_path: PathBuf,
    previous_base_url: PreviousValue,
    previous_api_key_helper: PreviousValue,
    #[serde(default)]
    previous_status_line: Option<PreviousValue>,
    #[serde(default)]
    previous_codex_config: Option<String>,
}

pub(crate) fn read_gateway_token() -> Result<String> {
    credential_read(GATEWAY_SERVICE, GATEWAY_TOKEN_ACCOUNT)
}

pub(crate) fn select_gateway_account(name: &str) -> Result<bool> {
    let Ok(token) = read_gateway_token() else {
        return Ok(false);
    };
    crate::runtime()?.block_on(async move {
        let response = match reqwest::Client::new()
            .post(format!("{BASE_URL}/_subhub/select"))
            .bearer_auth(token)
            .json(&serde_json::json!({"name": name}))
            .timeout(std::time::Duration::from_secs(2))
            .send()
            .await
        {
            Ok(response) => response,
            Err(_) => return Ok(false),
        };
        if response.status().is_success() {
            Ok(true)
        } else {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            Err(AppError(format!(
                "gateway returned {status}: {}",
                body.chars().take(200).collect::<String>()
            )))
        }
    })
}

/// Ask a running gateway to re-read the vault. Returns Ok(false) when no
/// gateway is reachable, and an error when one is reachable but refuses.
pub(crate) fn reload_gateway_accounts() -> Result<bool> {
    let Ok(token) = read_gateway_token() else {
        return Ok(false);
    };
    crate::runtime()?.block_on(async move {
        let response = match reqwest::Client::new()
            .post(format!("{BASE_URL}/_subhub/reload"))
            .bearer_auth(token)
            // The gateway audits every credential before replying.
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await
        {
            Ok(response) => response,
            Err(_) => return Ok(false),
        };
        if response.status().is_success() {
            Ok(true)
        } else {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            Err(AppError(format!(
                "gateway returned {status}: {}",
                body.chars().take(200).collect::<String>()
            )))
        }
    })
}

fn ensure_gateway_token() -> Result<String> {
    if let Ok(token) = read_gateway_token()
        && !token.is_empty()
    {
        return Ok(token);
    }
    let token = Alphanumeric.sample_string(&mut rand::rng(), 48);
    credential_write(GATEWAY_SERVICE, GATEWAY_TOKEN_ACCOUNT, &token)?;
    Ok(token)
}

pub(crate) fn install() -> Result<()> {
    let binary_path = env::current_exe()
        .map_err(|error| AppError(format!("could not resolve the Subhub executable: {error}")))?;
    let settings_path = claude_settings_path()?;
    let state_path = install_state_path()?;
    let mut settings = read_json_object(&settings_path)?;
    ensure_no_conflicting_claude_credentials(&settings)?;
    let mut state = if state_path.exists() {
        read_install_state(&state_path)?
    } else {
        InstallState {
            version: 1,
            binary_path: binary_path.clone(),
            previous_base_url: get_nested(&settings, &["env", "ANTHROPIC_BASE_URL"]),
            previous_api_key_helper: get_nested(&settings, &["apiKeyHelper"]),
            previous_status_line: Some(get_nested(&settings, &["statusLine"])),
            previous_codex_config: Some(read_codex_config()?),
        }
    };
    if state.previous_codex_config.is_none() {
        state.previous_codex_config = Some(read_codex_config()?);
    }
    let previous_status_line = state
        .previous_status_line
        .or_else(|| Some(get_nested(&settings, &["statusLine"])));

    ensure_gateway_token()?;
    set_nested(
        &mut settings,
        &["env", "ANTHROPIC_BASE_URL"],
        Value::String(BASE_URL.into()),
    )?;
    set_nested(
        &mut settings,
        &["apiKeyHelper"],
        Value::String(auth_helper_path()?.to_string_lossy().into_owned()),
    )?;
    set_nested(
        &mut settings,
        &["statusLine"],
        managed_status_line(
            previous_status_line
                .as_ref()
                .and_then(|previous| previous.value.as_ref()),
        ),
    )?;

    let current_state = InstallState {
        binary_path: binary_path.clone(),
        previous_status_line,
        ..state
    };
    save_json_file(&state_path, &current_state)?;
    save_json_file(&settings_path, &settings)?;
    install_codex_config()?;
    write_auth_helper(&auth_helper_path()?, &binary_path)?;
    write_statusline_helper(&statusline_helper_path()?, &binary_path)?;
    let agent_path = background_service_path()?;
    write_background_service(&agent_path, &binary_path)?;
    restart()?;

    println!("Subhub gateway installed and running.");
    println!("Claude Code will use {BASE_URL} automatically.");
    println!("Codex CLI will use {BASE_URL}/openai automatically.");
    Ok(())
}

pub(crate) fn uninstall(purge: bool) -> Result<()> {
    stop()?;
    disable_background_service()?;
    let state_path = install_state_path()?;
    if state_path.exists() {
        let state = read_install_state(&state_path)?;
        restore_claude_settings(&state)?;
        restore_codex_config(&state)?;
        fs::remove_file(&state_path).map_err(|error| {
            AppError(format!(
                "could not remove {}: {error}",
                state_path.display()
            ))
        })?;
    }
    let agent_path = background_service_path()?;
    if agent_path.exists() {
        fs::remove_file(&agent_path).map_err(|error| {
            AppError(format!(
                "could not remove {}: {error}",
                agent_path.display()
            ))
        })?;
    }
    refresh_background_services()?;
    let helper_path = auth_helper_path()?;
    if helper_path.exists() {
        fs::remove_file(&helper_path).map_err(|error| {
            AppError(format!(
                "could not remove {}: {error}",
                helper_path.display()
            ))
        })?;
    }
    let statusline_path = statusline_helper_path()?;
    if statusline_path.exists() {
        fs::remove_file(&statusline_path).map_err(|error| {
            AppError(format!(
                "could not remove {}: {error}",
                statusline_path.display()
            ))
        })?;
    }

    if purge {
        purge_data()?;
        println!("Subhub gateway uninstalled and saved Subhub data removed.");
    } else {
        println!("Subhub gateway uninstalled. Saved credentials were preserved.");
    }
    Ok(())
}

pub(crate) fn reinstall() -> Result<()> {
    uninstall(false)?;
    install()
}

pub(crate) fn start() -> Result<()> {
    let agent_path = background_service_path()?;
    if !agent_path.exists() {
        return Err(AppError(
            "Subhub is not installed; run `subhub gateway install`".into(),
        ));
    }
    if background_service_running() {
        return Ok(());
    }
    start_background_service(&agent_path)
}

pub(crate) fn stop() -> Result<()> {
    if !background_service_running() {
        return Ok(());
    }
    stop_background_service()
}

pub(crate) fn restart() -> Result<()> {
    if background_service_running() {
        restart_background_service()
    } else {
        start()
    }
}

pub(crate) fn status(provider: Option<crate::Provider>) -> Result<()> {
    let installed = background_service_path()?.exists() && install_state_path()?.exists();
    let running = background_service_running();
    println!("Installed: {}", yes_no(installed));
    println!("Running:   {}", yes_no(running));
    println!("Endpoint:  {BASE_URL}");
    println!(
        "Token:     {}",
        if read_gateway_token().is_ok() {
            if cfg!(target_os = "macos") {
                "available in Keychain"
            } else {
                "available in credential store"
            }
        } else {
            "missing"
        }
    );
    if running {
        match fetch_gateway_status() {
            Ok(status) => print_gateway_health(&status, provider),
            Err(error) => println!("Gateway:   not reachable ({error})"),
        }
    }
    if env::var_os("ANTHROPIC_AUTH_TOKEN").is_some() || env::var_os("ANTHROPIC_API_KEY").is_some() {
        println!("Warning: a shell Anthropic credential may override Subhub's apiKeyHelper.");
    }
    Ok(())
}

pub(crate) fn logs(lines: usize) -> Result<()> {
    let records = crate::observability::tail(lines)?;
    if records.is_empty() {
        println!("No gateway events recorded yet.");
    } else {
        for record in records {
            println!("{record}");
        }
    }
    Ok(())
}

pub(crate) fn doctor() -> Result<()> {
    status(None)?;
    let status = fetch_gateway_status();
    match status {
        Ok(status) => {
            let unavailable = status
                .get("credentials")
                .and_then(Value::as_object)
                .into_iter()
                .flatten()
                .filter(|(_, health)| health.get("usage").is_none_or(Value::is_null))
                .count();
            if unavailable == 0 {
                println!("Doctor:    healthy");
            } else {
                println!("Doctor:    {unavailable} credential(s) need attention");
                println!("Next:      subhub gateway logs --lines 20");
            }
        }
        Err(error) => println!("Doctor:    gateway check failed ({error})"),
    }
    Ok(())
}

fn print_gateway_health(status: &Value, provider_filter: Option<crate::Provider>) {
    println!("Gateway:   reachable");
    let selected = status.get("selected").and_then(Value::as_object);
    let credentials = status.get("credentials").and_then(Value::as_object);
    let Some(credentials) = credentials else {
        println!("Credentials: auditing");
        return;
    };
    for (name, health) in credentials {
        let provider = health
            .get("provider")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        if provider_filter.is_some_and(|filter| crate::provider_name(filter) != provider) {
            continue;
        }
        let active = selected
            .and_then(|selected| selected.get(provider))
            .and_then(Value::as_str)
            == Some(name);
        let token = health
            .get("token_state")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let audit = if health.get("usage").is_some_and(|usage| !usage.is_null()) {
            "available"
        } else {
            "unavailable"
        };
        println!(
            "  {name} [{provider}]{}: token {token}, audit {audit}",
            if active { " (selected)" } else { "" }
        );
        if let Some(error) = health.get("error").and_then(Value::as_str) {
            println!("    Last error: {error}");
        }
    }
}

pub(crate) fn statusline() -> Result<()> {
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .map_err(|error| AppError(format!("could not read status-line input: {error}")))?;
    let previous = previous_statusline_output(&input).unwrap_or_default();
    let subhub = fetch_gateway_status()
        .map(|status| format_statusline_segment(&status))
        .unwrap_or_else(|_| "Subhub: unavailable".into());

    let previous = previous.trim_end();
    if previous.is_empty() {
        println!("{subhub}");
    } else if previous.contains('\n') {
        println!("{previous}");
        println!("{subhub}");
    } else {
        println!("{previous} | {subhub}");
    }
    Ok(())
}

fn fetch_gateway_status() -> Result<Value> {
    let token = read_gateway_token()?;
    crate::runtime()?.block_on(async move {
        let response = reqwest::Client::new()
            .get(format!("{BASE_URL}/_subhub/status"))
            .bearer_auth(token)
            .timeout(std::time::Duration::from_secs(2))
            .send()
            .await
            .map_err(|error| AppError(error.to_string()))?;
        if !response.status().is_success() {
            return Err(AppError(format!("HTTP {}", response.status())));
        }
        response
            .json()
            .await
            .map_err(|error| AppError(error.to_string()))
    })
}

fn format_statusline_segment(status: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(selected) = status.pointer("/selected/claude").and_then(Value::as_str) {
        let usage = status
            .get("credentials")
            .and_then(|credentials| credentials.get(selected))
            .and_then(|credential| credential.get("usage"));
        let five = usage
            .and_then(|usage| usage.get("five_hour"))
            .and_then(|window| window.get("utilization"))
            .and_then(Value::as_f64)
            .or_else(|| {
                usage
                    .and_then(|usage| usage.pointer("/rate_limit/primary_window/used_percent"))
                    .and_then(Value::as_f64)
            });
        let seven = usage
            .and_then(|usage| usage.get("seven_day"))
            .and_then(|window| window.get("utilization"))
            .and_then(Value::as_f64)
            .or_else(|| {
                usage
                    .and_then(|usage| usage.pointer("/rate_limit/secondary_window/used_percent"))
                    .and_then(Value::as_f64)
            });

        parts.push(format!("Subhub: {selected}"));
        if let Some(five) = five {
            parts.push(format!("5h {five:.0}%"));
        }
        if let Some(seven) = seven {
            parts.push(format!("7d {seven:.0}%"));
        }
    }
    if parts.is_empty() {
        "Subhub: auditing".into()
    } else {
        parts.join(" | ")
    }
}

fn previous_statusline_output(input: &str) -> Result<String> {
    let state_path = install_state_path()?;
    if !state_path.exists() {
        return Ok(String::new());
    }
    let state = read_install_state(&state_path)?;
    let Some(command) = state
        .previous_status_line
        .as_ref()
        .and_then(|previous| previous.value.as_ref())
        .and_then(|status_line| status_line.get("command"))
        .and_then(Value::as_str)
    else {
        return Ok(String::new());
    };
    let mut child = Command::new("/bin/sh")
        .args(["-c", command])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| AppError(format!("could not run previous status line: {error}")))?;
    child
        .stdin
        .take()
        .ok_or_else(|| AppError("previous status line has no stdin".into()))?
        .write_all(input.as_bytes())?;
    let output = child
        .wait_with_output()
        .map_err(|error| AppError(format!("previous status line failed: {error}")))?;
    if output.status.success() {
        String::from_utf8(output.stdout)
            .map_err(|_| AppError("previous status line returned non-UTF-8 output".into()))
    } else {
        Ok(String::new())
    }
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn purge_data() -> Result<()> {
    let index_path = index_path()?;
    if index_path.exists() {
        let index = load_index(&index_path)?;
        for name in index.credentials {
            let _ = credential_delete(VAULT_SERVICE, &name);
        }
        fs::remove_file(&index_path).map_err(|error| {
            AppError(format!(
                "could not remove {}: {error}",
                index_path.display()
            ))
        })?;
    }
    let _ = credential_delete(GATEWAY_SERVICE, GATEWAY_TOKEN_ACCOUNT);
    Ok(())
}

fn restore_claude_settings(state: &InstallState) -> Result<()> {
    let path = claude_settings_path()?;
    let mut settings = read_json_object(&path)?;
    let managed_helper = Value::String(auth_helper_path()?.to_string_lossy().into_owned());
    restore_nested_if_managed(
        &mut settings,
        &["env", "ANTHROPIC_BASE_URL"],
        &Value::String(BASE_URL.into()),
        &state.previous_base_url,
    );
    restore_nested_if_managed(
        &mut settings,
        &["apiKeyHelper"],
        &managed_helper,
        &state.previous_api_key_helper,
    );
    if let Some(previous) = &state.previous_status_line {
        restore_nested_if_managed(
            &mut settings,
            &["statusLine"],
            &managed_status_line(previous.value.as_ref()),
            previous,
        );
    }
    remove_empty_env(&mut settings);
    save_json_file(&path, &settings)
}

fn install_state_path() -> Result<PathBuf> {
    Ok(index_path()?
        .parent()
        .expect("index path has a parent")
        .join("install.json"))
}

fn auth_helper_path() -> Result<PathBuf> {
    Ok(index_path()?
        .parent()
        .expect("index path has a parent")
        .join("auth-token"))
}

fn statusline_helper_path() -> Result<PathBuf> {
    Ok(index_path()?
        .parent()
        .expect("index path has a parent")
        .join("statusline"))
}

fn claude_settings_path() -> Result<PathBuf> {
    if let Some(directory) = env::var_os("CLAUDE_CONFIG_DIR") {
        return Ok(PathBuf::from(directory).join("settings.json"));
    }
    env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".claude").join("settings.json"))
        .ok_or_else(|| AppError("HOME is not set".into()))
}

fn codex_config_path() -> Result<PathBuf> {
    env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")))
        .map(|home| home.join("config.toml"))
        .ok_or_else(|| AppError("CODEX_HOME and HOME are not set".into()))
}

fn read_codex_config() -> Result<String> {
    let path = codex_config_path()?;
    if path.exists() {
        fs::read_to_string(&path)
            .map_err(|error| AppError(format!("could not read {}: {error}", path.display())))
    } else {
        Ok(String::new())
    }
}

fn install_codex_config() -> Result<()> {
    let path = codex_config_path()?;
    let mut document = read_codex_config()?
        .parse::<DocumentMut>()
        .map_err(|error| AppError(format!("invalid {}: {error}", path.display())))?;
    document["model_provider"] = value("subhub");
    let mut provider = Table::new();
    provider["name"] = value("Subhub");
    provider["base_url"] = value(format!("{BASE_URL}/openai"));
    provider["wire_api"] = value("responses");
    let mut auth = Table::new();
    auth["command"] = value(auth_helper_path()?.to_string_lossy().into_owned());
    auth["refresh_interval_ms"] = value(0);
    provider["auth"] = Item::Table(auth);
    document["model_providers"]["subhub"] = Item::Table(provider);
    write_private_file(&path, document.to_string().as_bytes())
}

fn restore_codex_config(state: &InstallState) -> Result<()> {
    let Some(previous) = &state.previous_codex_config else {
        return Ok(());
    };
    let path = codex_config_path()?;
    let mut current = read_codex_config()?
        .parse::<DocumentMut>()
        .map_err(|error| AppError(format!("invalid {}: {error}", path.display())))?;
    let prior = previous
        .parse::<DocumentMut>()
        .map_err(|error| AppError(format!("saved Codex config is invalid: {error}")))?;
    if current["model_provider"].as_str() == Some("subhub") {
        current["model_provider"] = prior.get("model_provider").cloned().unwrap_or(Item::None);
    }
    if current["model_providers"]["subhub"]["base_url"].as_str()
        == Some(&format!("{BASE_URL}/openai"))
    {
        current["model_providers"]["subhub"] = prior
            .get("model_providers")
            .and_then(|item| item.get("subhub"))
            .cloned()
            .unwrap_or(Item::None);
    }
    write_private_file(&path, current.to_string().as_bytes())
}

fn write_private_file(path: &Path, contents: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true).mode(0o600);
    let mut file = options.open(path)?;
    file.write_all(contents)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn background_service_path() -> Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| {
            home.join("Library")
                .join("LaunchAgents")
                .join(format!("{LAUNCH_AGENT_LABEL}.plist"))
        })
        .ok_or_else(|| AppError("HOME is not set".into()))
}

#[cfg(target_os = "linux")]
fn background_service_path() -> Result<PathBuf> {
    env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .map(|config| config.join("systemd").join("user").join(SYSTEMD_UNIT_NAME))
        .ok_or_else(|| AppError("XDG_CONFIG_HOME and HOME are not set".into()))
}

#[cfg(target_os = "macos")]
fn write_background_service(path: &Path, binary: &Path) -> Result<()> {
    let binary = xml_escape(
        binary
            .to_str()
            .ok_or_else(|| AppError("Subhub executable path is not UTF-8".into()))?,
    );
    let contents = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LAUNCH_AGENT_LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{binary}</string>
    <string>gateway</string>
    <string>serve</string>
    <string>--background</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>ProcessType</key>
  <string>Background</string>
  <key>ThrottleInterval</key>
  <integer>10</integer>
</dict>
</plist>
"#
    );
    let parent = path
        .parent()
        .ok_or_else(|| AppError("LaunchAgent path has no parent".into()))?;
    fs::create_dir_all(parent)?;
    fs::write(path, contents)
        .map_err(|error| AppError(format!("could not write {}: {error}", path.display())))
}

#[cfg(target_os = "linux")]
fn write_background_service(path: &Path, binary: &Path) -> Result<()> {
    let executable = systemd_quote(binary)?;
    let contents = format!(
        "[Unit]\nDescription=Subhub credential-routing gateway\nAfter=network-online.target\nWants=network-online.target\n\n[Service]\nType=simple\nExecStart={executable} gateway serve --background\nRestart=on-failure\nRestartSec=10\n\n[Install]\nWantedBy=default.target\n"
    );
    write_private_file(path, contents.as_bytes())
}

fn write_auth_helper(path: &Path, binary: &Path) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| AppError("auth helper path has no parent".into()))?;
    fs::create_dir_all(parent)?;
    let contents = format!(
        "#!/bin/sh\nexec {} gateway auth-token\n",
        shell_quote(binary)
    );
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true).mode(0o700);
    let mut file = options.open(path)?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn write_statusline_helper(path: &Path, binary: &Path) -> Result<()> {
    write_executable_helper(
        path,
        &format!(
            "#!/bin/sh\nexec {} gateway statusline\n",
            shell_quote(binary)
        ),
    )
}

fn write_executable_helper(path: &Path, contents: &str) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| AppError("helper path has no parent".into()))?;
    fs::create_dir_all(parent)?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true).mode(0o700);
    let mut file = options.open(path)?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn managed_status_line(previous: Option<&Value>) -> Value {
    let mut managed = Map::new();
    managed.insert("type".into(), Value::String("command".into()));
    managed.insert(
        "command".into(),
        Value::String(statusline_helper_path().map_or_else(
            |_| "subhub gateway statusline".into(),
            |path| shell_quote(&path),
        )),
    );
    if let Some(previous) = previous.and_then(Value::as_object) {
        for key in ["padding", "refreshInterval", "hideVimModeIndicator"] {
            if let Some(value) = previous.get(key) {
                managed.insert(key.into(), value.clone());
            }
        }
    }
    Value::Object(managed)
}

fn ensure_no_conflicting_claude_credentials(settings: &Value) -> Result<()> {
    let conflicts: Vec<&str> = ["ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_API_KEY"]
        .into_iter()
        .filter(|name| get_nested(settings, &["env", name]).present)
        .collect();
    if conflicts.is_empty() {
        Ok(())
    } else {
        Err(AppError(format!(
            "Claude settings define {}; remove {} before installing Subhub because they override apiKeyHelper",
            conflicts.join(" and "),
            if conflicts.len() == 1 { "it" } else { "them" }
        )))
    }
}

fn read_install_state(path: &Path) -> Result<InstallState> {
    let contents = fs::read_to_string(path)
        .map_err(|error| AppError(format!("could not read {}: {error}", path.display())))?;
    let state: InstallState = serde_json::from_str(&contents)
        .map_err(|error| AppError(format!("invalid {}: {error}", path.display())))?;
    if state.version != 1 {
        return Err(AppError(format!(
            "unsupported install state version {}",
            state.version
        )));
    }
    Ok(state)
}

fn read_json_object(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(Value::Object(Map::new()));
    }
    let contents = fs::read_to_string(path)
        .map_err(|error| AppError(format!("could not read {}: {error}", path.display())))?;
    let value: Value = serde_json::from_str(&contents)
        .map_err(|error| AppError(format!("invalid {}: {error}", path.display())))?;
    if value.is_object() {
        Ok(value)
    } else {
        Err(AppError(format!("{} is not a JSON object", path.display())))
    }
}

fn get_nested(root: &Value, path: &[&str]) -> PreviousValue {
    let mut current = root;
    for key in path {
        let Some(next) = current.get(*key) else {
            return PreviousValue {
                present: false,
                value: None,
            };
        };
        current = next;
    }
    PreviousValue {
        present: true,
        value: Some(current.clone()),
    }
}

fn set_nested(root: &mut Value, path: &[&str], value: Value) -> Result<()> {
    let mut current = root;
    for key in &path[..path.len() - 1] {
        let object = current
            .as_object_mut()
            .ok_or_else(|| AppError("Claude settings contain a non-object parent".into()))?;
        current = object
            .entry((*key).to_owned())
            .or_insert_with(|| Value::Object(Map::new()));
    }
    current
        .as_object_mut()
        .ok_or_else(|| AppError("Claude settings contain a non-object parent".into()))?
        .insert(path[path.len() - 1].to_owned(), value);
    Ok(())
}

fn restore_nested_if_managed(
    root: &mut Value,
    path: &[&str],
    managed: &Value,
    previous: &PreviousValue,
) {
    if get_nested(root, path).value.as_ref() != Some(managed) {
        return;
    }
    if previous.present {
        if let Some(value) = previous.value.clone() {
            let _ = set_nested(root, path, value);
        }
    } else {
        remove_nested(root, path);
    }
}

fn remove_nested(root: &mut Value, path: &[&str]) {
    let mut current = root;
    for key in &path[..path.len() - 1] {
        let Some(next) = current.get_mut(*key) else {
            return;
        };
        current = next;
    }
    if let Some(object) = current.as_object_mut() {
        object.remove(path[path.len() - 1]);
    }
}

fn remove_empty_env(settings: &mut Value) {
    let empty = settings
        .get("env")
        .and_then(Value::as_object)
        .is_some_and(Map::is_empty);
    if empty {
        settings.as_object_mut().unwrap().remove("env");
    }
}

fn shell_quote(path: &Path) -> String {
    let raw = path.to_string_lossy();
    format!("'{}'", raw.replace('\'', "'\"'\"'"))
}

#[cfg(any(target_os = "macos", test))]
fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(target_os = "macos")]
fn launch_domain() -> Result<String> {
    Ok(format!("gui/{}", user_id()?))
}

#[cfg(target_os = "macos")]
fn launch_target() -> Result<String> {
    Ok(format!("{}/{LAUNCH_AGENT_LABEL}", launch_domain()?))
}

#[cfg(target_os = "macos")]
fn user_id() -> Result<String> {
    let output = Command::new("/usr/bin/id")
        .arg("-u")
        .output()
        .map_err(|error| AppError(format!("could not run `id -u`: {error}")))?;
    if !output.status.success() {
        return Err(AppError("`id -u` failed".into()));
    }
    String::from_utf8(output.stdout)
        .map(|value| value.trim().to_owned())
        .map_err(|_| AppError("`id -u` returned non-UTF-8 output".into()))
}

#[cfg(target_os = "macos")]
fn launchctl<const N: usize>(arguments: [&str; N]) -> Result<()> {
    let output = Command::new("/bin/launchctl")
        .args(arguments)
        .output()
        .map_err(|error| AppError(format!("could not run launchctl: {error}")))?;
    if output.status.success() {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    Err(AppError(if detail.is_empty() {
        format!("launchctl failed with {}", output.status)
    } else {
        format!("launchctl failed: {detail}")
    }))
}

#[cfg(target_os = "macos")]
fn background_service_running() -> bool {
    launch_target().is_ok_and(|target| launchctl(["print", &target]).is_ok())
}

#[cfg(target_os = "macos")]
fn start_background_service(path: &Path) -> Result<()> {
    launchctl([
        "bootstrap",
        &launch_domain()?,
        path.to_str()
            .ok_or_else(|| AppError("LaunchAgent path is not UTF-8".into()))?,
    ])
}

#[cfg(target_os = "macos")]
fn stop_background_service() -> Result<()> {
    launchctl(["bootout", &launch_target()?])
}

#[cfg(target_os = "macos")]
fn restart_background_service() -> Result<()> {
    launchctl(["kickstart", "-k", &launch_target()?])
}

#[cfg(target_os = "macos")]
fn disable_background_service() -> Result<()> {
    Ok(())
}

#[cfg(target_os = "macos")]
fn refresh_background_services() -> Result<()> {
    Ok(())
}

#[cfg(target_os = "linux")]
fn systemctl(arguments: &[&str]) -> Result<()> {
    let output = Command::new("systemctl")
        .arg("--user")
        .args(arguments)
        .output()
        .map_err(|error| AppError(format!("could not run `systemctl --user`: {error}")))?;
    if output.status.success() {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    Err(AppError(if detail.is_empty() {
        format!("systemctl --user failed with {}", output.status)
    } else {
        format!("systemctl --user failed: {detail}")
    }))
}

#[cfg(target_os = "linux")]
fn background_service_running() -> bool {
    systemctl(&["is-active", "--quiet", SYSTEMD_UNIT_NAME]).is_ok()
}

#[cfg(target_os = "linux")]
fn start_background_service(_path: &Path) -> Result<()> {
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", "--now", SYSTEMD_UNIT_NAME])
}

#[cfg(target_os = "linux")]
fn stop_background_service() -> Result<()> {
    systemctl(&["stop", SYSTEMD_UNIT_NAME])
}

#[cfg(target_os = "linux")]
fn restart_background_service() -> Result<()> {
    systemctl(&["restart", SYSTEMD_UNIT_NAME])
}

#[cfg(target_os = "linux")]
fn disable_background_service() -> Result<()> {
    let _ = systemctl(&["disable", SYSTEMD_UNIT_NAME]);
    Ok(())
}

#[cfg(target_os = "linux")]
fn refresh_background_services() -> Result<()> {
    systemctl(&["daemon-reload"])
}

#[cfg(target_os = "linux")]
fn systemd_quote(path: &Path) -> Result<String> {
    let path = path
        .to_str()
        .ok_or_else(|| AppError("Subhub executable path is not UTF-8".into()))?;
    Ok(format!(
        "\"{}\"",
        path.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn settings_are_changed_and_surgically_restored() {
        let mut settings = serde_json::json!({
            "env": {
                "ANTHROPIC_BASE_URL": "https://old.example",
                "KEEP": "yes"
            },
            "apiKeyHelper": "old-helper",
            "statusLine": {
                "type": "command",
                "command": "old-status"
            },
            "theme": "dark"
        });
        let previous_base = get_nested(&settings, &["env", "ANTHROPIC_BASE_URL"]);
        let previous_helper = get_nested(&settings, &["apiKeyHelper"]);
        let previous_status = get_nested(&settings, &["statusLine"]);
        set_nested(
            &mut settings,
            &["env", "ANTHROPIC_BASE_URL"],
            Value::String(BASE_URL.into()),
        )
        .unwrap();
        set_nested(
            &mut settings,
            &["apiKeyHelper"],
            Value::String("'subhub' auth-token".into()),
        )
        .unwrap();
        set_nested(
            &mut settings,
            &["statusLine"],
            managed_status_line(previous_status.value.as_ref()),
        )
        .unwrap();
        restore_nested_if_managed(
            &mut settings,
            &["env", "ANTHROPIC_BASE_URL"],
            &Value::String(BASE_URL.into()),
            &previous_base,
        );
        restore_nested_if_managed(
            &mut settings,
            &["apiKeyHelper"],
            &Value::String("'subhub' auth-token".into()),
            &previous_helper,
        );
        restore_nested_if_managed(
            &mut settings,
            &["statusLine"],
            &managed_status_line(previous_status.value.as_ref()),
            &previous_status,
        );
        assert_eq!(settings["env"]["ANTHROPIC_BASE_URL"], "https://old.example");
        assert_eq!(settings["env"]["KEEP"], "yes");
        assert_eq!(settings["apiKeyHelper"], "old-helper");
        assert_eq!(settings["statusLine"]["command"], "old-status");
        assert_eq!(settings["theme"], "dark");
    }

    #[test]
    fn user_changes_are_not_overwritten_during_restore() {
        let mut settings = serde_json::json!({
            "env": {"ANTHROPIC_BASE_URL": "https://user-change.example"}
        });
        let previous = PreviousValue {
            present: true,
            value: Some(Value::String("https://old.example".into())),
        };
        restore_nested_if_managed(
            &mut settings,
            &["env", "ANTHROPIC_BASE_URL"],
            &Value::String(BASE_URL.into()),
            &previous,
        );
        assert_eq!(
            settings["env"]["ANTHROPIC_BASE_URL"],
            "https://user-change.example"
        );
    }

    #[test]
    fn quoting_and_xml_escaping_handle_special_paths() {
        assert_eq!(
            shell_quote(Path::new("/tmp/Sub Hub's/bin")),
            "'/tmp/Sub Hub'\"'\"'s/bin'"
        );
        assert_eq!(xml_escape("a&<b>"), "a&amp;&lt;b&gt;");
    }

    #[test]
    fn conflicting_claude_credentials_are_rejected() {
        let settings = serde_json::json!({
            "env": {"ANTHROPIC_API_KEY": "existing-secret"}
        });
        let error = ensure_no_conflicting_claude_credentials(&settings).unwrap_err();
        assert!(error.to_string().contains("ANTHROPIC_API_KEY"));
    }

    #[test]
    fn generated_agent_and_helper_do_not_contain_gateway_token() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = env::temp_dir().join(format!(
            "subhub-lifecycle-test-{}-{unique}",
            std::process::id()
        ));
        let agent = directory.join("agent.plist");
        let helper = directory.join("auth-token");
        let statusline = directory.join("statusline");
        let binary = Path::new("/Applications/Sub Hub/subhub");

        write_background_service(&agent, binary).unwrap();
        write_auth_helper(&helper, binary).unwrap();
        write_statusline_helper(&statusline, binary).unwrap();

        let agent_contents = fs::read_to_string(&agent).unwrap();
        let helper_contents = fs::read_to_string(&helper).unwrap();
        let statusline_contents = fs::read_to_string(&statusline).unwrap();
        #[cfg(target_os = "macos")]
        assert!(agent_contents.contains("<string>gateway</string>"));
        #[cfg(target_os = "macos")]
        assert!(agent_contents.contains("<string>--background</string>"));
        #[cfg(target_os = "linux")]
        assert!(
            agent_contents
                .contains("ExecStart=\"/Applications/Sub Hub/subhub\" gateway serve --background")
        );
        assert!(!agent_contents.contains("local-client-token"));
        assert_eq!(
            helper_contents,
            "#!/bin/sh\nexec '/Applications/Sub Hub/subhub' gateway auth-token\n"
        );
        assert_eq!(
            statusline_contents,
            "#!/bin/sh\nexec '/Applications/Sub Hub/subhub' gateway statusline\n"
        );
        assert_eq!(
            fs::metadata(&helper).unwrap().permissions().mode() & 0o777,
            0o700
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn statusline_segment_shows_selected_account_and_usage() {
        let status = serde_json::json!({
            "selected": {"claude": "personal", "codex": null},
            "credentials": {
                "personal": {
                    "usage": {
                        "five_hour": {"utilization": 12.4},
                        "seven_day": {"utilization": 34.6}
                    }
                }
            }
        });
        assert_eq!(
            format_statusline_segment(&status),
            "Subhub: personal | 5h 12% | 7d 35%"
        );
        assert_eq!(
            format_statusline_segment(&serde_json::json!({"selected": {}})),
            "Subhub: auditing"
        );
    }

    #[test]
    fn statusline_segment_ignores_codex_provider() {
        let status = serde_json::json!({
            "selected": {"claude": "personal", "codex": "work"},
            "credentials": {
                "personal": {
                    "usage": {"five_hour": {"utilization": 12.4}}
                },
                "work": {
                    "usage": {"rate_limit": {"primary_window": {"used_percent": 55.0}}}
                }
            }
        });
        assert_eq!(
            format_statusline_segment(&status),
            "Subhub: personal | 5h 12%"
        );

        let codex_only = serde_json::json!({
            "selected": {"claude": null, "codex": "work"},
            "credentials": {
                "work": {
                    "usage": {"rate_limit": {"primary_window": {"used_percent": 55.0}}}
                }
            }
        });
        assert_eq!(format_statusline_segment(&codex_only), "Subhub: auditing");
    }

    #[test]
    fn managed_statusline_preserves_display_options() {
        let previous = serde_json::json!({
            "type": "command",
            "command": "old-status",
            "padding": 2,
            "refreshInterval": 10,
            "hideVimModeIndicator": true
        });
        let managed = managed_status_line(Some(&previous));
        assert_eq!(managed["type"], "command");
        assert_eq!(managed["padding"], 2);
        assert_eq!(managed["refreshInterval"], 10);
        assert_eq!(managed["hideVimModeIndicator"], true);
        assert_ne!(managed["command"], "old-status");
    }
}
