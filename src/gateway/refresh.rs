use super::state::{ProxyState, RefreshBackoff, now, safe_error};
use crate::provider::StoredCredential;
use crate::{Result, refresh_claude_credential};

pub(super) async fn refresh_due_credentials(state: &ProxyState) {
    let deadline = chrono::Utc::now().timestamp_millis() + 60_000;
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
        .is_none_or(|backoff| backoff.retry_at <= now())
}

pub(super) async fn refresh_credential(
    state: &ProxyState,
    name: &str,
    force: bool,
    expected_access_token: Option<&str>,
) -> Result<StoredCredential> {
    let _guard = state.refresh_lock.lock().await;
    // Another task may have completed the refresh while this one waited.
    if let Some(current) = state
        .credentials
        .read()
        .await
        .iter()
        .find(|credential| credential.name == name)
        .cloned()
        && ((!force
            && current.expires_at.is_none_or(|expires_at| {
                expires_at > chrono::Utc::now().timestamp_millis() + 60_000
            }))
            || expected_access_token.is_some_and(|expected| current.access_token != expected))
    {
        return Ok(current);
    }
    let refreshed = match refresh_claude_credential(&state.client, name).await {
        Ok(refreshed) => {
            state.refresh_backoff.lock().await.remove(name);
            crate::observability::event(
                "refresh_succeeded",
                serde_json::json!({"credential": name, "provider": "claude"}),
            );
            refreshed
        }
        Err(error) => {
            let mut backoffs = state.refresh_backoff.lock().await;
            let failures = backoffs
                .get(name)
                .map_or(1, |backoff| backoff.failures.saturating_add(1));
            let delay = 30_u64.saturating_mul(2_u64.saturating_pow(failures.min(6) - 1));
            let jitter = rand::random_range(0..=15);
            backoffs.insert(
                name.to_owned(),
                RefreshBackoff {
                    failures,
                    retry_at: now() + delay.min(1_800) + jitter,
                },
            );
            crate::observability::event(
                "refresh_failed",
                serde_json::json!({
                    "credential": name,
                    "provider": "claude",
                    "retry_at": now() + delay.min(1_800) + jitter,
                    "error": safe_error(error.to_string())
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
}
