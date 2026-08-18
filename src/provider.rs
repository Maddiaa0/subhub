//! The provider seam: [`Provider`] and every behavior that differs between
//! Claude and Codex.

use crate::codex;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Provider {
    #[default]
    Claude,
    Codex,
}

/// The single inference endpoint on which a provider credential may be used.
/// Both transports and the generated Iron policy derive their routing rules
/// from this descriptor so adding a provider cannot leave a second endpoint
/// table silently out of sync.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct InferenceEndpoint {
    pub(crate) host: &'static str,
    pub(crate) method: &'static str,
    pub(crate) path: &'static str,
}

/// Everything the gateway does differently per provider lives in this impl.
/// Adding a provider means adding a variant and letting the compiler point
/// at each exhaustive match that needs a decision.
impl Provider {
    pub(crate) fn all() -> &'static [Self] {
        <Self as clap::ValueEnum>::value_variants()
    }

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

    /// Exact public endpoint authorized to receive this provider's identity in
    /// Iron mode.
    pub(crate) fn inference_endpoint(self) -> InferenceEndpoint {
        match self {
            Self::Claude => InferenceEndpoint {
                host: "api.anthropic.com",
                method: "POST",
                path: "/v1/messages",
            },
            Self::Codex => InferenceEndpoint {
                host: "chatgpt.com",
                method: "POST",
                path: "/backend-api/codex/responses",
            },
        }
    }

    pub(crate) fn from_inference_endpoint(host: &str, method: &str, path: &str) -> Option<Self> {
        Self::all().iter().copied().find(|provider| {
            let endpoint = provider.inference_endpoint();
            endpoint.host == host
                && endpoint.method.eq_ignore_ascii_case(method)
                && endpoint.path == path
        })
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

    #[test]
    fn inference_endpoints_round_trip_to_their_provider() {
        for provider in Provider::all() {
            let endpoint = provider.inference_endpoint();
            assert_eq!(
                Provider::from_inference_endpoint(endpoint.host, endpoint.method, endpoint.path),
                Some(*provider)
            );
        }
    }
}
