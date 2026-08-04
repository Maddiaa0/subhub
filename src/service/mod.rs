mod claude_settings;
mod codex_config;
mod gateway_client;
mod platform;

pub(crate) use gateway_client::{
    read_gateway_token, reload_gateway_accounts, select_gateway_account,
};

use crate::output::{print_gateway_health, yes_no};
use crate::{
    Error, Result, VAULT_SERVICE, credential_delete, credential_write, index_path, load_index,
    save_json_file,
};
use claude_settings::{
    claude_settings_path, ensure_no_conflicting_claude_credentials, get_nested,
    managed_status_line, read_json_object, restore_claude_settings, set_nested,
};
use codex_config::{install_codex_config, read_codex_config, restore_codex_config};
use gateway_client::fetch_gateway_status;
use platform::{
    background_service_path, background_service_running, disable_background_service,
    refresh_background_services, restart_background_service, start_background_service,
    stop_background_service, write_background_service,
};
use rand::distr::{Alphanumeric, SampleString};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::env;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const GATEWAY_SERVICE: &str = "subhub-gateway";
const GATEWAY_TOKEN_ACCOUNT: &str = "local-client-token";
pub(super) const BASE_URL: &str = "http://127.0.0.1:7842";

#[derive(Debug, Deserialize, Serialize)]
pub(super) struct PreviousValue {
    pub(super) present: bool,
    pub(super) value: Option<Value>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(super) struct InstallState {
    version: u8,
    binary_path: PathBuf,
    pub(super) previous_base_url: PreviousValue,
    pub(super) previous_api_key_helper: PreviousValue,
    #[serde(default)]
    pub(super) previous_status_line: Option<PreviousValue>,
    #[serde(default)]
    pub(super) previous_codex_config: Option<String>,
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
    let binary_path = env::current_exe().map_err(|error| {
        Error::Message(format!("could not resolve the Subhub executable: {error}"))
    })?;
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
            Error::Message(format!(
                "could not remove {}: {error}",
                state_path.display()
            ))
        })?;
    }
    let agent_path = background_service_path()?;
    if agent_path.exists() {
        fs::remove_file(&agent_path).map_err(|error| {
            Error::Message(format!(
                "could not remove {}: {error}",
                agent_path.display()
            ))
        })?;
    }
    refresh_background_services()?;
    let helper_path = auth_helper_path()?;
    if helper_path.exists() {
        fs::remove_file(&helper_path).map_err(|error| {
            Error::Message(format!(
                "could not remove {}: {error}",
                helper_path.display()
            ))
        })?;
    }
    let statusline_path = statusline_helper_path()?;
    if statusline_path.exists() {
        fs::remove_file(&statusline_path).map_err(|error| {
            Error::Message(format!(
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
        return Err(Error::Message(
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

pub(crate) fn statusline() -> Result<()> {
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .map_err(|error| Error::Message(format!("could not read status-line input: {error}")))?;
    let previous = previous_statusline_output(&input).unwrap_or_default();
    let subhub = fetch_gateway_status()
        .map(|status| crate::output::format_statusline_segment(&status))
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
        .map_err(|error| Error::Message(format!("could not run previous status line: {error}")))?;
    child
        .stdin
        .take()
        .ok_or_else(|| Error::Message("previous status line has no stdin".into()))?
        .write_all(input.as_bytes())?;
    let output = child
        .wait_with_output()
        .map_err(|error| Error::Message(format!("previous status line failed: {error}")))?;
    if output.status.success() {
        String::from_utf8(output.stdout)
            .map_err(|_| Error::Message("previous status line returned non-UTF-8 output".into()))
    } else {
        Ok(String::new())
    }
}

fn purge_data() -> Result<()> {
    let index_path = index_path()?;
    if index_path.exists() {
        let index = load_index(&index_path)?;
        for name in index.credentials {
            let _ = credential_delete(VAULT_SERVICE, &name);
        }
        fs::remove_file(&index_path).map_err(|error| {
            Error::Message(format!(
                "could not remove {}: {error}",
                index_path.display()
            ))
        })?;
    }
    let _ = credential_delete(GATEWAY_SERVICE, GATEWAY_TOKEN_ACCOUNT);
    Ok(())
}

fn install_state_path() -> Result<PathBuf> {
    Ok(index_path()?
        .parent()
        .expect("index path has a parent")
        .join("install.json"))
}

pub(super) fn auth_helper_path() -> Result<PathBuf> {
    Ok(index_path()?
        .parent()
        .expect("index path has a parent")
        .join("auth-token"))
}

pub(super) fn statusline_helper_path() -> Result<PathBuf> {
    Ok(index_path()?
        .parent()
        .expect("index path has a parent")
        .join("statusline"))
}

fn read_install_state(path: &Path) -> Result<InstallState> {
    let contents = fs::read_to_string(path)
        .map_err(|error| Error::Message(format!("could not read {}: {error}", path.display())))?;
    let state: InstallState = serde_json::from_str(&contents)
        .map_err(|error| Error::Message(format!("invalid {}: {error}", path.display())))?;
    if state.version != 1 {
        return Err(Error::Message(format!(
            "unsupported install state version {}",
            state.version
        )));
    }
    Ok(state)
}

pub(super) fn write_private_file(path: &Path, contents: &[u8]) -> Result<()> {
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

fn write_auth_helper(path: &Path, binary: &Path) -> Result<()> {
    write_executable_helper(
        path,
        &format!(
            "#!/bin/sh\nexec {} gateway auth-token\n",
            shell_quote(binary)
        ),
    )
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
        .ok_or_else(|| Error::Message("helper path has no parent".into()))?;
    fs::create_dir_all(parent)?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true).mode(0o700);
    let mut file = options.open(path)?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

pub(super) fn shell_quote(path: &Path) -> String {
    let raw = path.to_string_lossy();
    format!("'{}'", raw.replace('\'', "'\"'\"'"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn shell_quoting_handles_special_paths() {
        assert_eq!(
            shell_quote(Path::new("/tmp/Sub Hub's/bin")),
            "'/tmp/Sub Hub'\"'\"'s/bin'"
        );
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
}
