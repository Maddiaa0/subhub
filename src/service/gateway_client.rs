//! HTTP client for the running gateway's local admin endpoints.

use super::BASE_URL;
use crate::gateway::protocol::GatewayStatus;
use crate::{Error, Result, credential_read};

pub(crate) fn read_gateway_token() -> Result<String> {
    credential_read(super::GATEWAY_SERVICE, super::GATEWAY_TOKEN_ACCOUNT)
}

pub(crate) fn select_gateway_account(name: &str) -> Result<bool> {
    let Ok(token) = read_gateway_token() else {
        return Ok(false);
    };
    crate::runtime()?.block_on(async move {
        let response = match reqwest::Client::new()
            .post(format!("{BASE_URL}/_subhub/select"))
            .bearer_auth(token)
            .json(&serde_json::json!({"name": name}))
            .timeout(std::time::Duration::from_secs(2))
            .send()
            .await
        {
            Ok(response) => response,
            Err(_) => return Ok(false),
        };
        if response.status().is_success() {
            Ok(true)
        } else {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            Err(Error::Message(format!(
                "gateway returned {status}: {}",
                body.chars().take(200).collect::<String>()
            )))
        }
    })
}

/// Ask a running gateway to re-read the vault. Returns Ok(false) when no
/// gateway is reachable, and an error when one is reachable but refuses.
pub(crate) fn reload_gateway_accounts() -> Result<bool> {
    let Ok(token) = read_gateway_token() else {
        return Ok(false);
    };
    crate::runtime()?.block_on(async move {
        let response = match reqwest::Client::new()
            .post(format!("{BASE_URL}/_subhub/reload"))
            .bearer_auth(token)
            // The gateway audits every credential before replying.
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await
        {
            Ok(response) => response,
            Err(_) => return Ok(false),
        };
        if response.status().is_success() {
            Ok(true)
        } else {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            Err(Error::Message(format!(
                "gateway returned {status}: {}",
                body.chars().take(200).collect::<String>()
            )))
        }
    })
}

pub(super) fn fetch_gateway_status() -> Result<GatewayStatus> {
    let token = read_gateway_token()?;
    crate::runtime()?.block_on(async move {
        let response = reqwest::Client::new()
            .get(format!("{BASE_URL}/_subhub/status"))
            .bearer_auth(token)
            .timeout(std::time::Duration::from_secs(2))
            .send()
            .await
            .map_err(|error| Error::Message(error.to_string()))?;
        if !response.status().is_success() {
            return Err(Error::Message(format!("HTTP {}", response.status())));
        }
        response
            .json()
            .await
            .map_err(|error| Error::Message(error.to_string()))
    })
}
