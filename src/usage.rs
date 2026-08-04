use crate::{Error, Result};
use chrono::{DateTime, Utc};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use std::time::Duration;

pub const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const OAUTH_BETA: &str = "oauth-2025-04-20";

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct UsageWindow {
    pub utilization: Option<f64>,
    pub resets_at: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ExtraUsage {
    pub is_enabled: Option<bool>,
    pub monthly_limit: Option<f64>,
    pub used_credits: Option<f64>,
    pub utilization: Option<f64>,
    pub currency: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LimitScopeModel {
    pub id: Option<String>,
    pub display_name: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LimitScope {
    pub model: Option<LimitScopeModel>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct UsageLimit {
    pub kind: Option<String>,
    pub group: Option<String>,
    pub percent: Option<f64>,
    pub resets_at: Option<String>,
    pub scope: Option<LimitScope>,
    pub is_active: Option<bool>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct UsageSnapshot {
    pub five_hour: Option<UsageWindow>,
    pub seven_day: Option<UsageWindow>,
    pub seven_day_opus: Option<UsageWindow>,
    pub seven_day_sonnet: Option<UsageWindow>,
    pub extra_usage: Option<ExtraUsage>,
    #[serde(default)]
    pub limits: Vec<UsageLimit>,
}

impl UsageSnapshot {
    pub fn tightest_utilization(&self, model: Option<&str>) -> Option<f64> {
        self.tightest_utilization_at(model, Utc::now())
    }

    fn tightest_utilization_at(&self, model: Option<&str>, now: DateTime<Utc>) -> Option<f64> {
        let mut values = Vec::new();
        values.extend(current_window_utilization(self.five_hour.as_ref(), now));
        values.extend(current_window_utilization(self.seven_day.as_ref(), now));

        let model = model.unwrap_or_default().to_ascii_lowercase();
        if model.contains("opus") {
            values.extend(current_window_utilization(
                self.seven_day_opus.as_ref(),
                now,
            ));
        }
        if model.contains("sonnet") {
            values.extend(current_window_utilization(
                self.seven_day_sonnet.as_ref(),
                now,
            ));
        }
        for limit in &self.limits {
            if limit.is_active == Some(false) || reset_has_passed(limit.resets_at.as_deref(), now) {
                continue;
            }
            let applies = limit
                .scope
                .as_ref()
                .and_then(|scope| scope.model.as_ref())
                .map(|scope_model| {
                    let id = scope_model
                        .id
                        .as_deref()
                        .unwrap_or_default()
                        .to_ascii_lowercase();
                    let display = scope_model
                        .display_name
                        .as_deref()
                        .unwrap_or_default()
                        .to_ascii_lowercase();
                    model.is_empty() || model.contains(&id) || model.contains(&display)
                })
                .unwrap_or(true);
            if applies {
                values.extend(limit.percent);
            }
        }
        values.into_iter().reduce(f64::max)
    }

    pub fn eligible(&self, model: Option<&str>, reserve_percent: f64) -> bool {
        self.tightest_utilization(model)
            .map(|used| used < 100.0 - reserve_percent)
            .unwrap_or(true)
    }
}

fn current_window_utilization(window: Option<&UsageWindow>, now: DateTime<Utc>) -> Option<f64> {
    window
        .filter(|window| !reset_has_passed(window.resets_at.as_deref(), now))
        .and_then(|window| window.utilization)
}

fn reset_has_passed(reset: Option<&str>, now: DateTime<Utc>) -> bool {
    reset
        .and_then(|reset| DateTime::parse_from_rfc3339(reset).ok())
        .is_some_and(|reset| reset <= now)
}

#[derive(Clone)]
pub struct UsageClient {
    client: reqwest::Client,
    base_url: String,
    user_agent: String,
}

impl UsageClient {
    pub fn new(client: reqwest::Client, claude_version: Option<&str>) -> Self {
        Self::with_base_url(client, USAGE_URL, claude_version)
    }

    pub fn with_base_url(
        client: reqwest::Client,
        base_url: impl Into<String>,
        claude_version: Option<&str>,
    ) -> Self {
        Self {
            client,
            base_url: base_url.into(),
            user_agent: format!("claude-code/{}", claude_version.unwrap_or("2.1.0")),
        }
    }

    pub async fn fetch(&self, access_token: &str) -> Result<UsageSnapshot> {
        let response = self
            .client
            .get(&self.base_url)
            .bearer_auth(access_token)
            .header("anthropic-beta", OAUTH_BETA)
            .header("user-agent", &self.user_agent)
            .header("accept", "application/json")
            .header("content-type", "application/json")
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|error| Error::audit_transient(format!("usage request failed: {error}")))?;

        match response.status() {
            StatusCode::OK => response.json().await.map_err(|error| {
                Error::audit_transient(format!("invalid usage response: {error}"))
            }),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(Error::audit_fatal(
                "OAuth token is unauthorized or lacks user:profile scope",
            )),
            StatusCode::TOO_MANY_REQUESTS => {
                let retry = response
                    .headers()
                    .get("retry-after")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or("300");
                Err(Error::audit_transient(format!(
                    "usage endpoint rate limited; retry after {retry}"
                )))
            }
            status => {
                let body = response.text().await.unwrap_or_default();
                let message = format!(
                    "usage endpoint returned {status}: {}",
                    body.chars().take(300).collect::<String>()
                );
                if status.is_server_error() {
                    Err(Error::audit_transient(message))
                } else {
                    Err(Error::audit_fatal(message))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tightest_window_is_model_aware() {
        let usage: UsageSnapshot = serde_json::from_value(serde_json::json!({
            "five_hour": {"utilization": 20.0, "resets_at": null},
            "seven_day": {"utilization": 40.0, "resets_at": null},
            "seven_day_opus": {"utilization": 99.5, "resets_at": null},
            "seven_day_sonnet": {"utilization": 50.0, "resets_at": null}
        }))
        .unwrap();
        assert_eq!(
            usage.tightest_utilization(Some("claude-opus-4")),
            Some(99.5)
        );
        assert_eq!(
            usage.tightest_utilization(Some("claude-sonnet-4")),
            Some(50.0)
        );
        assert!(!usage.eligible(Some("claude-opus-4"), 1.0));
        assert!(usage.eligible(Some("claude-sonnet-4"), 1.0));
    }

    #[test]
    fn expired_windows_do_not_keep_credentials_blocked() {
        let usage: UsageSnapshot = serde_json::from_value(serde_json::json!({
            "five_hour": {
                "utilization": 100.0,
                "resets_at": "2030-08-01T12:00:00Z"
            },
            "seven_day": {
                "utilization": 40.0,
                "resets_at": "2030-08-08T13:00:00Z"
            }
        }))
        .unwrap();
        let now = DateTime::parse_from_rfc3339("2030-08-01T13:00:00Z")
            .unwrap()
            .to_utc();

        assert_eq!(usage.tightest_utilization_at(None, now), Some(40.0));
    }

    #[test]
    fn expired_model_limit_does_not_keep_credentials_blocked() {
        let usage: UsageSnapshot = serde_json::from_value(serde_json::json!({
            "five_hour": {"utilization": 20.0, "resets_at": null},
            "limits": [{
                "percent": 100.0,
                "resets_at": "2030-08-01T12:00:00Z",
                "scope": {"model": {"id": "claude-sonnet"}}
            }]
        }))
        .unwrap();
        let now = DateTime::parse_from_rfc3339("2030-08-01T13:00:00Z")
            .unwrap()
            .to_utc();

        assert_eq!(
            usage.tightest_utilization_at(Some("claude-sonnet-4"), now),
            Some(20.0)
        );
    }
}
