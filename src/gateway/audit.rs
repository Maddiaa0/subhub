//! Usage auditing: periodically fetch each credential's subscription usage
//! and record health. Audits are advisory; transient failures must not make
//! a working credential unroutable.

use super::protocol::CredentialUsage;
use super::refresh::refresh_credential;
use super::state::{CredentialHealth, ProxyState, now, safe_error};
use crate::codex;
use crate::error::{CredentialError, ErrorKind};
use crate::provider::{Provider, StoredCredential};
use crate::{Error, Result};

pub(super) async fn audit_all(state: &ProxyState) {
    let credentials = state.credentials.read().await.clone();
    for credential in &credentials {
        let credential = if credential.provider.supports_refresh()
            && credential.expires_at.is_some_and(|expires_at| {
                expires_at <= chrono::Utc::now().timestamp_millis() + 60_000
            }) {
            match refresh_credential(state, &credential.name, false, None).await {
                Ok(credential) => credential,
                Err(error) => {
                    record_audit(state, credential, Err(error)).await;
                    continue;
                }
            }
        } else {
            credential.clone()
        };
        let mut result = if credential.provider == Provider::Codex {
            match credential.account_id.as_deref() {
                Some(account) => {
                    codex::fetch_usage(&state.client, &credential.access_token, account)
                        .await
                        .map(CredentialUsage::Codex)
                }
                None => Err(Error::audit_fatal("Codex credential has no account id")),
            }
        } else if !credential.scopes.is_empty()
            && !credential
                .scopes
                .iter()
                .any(|scope| scope == "user:profile")
        {
            Err(Error::audit_fatal("OAuth token lacks user:profile scope"))
        } else {
            state
                .usage_client
                .fetch(&credential.access_token)
                .await
                .map(CredentialUsage::Claude)
        };
        if credential.provider.supports_refresh()
            && result
                .as_ref()
                .is_err_and(|error| error.kind() == ErrorKind::FatalAudit)
            && let Ok(refreshed) = refresh_credential(
                state,
                &credential.name,
                true,
                Some(&credential.access_token),
            )
            .await
        {
            result = state
                .usage_client
                .fetch(&refreshed.access_token)
                .await
                .map(CredentialUsage::Claude);
        }
        record_audit(state, &credential, result).await;
    }
}

async fn record_audit(
    state: &ProxyState,
    credential: &StoredCredential,
    result: Result<CredentialUsage>,
) {
    crate::observability::event(
        if result.is_ok() {
            "audit_succeeded"
        } else {
            "audit_failed"
        },
        serde_json::json!({
            "credential": credential.name,
            "provider": credential.provider,
            "error": result.as_ref().err().map(|error| safe_error(error.to_string())),
            "error_kind": result.as_ref().err().map(Error::kind)
        }),
    );
    let mut health = state.health.write().await;
    let previous_usage = health
        .get(&credential.name)
        .and_then(|entry| entry.usage.clone());
    health.insert(
        credential.name.clone(),
        audit_health(previous_usage, result),
    );
}

fn audit_health(
    previous_usage: Option<CredentialUsage>,
    result: Result<CredentialUsage>,
) -> CredentialHealth {
    match result {
        Ok(usage) => CredentialHealth {
            usage: Some(usage),
            error: None,
            checked_at: now(),
        },
        Err(error) => CredentialHealth {
            // A usage audit is advisory. In particular, the usage endpoint can
            // rate-limit independently of inference, so a transient audit
            // failure must not make a working credential unroutable.
            usage: previous_usage,
            error: Some(CredentialError::from(&error)),
            checked_at: now(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_audit_retains_last_known_usage() {
        let previous = CredentialUsage::Claude(
            serde_json::from_value(serde_json::json!({
                "five_hour": {"utilization": 25.0, "resets_at": null}
            }))
            .unwrap(),
        );

        let health = audit_health(
            Some(previous),
            Err(Error::audit_transient(
                "usage endpoint rate limited; retry after 300",
            )),
        );

        assert_eq!(
            health
                .usage
                .as_ref()
                .and_then(|usage| match usage {
                    CredentialUsage::Claude(usage) => usage.five_hour.as_ref(),
                    CredentialUsage::Codex(_) => None,
                })
                .and_then(|window| window.utilization),
            Some(25.0)
        );
        let error = health.error.unwrap();
        assert_eq!(error.kind, ErrorKind::TransientAudit);
        assert_eq!(
            error.message,
            "usage endpoint rate limited; retry after 300"
        );
    }

    #[test]
    fn initial_failed_audit_remains_unroutable() {
        let health = audit_health(None, Err(Error::Message("usage request failed".into())));

        assert!(health.usage.is_none());
        let error = health.error.unwrap();
        assert_eq!(error.kind, ErrorKind::FatalAudit);
        assert_eq!(error.message, "usage request failed");
    }
}
