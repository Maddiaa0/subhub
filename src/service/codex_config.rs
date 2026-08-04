//! Codex config.toml integration: point the Codex CLI at the gateway and
//! restore the previous provider on uninstall.

use super::{BASE_URL, InstallState, auth_helper_path, write_private_file};
use crate::{Error, Result};
use std::env;
use std::fs;
use std::path::PathBuf;
use toml_edit::{DocumentMut, Item, Table, value};

fn codex_config_path() -> Result<PathBuf> {
    env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")))
        .map(|home| home.join("config.toml"))
        .ok_or_else(|| Error::Message("CODEX_HOME and HOME are not set".into()))
}

pub(super) fn read_codex_config() -> Result<String> {
    let path = codex_config_path()?;
    if path.exists() {
        fs::read_to_string(&path)
            .map_err(|error| Error::Message(format!("could not read {}: {error}", path.display())))
    } else {
        Ok(String::new())
    }
}

pub(super) fn install_codex_config() -> Result<()> {
    let path = codex_config_path()?;
    let mut document = read_codex_config()?
        .parse::<DocumentMut>()
        .map_err(|error| Error::Message(format!("invalid {}: {error}", path.display())))?;
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

pub(super) fn restore_codex_config(state: &InstallState) -> Result<()> {
    let Some(previous) = &state.previous_codex_config else {
        return Ok(());
    };
    let path = codex_config_path()?;
    let mut current = read_codex_config()?
        .parse::<DocumentMut>()
        .map_err(|error| Error::Message(format!("invalid {}: {error}", path.display())))?;
    let prior = previous
        .parse::<DocumentMut>()
        .map_err(|error| Error::Message(format!("saved Codex config is invalid: {error}")))?;
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
