//! Shared gateway state and small helpers used across gateway submodules.

use super::protocol::CredentialUsage;
use crate::error::CredentialError;
use crate::provider::{Provider, StoredCredential};
use crate::usage::UsageClient;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex, RwLock};

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct SelectedAccounts {
    pub(crate) claude: Option<String>,
    pub(crate) codex: Option<String>,
}

impl SelectedAccounts {
    pub(crate) fn get(&self, provider: Provider) -> Option<String> {
        self.slot_ref(provider).clone()
    }

    fn slot_ref(&self, provider: Provider) -> &Option<String> {
        match provider {
            Provider::Claude => &self.claude,
            Provider::Codex => &self.codex,
        }
    }

    pub(crate) fn slot(&mut self, provider: Provider) -> &mut Option<String> {
        match provider {
            Provider::Claude => &mut self.claude,
            Provider::Codex => &mut self.codex,
        }
    }

    pub(crate) fn clear_name(&mut self, name: &str) {
        for slot in [&mut self.claude, &mut self.codex] {
            if slot.as_deref() == Some(name) {
                *slot = None;
            }
        }
    }

    pub(crate) fn retain_names(&mut self, names: &[String]) {
        for slot in [&mut self.claude, &mut self.codex] {
            if slot.as_ref().is_some_and(|name| !names.contains(name)) {
                *slot = None;
            }
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct CredentialHealth {
    pub(crate) usage: Option<CredentialUsage>,
    pub(crate) error: Option<CredentialError>,
    pub(crate) checked_at: u64,
}

#[derive(Clone)]
pub(crate) struct ProxyState {
    pub(crate) client: reqwest::Client,
    pub(crate) usage_client: UsageClient,
    pub(crate) credentials: Arc<RwLock<Vec<StoredCredential>>>,
    pub(crate) health: Arc<RwLock<HashMap<String, CredentialHealth>>>,
    pub(crate) selected: Arc<Mutex<SelectedAccounts>>,
    pub(crate) refresh_lock: Arc<Mutex<()>>,
    pub(crate) refresh_backoff: Arc<Mutex<HashMap<String, RefreshBackoff>>>,
    pub(crate) client_token: Arc<String>,
    pub(crate) reserve_percent: f64,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct RefreshBackoff {
    pub(crate) failures: u32,
    pub(crate) retry_at: u64,
}

pub(crate) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub(crate) fn safe_error(error: String) -> String {
    error.chars().take(300).collect()
}

#[cfg(test)]
pub(crate) fn test_state() -> ProxyState {
    let client = reqwest::Client::new();
    ProxyState {
        usage_client: UsageClient::new(client.clone(), None),
        client,
        credentials: Arc::new(RwLock::new(vec![
            StoredCredential {
                name: "full".into(),
                access_token: "secret-a".into(),
                expires_at: None,
                scopes: vec!["user:profile".into()],
                provider: Provider::Claude,
                account_id: None,
            },
            StoredCredential {
                name: "ready".into(),
                access_token: "secret-b".into(),
                expires_at: None,
                scopes: vec!["user:profile".into()],
                provider: Provider::Claude,
                account_id: None,
            },
        ])),
        health: Arc::new(RwLock::new(HashMap::from([
            (
                "full".into(),
                CredentialHealth {
                    usage: Some(
                        serde_json::from_value(serde_json::json!({
                            "five_hour": {"utilization": 100.0, "resets_at": null}
                        }))
                        .unwrap(),
                    ),
                    error: None,
                    checked_at: 1,
                },
            ),
            (
                "ready".into(),
                CredentialHealth {
                    usage: Some(
                        serde_json::from_value(serde_json::json!({
                            "five_hour": {"utilization": 25.0, "resets_at": null}
                        }))
                        .unwrap(),
                    ),
                    error: None,
                    checked_at: 1,
                },
            ),
        ]))),
        selected: Arc::default(),
        refresh_lock: Arc::default(),
        refresh_backoff: Arc::default(),
        client_token: Arc::new("local-secret".into()),
        reserve_percent: 1.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reload_drops_selections_for_removed_credentials() {
        let mut selected = SelectedAccounts {
            claude: Some("kept".into()),
            codex: Some("removed".into()),
        };
        selected.retain_names(&["kept".to_string()]);
        assert_eq!(selected.claude.as_deref(), Some("kept"));
        assert_eq!(selected.codex, None);
    }
}
