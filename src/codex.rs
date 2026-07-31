use crate::{AppError, Result};
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};

pub const RESPONSES_UPSTREAM: &str = "https://chatgpt.com/backend-api/codex";
const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct UsageWindow {
    pub used_percent: Option<f64>,
    pub reset_at: Option<i64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct UsageSnapshot {
    pub rate_limit: RateLimit,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct RateLimit {
    pub primary_window: Option<UsageWindow>,
    pub secondary_window: Option<UsageWindow>,
}

impl UsageSnapshot {
    pub fn eligible(&self, reserve_percent: f64) -> bool {
        [
            &self.rate_limit.primary_window,
            &self.rate_limit.secondary_window,
        ]
        .into_iter()
        .flatten()
        .filter_map(|window| window.used_percent)
        .all(|used| used < 100.0 - reserve_percent)
    }

    pub fn tightest_utilization(&self) -> f64 {
        [
            &self.rate_limit.primary_window,
            &self.rate_limit.secondary_window,
        ]
        .into_iter()
        .flatten()
        .filter_map(|window| window.used_percent)
        .fold(0.0, f64::max)
    }
}

pub async fn fetch_usage(
    client: &reqwest::Client,
    access_token: &str,
    account_id: &str,
) -> Result<UsageSnapshot> {
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {access_token}"))
            .map_err(|error| AppError(error.to_string()))?,
    );
    headers.insert(
        "chatgpt-account-id",
        HeaderValue::from_str(account_id).map_err(|error| AppError(error.to_string()))?,
    );
    let response = client
        .get(USAGE_URL)
        .headers(headers)
        .send()
        .await
        .map_err(|error| AppError(format!("Codex usage audit failed: {error}")))?;
    if !response.status().is_success() {
        return Err(AppError(format!(
            "Codex usage audit returned HTTP {}",
            response.status()
        )));
    }
    response
        .json()
        .await
        .map_err(|error| AppError(format!("invalid Codex usage response: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_windows_control_codex_eligibility() {
        let usage: UsageSnapshot = serde_json::from_value(serde_json::json!({
            "rate_limit": {
                "primary_window": {"used_percent": 25, "reset_at": 1},
                "secondary_window": {"used_percent": 99.5, "reset_at": 2}
            }
        }))
        .unwrap();
        assert_eq!(usage.tightest_utilization(), 99.5);
        assert!(!usage.eligible(1.0));
        assert!(usage.eligible(0.1));
    }
}
