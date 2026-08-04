//! Wire types for the gateway's `/_subhub/status` endpoint, shared by the
//! server handler and every CLI consumer (`status`, `doctor`, `statusline`).
//! Changing a field here changes the endpoint's JSON shape — the compiler
//! keeps both sides in sync.

use crate::codex;
use crate::error::CredentialError;
use crate::provider::Provider;
use crate::usage::UsageSnapshot;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Default, Deserialize, Serialize)]
pub(crate) struct GatewayStatus {
    #[serde(default)]
    pub selected: SelectedReport,
    #[serde(default)]
    pub credentials: BTreeMap<String, CredentialReport>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(crate) struct SelectedReport {
    pub claude: Option<String>,
    pub codex: Option<String>,
}

impl SelectedReport {
    pub(crate) fn for_provider(&self, provider: Provider) -> Option<&str> {
        match provider {
            Provider::Claude => self.claude.as_deref(),
            Provider::Codex => self.codex.as_deref(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct CredentialReport {
    /// None when the health entry has no matching stored credential.
    pub provider: Option<Provider>,
    #[serde(default)]
    pub token_state: TokenState,
    pub token_expires_at: Option<i64>,
    pub usage: Option<CredentialUsage>,
    pub error: Option<CredentialError>,
    pub checked_at: u64,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TokenState {
    Expired,
    RefreshDue,
    Valid,
    #[default]
    Unknown,
}

impl TokenState {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Expired => "expired",
            Self::RefreshDue => "refresh_due",
            Self::Valid => "valid",
            Self::Unknown => "unknown",
        }
    }
}

/// Usage snapshot for either provider. Untagged: the Codex variant is listed
/// first because its `rate_limit` field is required, while every Claude field
/// is optional — Claude-first would swallow Codex payloads as an empty
/// Claude snapshot during deserialization.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub(crate) enum CredentialUsage {
    Codex(codex::UsageSnapshot),
    Claude(UsageSnapshot),
}

impl CredentialUsage {
    pub(crate) fn eligible(&self, model: Option<&str>, reserve: f64) -> bool {
        match self {
            Self::Claude(usage) => usage.eligible(model, reserve),
            Self::Codex(usage) => usage.eligible(reserve),
        }
    }
    pub(crate) fn utilization(&self, model: Option<&str>) -> f64 {
        match self {
            Self::Claude(usage) => usage.tightest_utilization(model).unwrap_or(0.0),
            Self::Codex(usage) => usage.tightest_utilization(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn untagged_usage_distinguishes_providers() {
        let claude: CredentialUsage = serde_json::from_value(serde_json::json!({
            "five_hour": {"utilization": 20.0, "resets_at": null}
        }))
        .unwrap();
        assert!(matches!(claude, CredentialUsage::Claude(_)));

        let codex: CredentialUsage = serde_json::from_value(serde_json::json!({
            "rate_limit": {"primary_window": {"used_percent": 55.0, "reset_at": 1}}
        }))
        .unwrap();
        assert!(matches!(codex, CredentialUsage::Codex(_)));
    }
}
