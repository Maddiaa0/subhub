use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Provider {
    #[default]
    Claude,
    Codex,
}

pub(crate) fn provider_name(provider: Provider) -> &'static str {
    match provider {
        Provider::Claude => "claude",
        Provider::Codex => "codex",
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
}
