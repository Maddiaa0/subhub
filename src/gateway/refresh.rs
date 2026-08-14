//! Proactive OAuth refresh: a scheduler that refreshes soon-to-expire
//! tokens with per-credential exponential backoff on failure.

use super::state::{ProxyState, RefreshBackoff, now, safe_error};
use crate::provider::StoredCredential;
use crate::{Result, refresh_claude_credential};

pub(super) const REFRESH_MARGIN_MS: i64 = 5 * 60 * 1000;

pub(super) async fn refresh_due_credentials(state: &ProxyState) {
    let deadline = chrono::Utc::now().timestamp_millis() + REFRESH_MARGIN_MS;
    let credentials = state.credentials.read().await.clone();
    for credential in credentials {
        if credential.provider.supports_refresh()
            && credential
                .expires_at
                .is_some_and(|expires_at| expires_at <= deadline)
            && refresh_retry_ready(state, &credential.name).await
        {
            let _ = refresh_credential(state, &credential.name, false, None).await;
        }
    }
}

async fn refresh_retry_ready(state: &ProxyState, name: &str) -> bool {
    state
        .refresh_backoff
        .lock()
        .await
        .get(name)
        .is_none_or(|backoff| !backoff.terminal && backoff.retry_at <= now())
}

pub(super) async fn refresh_credential(
    state: &ProxyState,
    name: &str,
    force: bool,
    expected_access_token: Option<&str>,
) -> Result<StoredCredential> {
    let lock = {
        let mut locks = state.refresh_locks.lock().await;
        locks.entry(name.to_owned()).or_default().clone()
    };
    let _guard = lock.lock().await;
    // Another task may have completed the refresh while this one waited.
    let current = state
        .credentials
        .read()
        .await
        .iter()
        .find(|credential| credential.name == name)
        .cloned();
    if let Some(current) = &current
        && ((!force
            && current.expires_at.is_none_or(|expires_at| {
                expires_at > chrono::Utc::now().timestamp_millis() + REFRESH_MARGIN_MS
            }))
            || expected_access_token.is_some_and(|expected| current.access_token != expected))
    {
        return Ok(current.clone());
    }
    if let Some(backoff) = state.refresh_backoff.lock().await.get(name).cloned() {
        if backoff.terminal {
            return Err(crate::Error::refresh_terminal(backoff.message));
        }
        if backoff.retry_at > now() {
            return Err(crate::Error::refresh_transient(format!(
                "Claude OAuth refresh is backed off until epoch second {}",
                backoff.retry_at
            )));
        }
    }
    let expected = expected_access_token.map(str::to_owned).or_else(|| {
        current
            .as_ref()
            .map(|credential| credential.access_token.clone())
    });
    let refreshed = match refresh_claude_credential(&state.client, name, expected.as_deref()).await
    {
        Ok(refreshed) => {
            state.refresh_backoff.lock().await.remove(name);
            if let Some(health) = state.health.write().await.get_mut(name)
                && health
                    .error
                    .as_ref()
                    .is_some_and(|error| error.kind == crate::error::ErrorKind::Refresh)
            {
                health.error = None;
            }
            crate::observability::event(
                "refresh_succeeded",
                serde_json::json!({"credential": name, "provider": "claude"}),
            );
            refreshed
        }
        Err(error) => {
            let terminal = error.refresh_is_terminal();
            let message = safe_error(error.to_string());
            let mut backoffs = state.refresh_backoff.lock().await;
            let failures = backoffs
                .get(name)
                .map_or(1, |backoff| backoff.failures.saturating_add(1));
            let delay = 30_u64.saturating_mul(2_u64.saturating_pow(failures.min(6) - 1));
            let jitter = rand::random_range(0..=15);
            let retry_at = if terminal {
                u64::MAX
            } else {
                now() + delay.min(1_800) + jitter
            };
            backoffs.insert(
                name.to_owned(),
                RefreshBackoff {
                    failures,
                    retry_at,
                    terminal,
                    message: message.clone(),
                },
            );
            drop(backoffs);
            if let Some(health) = state.health.write().await.get_mut(name) {
                health.usage = None;
                health.error = Some(crate::error::CredentialError::from(&error));
                health.checked_at = now();
            }
            state.selected.lock().await.clear_name(name);
            crate::observability::event(
                "refresh_failed",
                serde_json::json!({
                    "credential": name,
                    "provider": "claude",
                    "retry_at": (!terminal).then_some(retry_at),
                    "terminal": terminal,
                    "error": message
                }),
            );
            return Err(error);
        }
    };
    if let Some(current) = state
        .credentials
        .write()
        .await
        .iter_mut()
        .find(|credential| credential.name == name)
    {
        *current = refreshed.clone();
    }
    Ok(refreshed)
}

#[cfg(test)]
mod tests {
    use super::super::state::test_state;
    use super::*;

    #[tokio::test]
    async fn refresh_backoff_suppresses_retries_until_due() {
        let state = test_state();
        state.refresh_backoff.lock().await.insert(
            "ready".into(),
            RefreshBackoff {
                failures: 3,
                retry_at: now() + 300,
                terminal: false,
                message: "temporary failure".into(),
            },
        );
        assert!(!refresh_retry_ready(&state, "ready").await);
        state
            .refresh_backoff
            .lock()
            .await
            .get_mut("ready")
            .unwrap()
            .retry_at = now();
        assert!(refresh_retry_ready(&state, "ready").await);
    }

    #[tokio::test]
    async fn terminal_refresh_failure_never_becomes_retry_ready() {
        let state = test_state();
        state.refresh_backoff.lock().await.insert(
            "ready".into(),
            RefreshBackoff {
                failures: 1,
                retry_at: 0,
                terminal: true,
                message: "invalid_grant".into(),
            },
        );
        assert!(!refresh_retry_ready(&state, "ready").await);
    }
}
