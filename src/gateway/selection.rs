use super::protocol::CredentialUsage;
use super::state::{CredentialHealth, ProxyState};
use crate::error::{CredentialError, ErrorKind};
use crate::provider::{Provider, StoredCredential};
use axum::http::StatusCode;
use std::collections::HashMap;

pub(super) async fn select_credential(
    state: &ProxyState,
    model: Option<&str>,
    exclude: Option<&str>,
    provider: Provider,
) -> Option<StoredCredential> {
    let credentials = state.credentials.read().await.clone();
    let health = state.health.read().await;
    let current = state.selected.lock().await.get(provider);
    let eligible = |credential: &&StoredCredential| {
        credential.provider == provider
            && exclude != Some(credential.name.as_str())
            && health
                .get(&credential.name)
                .and_then(|entry| entry.usage.as_ref())
                .is_some_and(|usage| usage.eligible(model, state.reserve_percent))
    };
    if let Some(name) = current
        && let Some(credential) = credentials
            .iter()
            .find(|credential| credential.name == name)
            .filter(eligible)
    {
        return Some(credential.clone());
    }
    let selected = credentials
        .iter()
        .filter(eligible)
        .min_by(|a, b| {
            utilization(&health, &a.name, model).total_cmp(&utilization(&health, &b.name, model))
        })
        .cloned();
    let selected = selected.or_else(|| {
        credentials
            .iter()
            .filter(|credential| {
                credential.provider == provider
                    && exclude != Some(credential.name.as_str())
                    && health.get(&credential.name).is_some_and(|entry| {
                        entry.usage.is_none()
                            && entry
                                .error
                                .as_ref()
                                .is_some_and(|error| error.kind == ErrorKind::TransientAudit)
                    })
            })
            .min_by_key(|credential| health[&credential.name].checked_at)
            .cloned()
    });
    if let Some(credential) = &selected {
        *state.selected.lock().await.slot(provider) = Some(credential.name.clone());
    }
    selected
}

fn utilization(health: &HashMap<String, CredentialHealth>, name: &str, model: Option<&str>) -> f64 {
    health[name]
        .usage
        .as_ref()
        .map(|usage| usage.utilization(model))
        .unwrap_or(0.0)
}

pub(super) async fn set_selected_account(state: &ProxyState, name: &str) -> bool {
    let Some(provider) = state
        .credentials
        .read()
        .await
        .iter()
        .find(|credential| credential.name == name)
        .map(|credential| credential.provider)
    else {
        return false;
    };
    *state.selected.lock().await.slot(provider) = Some(name.to_owned());
    true
}

pub(super) async fn routing_error_message(state: &ProxyState, provider: Provider) -> String {
    let credentials = state.credentials.read().await;
    let health = state.health.read().await;
    let relevant: Vec<_> = credentials
        .iter()
        .filter(|credential| credential.provider == provider)
        .collect();
    if relevant.is_empty() {
        return match provider {
            Provider::Claude => "no Claude credential configured; run `subhub add <name>`".into(),
            Provider::Codex => "no Codex credential configured; run `subhub add <name>`".into(),
        };
    }
    if let Some((credential, error)) = relevant.iter().find_map(|credential| {
        health
            .get(&credential.name)
            .and_then(|entry| entry.error.as_ref())
            .filter(|error| error.kind == ErrorKind::Refresh)
            .map(|error| (*credential, error))
    }) {
        return format!(
            "credential `{}` could not refresh: {}; run `subhub add {} --force` if its refresh token was revoked",
            credential.name, error.message, credential.name
        );
    }
    if relevant.iter().all(|credential| {
        health
            .get(&credential.name)
            .is_some_and(|entry| entry.usage.is_some())
    }) {
        return "all credentials are at their configured usage limit; check `subhub audit` for reset times".into();
    }
    let detail = relevant.iter().find_map(|credential| {
        health
            .get(&credential.name)
            .and_then(|entry| entry.error.as_ref())
    });
    match detail {
        Some(error) => format!(
            "credential usage is unknown because its latest audit failed: {}",
            error.message
        ),
        None => "credentials are still being audited; retry shortly".into(),
    }
}

pub(super) async fn mark_failed(state: &ProxyState, name: &str, status: StatusCode) {
    if let Some(health) = state.health.write().await.get_mut(name) {
        health.error = Some(CredentialError {
            kind: if status == StatusCode::UNAUTHORIZED {
                ErrorKind::FatalAudit
            } else {
                ErrorKind::TransientAudit
            },
            message: format!("inference request returned {status}"),
        });
        if status == StatusCode::UNAUTHORIZED {
            health.usage = None;
        } else if let Some(usage) = health.usage.as_mut() {
            match usage {
                CredentialUsage::Claude(usage) => {
                    if let Some(window) = usage.five_hour.as_mut() {
                        window.utilization = Some(100.0);
                    }
                }
                CredentialUsage::Codex(usage) => {
                    if let Some(window) = usage.rate_limit.primary_window.as_mut() {
                        window.used_percent = Some(100.0);
                    }
                }
            }
        }
    }
    state.selected.lock().await.clear_name(name);
}

#[cfg(test)]
mod tests {
    use super::super::state::test_state;
    use super::*;

    #[tokio::test]
    async fn selection_skips_exhausted_and_remains_sticky() {
        let state = test_state();
        let selected = select_credential(&state, Some("claude-sonnet"), None, Provider::Claude)
            .await
            .unwrap();
        assert_eq!(selected.name, "ready");
        assert_eq!(state.selected.lock().await.claude.as_deref(), Some("ready"));
        let selected_again =
            select_credential(&state, Some("claude-sonnet"), None, Provider::Claude)
                .await
                .unwrap();
        assert_eq!(selected_again.name, "ready");
    }

    #[tokio::test]
    async fn routing_error_explains_refresh_recovery() {
        let state = test_state();
        let mut health = state.health.write().await;
        health.get_mut("ready").unwrap().usage = None;
        health.get_mut("ready").unwrap().error = Some(CredentialError {
            kind: ErrorKind::Refresh,
            message: "Claude OAuth refresh returned 400 Bad Request".into(),
        });
        drop(health);

        let message = routing_error_message(&state, Provider::Claude).await;
        assert!(message.contains("could not refresh"));
        assert!(message.contains("subhub add ready --force"));
        assert!(!message.contains("access_token"));
    }

    #[tokio::test]
    async fn transient_audit_failure_can_fall_back_to_inference() {
        let state = test_state();
        let mut health = state.health.write().await;
        health.get_mut("full").unwrap().usage = None;
        health.get_mut("full").unwrap().error = Some(CredentialError {
            kind: ErrorKind::TransientAudit,
            message: "usage request timed out".into(),
        });
        health.get_mut("ready").unwrap().usage = None;
        health.get_mut("ready").unwrap().error = Some(CredentialError {
            kind: ErrorKind::FatalAudit,
            message: "OAuth token is unauthorized".into(),
        });
        drop(health);

        let selected = select_credential(&state, None, None, Provider::Claude)
            .await
            .unwrap();
        assert_eq!(selected.name, "full");
    }

    #[tokio::test]
    async fn manual_selection_updates_known_account_only() {
        let state = test_state();
        assert!(set_selected_account(&state, "ready").await);
        assert_eq!(state.selected.lock().await.claude.as_deref(), Some("ready"));
        assert!(!set_selected_account(&state, "missing").await);
        assert_eq!(state.selected.lock().await.claude.as_deref(), Some("ready"));
    }

    #[tokio::test]
    async fn provider_selection_never_crosses_subscription_types() {
        let state = test_state();
        state.credentials.write().await.push(StoredCredential {
            name: "codex".into(),
            access_token: "secret-c".into(),
            expires_at: None,
            scopes: Vec::new(),
            provider: Provider::Codex,
            account_id: Some("account-c".into()),
        });
        state.health.write().await.insert(
            "codex".into(),
            CredentialHealth {
                usage: Some(CredentialUsage::Codex(
                    serde_json::from_value(serde_json::json!({
                        "rate_limit": {
                            "primary_window": {"used_percent": 10.0, "reset_at": 1},
                            "secondary_window": {"used_percent": 20.0, "reset_at": 2}
                        }
                    }))
                    .unwrap(),
                )),
                error: None,
                checked_at: 1,
            },
        );
        let selected = select_credential(&state, None, None, Provider::Codex)
            .await
            .unwrap();
        assert_eq!(selected.name, "codex");
    }

    #[tokio::test]
    async fn each_provider_keeps_its_own_selection() {
        let state = test_state();
        state.credentials.write().await.push(StoredCredential {
            name: "codex".into(),
            access_token: "secret-c".into(),
            expires_at: None,
            scopes: Vec::new(),
            provider: Provider::Codex,
            account_id: Some("account-c".into()),
        });
        state.health.write().await.insert(
            "codex".into(),
            CredentialHealth {
                usage: Some(CredentialUsage::Codex(
                    serde_json::from_value(serde_json::json!({
                        "rate_limit": {
                            "primary_window": {"used_percent": 10.0, "reset_at": 1}
                        }
                    }))
                    .unwrap(),
                )),
                error: None,
                checked_at: 1,
            },
        );
        select_credential(&state, Some("claude-sonnet"), None, Provider::Claude).await;
        select_credential(&state, None, None, Provider::Codex).await;
        let selected = state.selected.lock().await.clone();
        assert_eq!(selected.claude.as_deref(), Some("ready"));
        assert_eq!(selected.codex.as_deref(), Some("codex"));

        mark_failed(&state, "codex", StatusCode::UNAUTHORIZED).await;
        let selected = state.selected.lock().await.clone();
        assert_eq!(selected.claude.as_deref(), Some("ready"));
        assert_eq!(selected.codex, None);
    }
}
