use super::{BASE_URL, InstallState, PreviousValue, auth_helper_path, statusline_helper_path};
use crate::{Error, Result, save_json_file};
use serde_json::{Map, Value};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

pub(super) fn claude_settings_path() -> Result<PathBuf> {
    if let Some(directory) = env::var_os("CLAUDE_CONFIG_DIR") {
        return Ok(PathBuf::from(directory).join("settings.json"));
    }
    env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".claude").join("settings.json"))
        .ok_or_else(|| Error::Message("HOME is not set".into()))
}

pub(super) fn restore_claude_settings(state: &InstallState) -> Result<()> {
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

pub(super) fn managed_status_line(previous: Option<&Value>) -> Value {
    let mut managed = Map::new();
    managed.insert("type".into(), Value::String("command".into()));
    managed.insert(
        "command".into(),
        Value::String(statusline_helper_path().map_or_else(
            |_| "subhub gateway statusline".into(),
            |path| super::shell_quote(&path),
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

pub(super) fn ensure_no_conflicting_claude_credentials(settings: &Value) -> Result<()> {
    let conflicts: Vec<&str> = ["ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_API_KEY"]
        .into_iter()
        .filter(|name| get_nested(settings, &["env", name]).present)
        .collect();
    if conflicts.is_empty() {
        Ok(())
    } else {
        Err(Error::Message(format!(
            "Claude settings define {}; remove {} before installing Subhub because they override apiKeyHelper",
            conflicts.join(" and "),
            if conflicts.len() == 1 { "it" } else { "them" }
        )))
    }
}

pub(super) fn read_json_object(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(Value::Object(Map::new()));
    }
    let contents = fs::read_to_string(path)
        .map_err(|error| Error::Message(format!("could not read {}: {error}", path.display())))?;
    let value: Value = serde_json::from_str(&contents)
        .map_err(|error| Error::Message(format!("invalid {}: {error}", path.display())))?;
    if value.is_object() {
        Ok(value)
    } else {
        Err(Error::Message(format!(
            "{} is not a JSON object",
            path.display()
        )))
    }
}

pub(super) fn get_nested(root: &Value, path: &[&str]) -> PreviousValue {
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

pub(super) fn set_nested(root: &mut Value, path: &[&str], value: Value) -> Result<()> {
    let mut current = root;
    for key in &path[..path.len() - 1] {
        let object = current
            .as_object_mut()
            .ok_or_else(|| Error::Message("Claude settings contain a non-object parent".into()))?;
        current = object
            .entry((*key).to_owned())
            .or_insert_with(|| Value::Object(Map::new()));
    }
    current
        .as_object_mut()
        .ok_or_else(|| Error::Message("Claude settings contain a non-object parent".into()))?
        .insert(path[path.len() - 1].to_owned(), value);
    Ok(())
}

pub(super) fn restore_nested_if_managed(
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn conflicting_claude_credentials_are_rejected() {
        let settings = serde_json::json!({
            "env": {"ANTHROPIC_API_KEY": "existing-secret"}
        });
        let error = ensure_no_conflicting_claude_credentials(&settings).unwrap_err();
        assert!(error.to_string().contains("ANTHROPIC_API_KEY"));
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
