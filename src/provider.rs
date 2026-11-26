//! The provider seam: [`Provider`] and every behavior that differs across
//! Claude, Codex, and Antigravity CLI.

use crate::codex;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Provider {
    #[default]
    Claude,
    Codex,
    Antigravity,
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
            Self::Antigravity => "antigravity",
        }
    }

    /// Human-facing name for error messages.
    pub(crate) fn display_name(self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::Codex => "Codex",
            Self::Antigravity => "Antigravity",
        }
    }

    /// Whether requests for this provider are proxied through the Subhub
    /// gateway (as opposed to switching the CLI's active credential file).
    pub(crate) fn uses_gateway(self) -> bool {
        match self {
            Self::Claude | Self::Codex => true,
            Self::Antigravity => false,
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

    /// Upstream base URL requests are forwarded to, if gateway-routed.
    pub(crate) fn upstream(self) -> Option<&'static str> {
        match self {
            Self::Claude => Some("https://api.anthropic.com"),
            Self::Codex => Some(codex::RESPONSES_UPSTREAM),
            Self::Antigravity => None,
        }
    }

    /// Rewrite an incoming gateway path into the upstream's path space.
    pub(crate) fn rewrite_upstream_path(self, path: &str) -> &str {
        match self {
            Self::Claude | Self::Antigravity => path,
            Self::Codex => path.strip_prefix("/openai").unwrap_or(path),
        }
    }

    /// Whether the gateway can refresh this provider's OAuth tokens itself.
    pub(crate) fn supports_refresh(self) -> bool {
        match self {
            Self::Claude => true,
            Self::Codex | Self::Antigravity => false,
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

    #[test]
    fn antigravity_is_file_switched_not_gateway_routed() {
        assert_eq!(Provider::Antigravity.name(), "antigravity");
        assert_eq!(Provider::Antigravity.display_name(), "Antigravity");
        assert!(!Provider::Antigravity.uses_gateway());
        assert!(Provider::Antigravity.upstream().is_none());
        assert!(!Provider::Antigravity.supports_refresh());
        assert!(Provider::Claude.uses_gateway());
        assert!(Provider::Codex.uses_gateway());
    }
}
