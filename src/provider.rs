//! The provider seam: [`Provider`] and every behavior that differs between
//! Claude and Codex.

use crate::codex;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Provider {
    #[default]
    Claude,
    Codex,
}

/// Everything the gateway does differently per provider lives in this impl.
/// Adding a provider means adding a variant and letting the compiler point
/// at each exhaustive match that needs a decision.
impl Provider {
    /// Lowercase identifier, matching the serde representation.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    /// Human-facing name for error messages.
    pub(crate) fn display_name(self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::Codex => "Codex",
        }
    }

    /// Which provider a gateway request is destined for, from its path.
    pub(crate) fn from_request_path(path: &str) -> Self {
        if path.starts_with("/openai/") {
            Self::Codex
        } else {
            Self::Claude
        }
    }

    /// Upstream base URL requests are forwarded to.
    pub(crate) fn upstream(self) -> &'static str {
        match self {
            Self::Claude => "https://api.anthropic.com",
            Self::Codex => codex::RESPONSES_UPSTREAM,
        }
    }

    /// Rewrite an incoming gateway path into the upstream's path space.
    pub(crate) fn rewrite_upstream_path(self, path: &str) -> &str {
        match self {
            Self::Claude => path,
            Self::Codex => path.strip_prefix("/openai").unwrap_or(path),
        }
    }

    /// Whether the gateway can refresh this provider's OAuth tokens itself.
    pub(crate) fn supports_refresh(self) -> bool {
        match self {
            Self::Claude => true,
            Self::Codex => false,
        }
    }
}

/// A credential as loaded from the vault for use by the gateway: just the
/// fields routing and auditing need, never the refresh token.
#[derive(Clone, Debug)]
pub(crate) struct StoredCredential {
    pub name: String,
    pub access_token: String,
    pub expires_at: Option<i64>,
    pub scopes: Vec<String>,
    pub provider: Provider,
    pub account_id: Option<String>,
    /// Persisted terminal refresh state. This contains no token material and
    /// prevents a restarted gateway from reusing a rejected refresh token.
    pub refresh_error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_paths_route_to_the_right_provider() {
        assert_eq!(
            Provider::from_request_path("/v1/messages"),
            Provider::Claude
        );
        assert_eq!(
            Provider::from_request_path("/openai/responses"),
            Provider::Codex
        );
    }

    #[test]
    fn codex_paths_are_rewritten_for_upstream() {
        assert_eq!(
            Provider::Codex.rewrite_upstream_path("/openai/responses"),
            "/responses"
        );
        assert_eq!(
            Provider::Claude.rewrite_upstream_path("/v1/messages"),
            "/v1/messages"
        );
    }
}
