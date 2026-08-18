use crate::provider::Provider;
use std::collections::HashMap;
use std::time::{Duration, Instant};

const ATTEMPT_TTL: Duration = Duration::from_secs(10 * 60);
const MAX_ATTEMPTS: usize = 4_096;

#[derive(Clone, Debug)]
pub(crate) struct Attempt {
    pub(super) provider: Provider,
    pub(super) credential_name: String,
    pub(super) model: Option<String>,
    pub(super) host: String,
    pub(super) method: String,
    pub(super) path: String,
    created_at: Instant,
    retry_claimed: bool,
    attempt_id: Option<String>,
    pub(super) retry_credential_name: Option<String>,
}

impl Attempt {
    pub(super) fn new(
        provider: Provider,
        credential_name: String,
        model: Option<String>,
        host: String,
        method: String,
        path: String,
    ) -> Self {
        Self {
            provider,
            credential_name,
            model,
            host,
            method,
            path,
            created_at: Instant::now(),
            retry_claimed: false,
            attempt_id: None,
            retry_credential_name: None,
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct AttemptStore {
    by_traceparent: HashMap<String, Attempt>,
    traceparent_by_attempt_id: HashMap<String, String>,
}

impl AttemptStore {
    pub(super) fn insert(&mut self, traceparent: String, attempt: Attempt) {
        self.prune();
        if self.by_traceparent.len() >= MAX_ATTEMPTS
            && let Some(oldest) = self
                .by_traceparent
                .iter()
                .min_by_key(|(_, attempt)| attempt.created_at)
                .map(|(traceparent, _)| traceparent.clone())
        {
            self.remove_traceparent(&oldest);
        }
        self.remove_traceparent(&traceparent);
        self.by_traceparent.insert(traceparent, attempt);
    }

    pub(super) fn get(&mut self, traceparent: &str) -> Option<Attempt> {
        self.prune();
        self.by_traceparent.get(traceparent).cloned()
    }

    pub(super) fn claim_retry(&mut self, traceparent: &str) -> Option<Attempt> {
        self.prune();
        let attempt = self.by_traceparent.get_mut(traceparent)?;
        if attempt.retry_claimed {
            return None;
        }
        attempt.retry_claimed = true;
        Some(attempt.clone())
    }

    pub(super) fn authorize_retry(
        &mut self,
        traceparent: &str,
        attempt_id: String,
        credential_name: String,
    ) -> bool {
        let Some(attempt) = self.by_traceparent.get_mut(traceparent) else {
            return false;
        };
        if !attempt.retry_claimed || attempt.attempt_id.is_some() {
            return false;
        }
        attempt.attempt_id = Some(attempt_id.clone());
        attempt.retry_credential_name = Some(credential_name);
        self.traceparent_by_attempt_id
            .insert(attempt_id, traceparent.to_owned());
        true
    }

    pub(super) fn remove_traceparent(&mut self, traceparent: &str) -> Option<Attempt> {
        let removed = self.by_traceparent.remove(traceparent)?;
        if let Some(attempt_id) = &removed.attempt_id {
            self.traceparent_by_attempt_id.remove(attempt_id);
        }
        Some(removed)
    }

    pub(super) fn complete(&mut self, attempt_id: &str) -> Option<Attempt> {
        self.prune();
        let traceparent = self.traceparent_by_attempt_id.remove(attempt_id)?;
        self.by_traceparent.remove(&traceparent)
    }

    pub(crate) fn len(&mut self) -> usize {
        self.prune();
        self.by_traceparent.len()
    }

    pub(crate) fn clear(&mut self) {
        self.by_traceparent.clear();
        self.traceparent_by_attempt_id.clear();
    }

    fn prune(&mut self) {
        let expired: Vec<String> = self
            .by_traceparent
            .iter()
            .filter(|(_, attempt)| attempt.created_at.elapsed() >= ATTEMPT_TTL)
            .map(|(traceparent, _)| traceparent.clone())
            .collect();
        for traceparent in expired {
            self.remove_traceparent(&traceparent);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attempt() -> Attempt {
        Attempt::new(
            Provider::Claude,
            "primary".into(),
            Some("claude-sonnet".into()),
            "api.anthropic.com".into(),
            "POST".into(),
            "/v1/messages".into(),
        )
    }

    #[test]
    fn retry_claim_is_single_use_and_completion_cleans_both_indexes() {
        let mut store = AttemptStore::default();
        store.insert("trace".into(), attempt());
        assert!(store.claim_retry("trace").is_some());
        assert!(store.claim_retry("trace").is_none());
        assert!(store.authorize_retry("trace", "attempt".into(), "backup".into()));
        assert_eq!(
            store.complete("attempt").unwrap().credential_name,
            "primary"
        );
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn abandoned_attempts_expire_without_leaving_retry_ids() {
        let mut store = AttemptStore::default();
        store.insert("old-trace".into(), attempt());
        assert!(store.claim_retry("old-trace").is_some());
        assert!(store.authorize_retry("old-trace", "old-attempt".into(), "backup".into()));
        assert_eq!(store.len(), 1);

        store
            .by_traceparent
            .get_mut("old-trace")
            .unwrap()
            .created_at = Instant::now() - ATTEMPT_TTL;
        assert_eq!(store.len(), 0);
        assert!(store.complete("old-attempt").is_none());
    }

    #[test]
    fn clearing_attempts_invalidates_both_indexes() {
        let mut store = AttemptStore::default();
        store.insert("trace".into(), attempt());
        assert!(store.claim_retry("trace").is_some());
        assert!(store.authorize_retry("trace", "attempt".into(), "backup".into()));
        store.clear();
        assert_eq!(store.len(), 0);
        assert!(store.complete("attempt").is_none());
    }
}
