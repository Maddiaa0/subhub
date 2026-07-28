use crate::{
    AppError, Result, VAULT_SERVICE, index_path, keychain_delete, keychain_read, keychain_write,
    load_index, save_json_file,
};
use rand::distr::{Alphanumeric, SampleString};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::env;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

const GATEWAY_SERVICE: &str = "subhub-gateway";
const GATEWAY_TOKEN_ACCOUNT: &str = "local-client-token";
const LAUNCH_AGENT_LABEL: &str = "com.subhub.gateway";
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
}

pub(crate) fn read_gateway_token() -> Result<String> {
    keychain_read(GATEWAY_SERVICE, GATEWAY_TOKEN_ACCOUNT)
}

fn ensure_gateway_token() -> Result<String> {
    if let Ok(token) = read_gateway_token()
        && !token.is_empty()
    {
        return Ok(token);
    }
    let token = Alphanumeric.sample_string(&mut rand::rng(), 48);
    keychain_write(GATEWAY_SERVICE, GATEWAY_TOKEN_ACCOUNT, &token)?;
    Ok(token)
}

pub(crate) fn install() -> Result<()> {
    let binary_path = env::current_exe()
        .map_err(|error| AppError(format!("could not resolve the Subhub executable: {error}")))?;
    let settings_path = claude_settings_path()?;
    let state_path = install_state_path()?;
    let mut settings = read_json_object(&settings_path)?;
    ensure_no_conflicting_claude_credentials(&settings)?;
    let state = if state_path.exists() {
        read_install_state(&state_path)?
    } else {
        InstallState {
            version: 1,
            binary_path: binary_path.clone(),
            previous_base_url: get_nested(&settings, &["env", "ANTHROPIC_BASE_URL"]),
            previous_api_key_helper: get_nested(&settings, &["apiKeyHelper"]),
        }
    };

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

    let current_state = InstallState {
        binary_path: binary_path.clone(),
        ..state
    };
    save_json_file(&state_path, &current_state)?;
    save_json_file(&settings_path, &settings)?;
    write_auth_helper(&auth_helper_path()?, &binary_path)?;
    let agent_path = launch_agent_path()?;
    write_launch_agent(&agent_path, &binary_path)?;
    restart()?;

    println!("Subhub gateway installed and running.");
    println!("Claude Code will use {BASE_URL} automatically.");
    Ok(())
}

pub(crate) fn uninstall(purge: bool) -> Result<()> {
    stop()?;
    let state_path = install_state_path()?;
    if state_path.exists() {
        let state = read_install_state(&state_path)?;
        restore_claude_settings(&state)?;
        fs::remove_file(&state_path).map_err(|error| {
            AppError(format!(
                "could not remove {}: {error}",
                state_path.display()
            ))
        })?;
    }
    let agent_path = launch_agent_path()?;
    if agent_path.exists() {
        fs::remove_file(&agent_path).map_err(|error| {
            AppError(format!(
                "could not remove {}: {error}",
                agent_path.display()
            ))
        })?;
    }
    let helper_path = auth_helper_path()?;
    if helper_path.exists() {
        fs::remove_file(&helper_path).map_err(|error| {
            AppError(format!(
                "could not remove {}: {error}",
                helper_path.display()
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

pub(crate) fn start() -> Result<()> {
    let agent_path = launch_agent_path()?;
    if !agent_path.exists() {
        return Err(AppError(
            "Subhub is not installed; run `subhub install`".into(),
        ));
    }
    let target = launch_target()?;
    if launchctl(["print", &target]).is_ok() {
        return Ok(());
    }
    launchctl([
        "bootstrap",
        &launch_domain()?,
        agent_path
            .to_str()
            .ok_or_else(|| AppError("LaunchAgent path is not UTF-8".into()))?,
    ])
}

pub(crate) fn stop() -> Result<()> {
    let target = launch_target()?;
    if launchctl(["print", &target]).is_err() {
        return Ok(());
    }
    launchctl(["bootout", &target])
}

pub(crate) fn restart() -> Result<()> {
    let target = launch_target()?;
    if launchctl(["print", &target]).is_ok() {
        launchctl(["kickstart", "-k", &target])
    } else {
        start()
    }
}

pub(crate) fn status() -> Result<()> {
    let installed = launch_agent_path()?.exists() && install_state_path()?.exists();
    let target = launch_target()?;
    let running = launchctl(["print", &target]).is_ok();
    println!("Installed: {}", yes_no(installed));
    println!("Running:   {}", yes_no(running));
    println!("Endpoint:  {BASE_URL}");
    println!(
        "Token:     {}",
        if read_gateway_token().is_ok() {
            "available in Keychain"
        } else {
            "missing"
        }
    );
    if running {
        match gateway_health() {
            Ok(Some(selected)) => println!("Gateway:   reachable (using {selected})"),
            Ok(None) => println!("Gateway:   reachable (auditing credentials)"),
            Err(error) => println!("Gateway:   not reachable ({error})"),
        }
    }
    if env::var_os("ANTHROPIC_AUTH_TOKEN").is_some() || env::var_os("ANTHROPIC_API_KEY").is_some() {
        println!("Warning: a shell Anthropic credential may override Subhub's apiKeyHelper.");
    }
    Ok(())
}

fn gateway_health() -> Result<Option<String>> {
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
        let body: Value = response
            .json()
            .await
            .map_err(|error| AppError(error.to_string()))?;
        Ok(body
            .get("selected")
            .and_then(Value::as_str)
            .map(str::to_owned))
    })
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn purge_data() -> Result<()> {
    let index_path = index_path()?;
    if index_path.exists() {
        let index = load_index(&index_path)?;
        for name in index.credentials {
            let _ = keychain_delete(VAULT_SERVICE, &name);
        }
        fs::remove_file(&index_path).map_err(|error| {
            AppError(format!(
                "could not remove {}: {error}",
                index_path.display()
            ))
        })?;
    }
    let _ = keychain_delete(GATEWAY_SERVICE, GATEWAY_TOKEN_ACCOUNT);
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

fn claude_settings_path() -> Result<PathBuf> {
    if let Some(directory) = env::var_os("CLAUDE_CONFIG_DIR") {
        return Ok(PathBuf::from(directory).join("settings.json"));
    }
    env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".claude").join("settings.json"))
        .ok_or_else(|| AppError("HOME is not set".into()))
}

fn launch_agent_path() -> Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| {
            home.join("Library")
                .join("LaunchAgents")
                .join(format!("{LAUNCH_AGENT_LABEL}.plist"))
        })
        .ok_or_else(|| AppError("HOME is not set".into()))
}

fn write_launch_agent(path: &Path, binary: &Path) -> Result<()> {
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

fn write_auth_helper(path: &Path, binary: &Path) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| AppError("auth helper path has no parent".into()))?;
    fs::create_dir_all(parent)?;
    let contents = format!("#!/bin/sh\nexec {} auth-token\n", shell_quote(binary));
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true).mode(0o700);
    let mut file = options.open(path)?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
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

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn launch_domain() -> Result<String> {
    Ok(format!("gui/{}", user_id()?))
}

fn launch_target() -> Result<String> {
    Ok(format!("{}/{LAUNCH_AGENT_LABEL}", launch_domain()?))
}

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
            "theme": "dark"
        });
        let previous_base = get_nested(&settings, &["env", "ANTHROPIC_BASE_URL"]);
        let previous_helper = get_nested(&settings, &["apiKeyHelper"]);
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
        assert_eq!(settings["env"]["ANTHROPIC_BASE_URL"], "https://old.example");
        assert_eq!(settings["env"]["KEEP"], "yes");
        assert_eq!(settings["apiKeyHelper"], "old-helper");
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
        let binary = Path::new("/Applications/Sub Hub/subhub");

        write_launch_agent(&agent, binary).unwrap();
        write_auth_helper(&helper, binary).unwrap();

        let agent_contents = fs::read_to_string(&agent).unwrap();
        let helper_contents = fs::read_to_string(&helper).unwrap();
        assert!(agent_contents.contains("<string>--background</string>"));
        assert!(!agent_contents.contains("local-client-token"));
        assert_eq!(
            helper_contents,
            "#!/bin/sh\nexec '/Applications/Sub Hub/subhub' auth-token\n"
        );
        assert_eq!(
            fs::metadata(&helper).unwrap().permissions().mode() & 0o777,
            0o700
        );
        fs::remove_dir_all(directory).unwrap();
    }
}
