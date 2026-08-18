//! Transport-neutral credential routing operations shared by the direct HTTP
//! gateway and Iron's external transform and response-retry adapters.

use super::refresh::refresh_credential;
use super::selection::{mark_failed, routing_error_message, select_credential};
use super::state::ProxyState;
use crate::provider::{Provider, StoredCredential};
use crate::{Error, Result};
use axum::http::header::{AUTHORIZATION, HeaderName};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use std::collections::HashMap;

const X_API_KEY: HeaderName = HeaderName::from_static("x-api-key");
const ANTHROPIC_BETA: HeaderName = HeaderName::from_static("anthropic-beta");
const OPENAI_BETA: HeaderName = HeaderName::from_static("openai-beta");
const CHATGPT_ACCOUNT_ID: HeaderName = HeaderName::from_static("chatgpt-account-id");
const CLAUDE_OAUTH_BETA: &str = "oauth-2025-04-20";

pub(super) fn request_model(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("model")
                .and_then(|value| value.as_str())
                .map(str::to_owned)
        })
}

pub(super) async fn select_initial(
    state: &ProxyState,
    provider: Provider,
    model: Option<&str>,
) -> Result<StoredCredential> {
    match select_credential(state, model, None, provider).await {
        Some(credential) => Ok(credential),
        None => Err(Error::Message(routing_error_message(state, provider).await)),
    }
}

pub(super) async fn refresh_after_unauthorized(
    state: &ProxyState,
    credential: &StoredCredential,
) -> Result<StoredCredential> {
    if !credential.provider.supports_refresh() {
        return Err(Error::Message(format!(
            "{} credentials cannot be refreshed by Subhub",
            credential.provider.display_name()
        )));
    }
    refresh_credential(
        state,
        &credential.name,
        true,
        Some(&credential.access_token),
    )
    .await
}

pub(super) async fn rotate_after_failure(
    state: &ProxyState,
    credential: &StoredCredential,
    model: Option<&str>,
    status: StatusCode,
) -> Option<StoredCredential> {
    mark_failed(state, &credential.name, status).await;
    select_credential(state, model, Some(&credential.name), credential.provider).await
}

/// Replace any client-supplied credential with the selected provider OAuth
/// identity while preserving unrelated headers.
pub(super) fn apply_credential_headers(
    headers: &mut HeaderMap,
    credential: &StoredCredential,
) -> Result<()> {
    headers.remove(AUTHORIZATION);
    headers.remove(X_API_KEY);
    let authorization = HeaderValue::from_str(&format!("Bearer {}", credential.access_token))
        .map_err(|error| Error::Message(format!("invalid OAuth access token: {error}")))?;
    headers.insert(AUTHORIZATION, authorization);

    match credential.provider {
        Provider::Claude => {
            headers.remove(OPENAI_BETA);
            headers.remove(CHATGPT_ACCOUNT_ID);
            let beta = claude_oauth_beta(headers);
            headers.insert(
                ANTHROPIC_BETA,
                HeaderValue::from_str(&beta).map_err(|error| {
                    Error::Message(format!("invalid Anthropic beta header: {error}"))
                })?,
            );
        }
        Provider::Codex => {
            headers.remove(ANTHROPIC_BETA);
            headers.insert(OPENAI_BETA, HeaderValue::from_static("codex-1"));
            if let Some(account) = &credential.account_id {
                headers.insert(
                    CHATGPT_ACCOUNT_ID,
                    HeaderValue::from_str(account).map_err(|error| {
                        Error::Message(format!("invalid Codex account id: {error}"))
                    })?,
                );
            } else {
                headers.remove(CHATGPT_ACCOUNT_ID);
            }
        }
    }
    Ok(())
}

/// Header overlays returned to Iron's exact replay. The original transformed
/// request already contains all non-credential headers, so only identity-bound
/// values are returned here.
pub(super) fn retry_header_overrides(
    credential: &StoredCredential,
) -> Result<HashMap<String, String>> {
    let mut headers = HashMap::from([(
        "Authorization".into(),
        format!("Bearer {}", credential.access_token),
    )]);
    if credential.provider == Provider::Codex {
        let account = credential
            .account_id
            .as_ref()
            .ok_or_else(|| Error::Message("Codex credential does not have an account id".into()))?;
        headers.insert("chatgpt-account-id".into(), account.clone());
        headers.insert("openai-beta".into(), "codex-1".into());
    }
    Ok(headers)
}

fn claude_oauth_beta(headers: &HeaderMap) -> String {
    let existing = headers
        .get(ANTHROPIC_BETA)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if existing
        .split(',')
        .map(str::trim)
        .any(|value| value == CLAUDE_OAUTH_BETA)
    {
        existing.to_owned()
    } else if existing.is_empty() {
        CLAUDE_OAUTH_BETA.into()
    } else {
        format!("{existing},{CLAUDE_OAUTH_BETA}")
    }
}

#[cfg(test)]
mod tests {
    use super::super::state::test_state;
    use super::*;

    #[test]
    fn model_is_read_without_requiring_a_valid_request_shape() {
        assert_eq!(
            request_model(br#"{"model":"claude-sonnet","messages":[]}"#).as_deref(),
            Some("claude-sonnet")
        );
        assert_eq!(request_model(b"not-json"), None);
    }

    #[test]
    fn claude_headers_replace_client_auth_and_preserve_features() {
        let state = test_state();
        let credential = state.credentials.blocking_read()[1].clone();
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer placeholder"),
        );
        headers.insert(X_API_KEY, HeaderValue::from_static("placeholder"));
        headers.insert(
            ANTHROPIC_BETA,
            HeaderValue::from_static("feature-a,feature-b"),
        );
        headers.insert("x-request-id", HeaderValue::from_static("request-1"));

        apply_credential_headers(&mut headers, &credential).unwrap();

        assert_eq!(headers[AUTHORIZATION], "Bearer secret-b");
        assert!(!headers.contains_key(X_API_KEY));
        assert_eq!(
            headers[ANTHROPIC_BETA],
            "feature-a,feature-b,oauth-2025-04-20"
        );
        assert_eq!(headers["x-request-id"], "request-1");
    }

    #[test]
    fn retry_overrides_only_include_identity_bound_headers() {
        let state = test_state();
        let credential = state.credentials.blocking_read()[1].clone();
        let headers = retry_header_overrides(&credential).unwrap();
        assert_eq!(headers.len(), 1);
        assert_eq!(headers["Authorization"], "Bearer secret-b");
    }
}
