//! Crate-wide typed errors. Routing decisions depend on error *kinds*
//! (refresh, audit, or inference), never on matching message text.

use serde::{Deserialize, Serialize};

pub type Result<T> = std::result::Result<T, Error>;

/// Crate-wide error type.
///
/// Variants exist for the failures the gateway makes routing decisions on;
/// everything else is a `Message`. When adding a variant, decide how it maps
/// into [`ErrorKind`] so credential health reporting stays accurate.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// An OAuth token refresh failed because of a temporary transport or
    /// provider problem and may be retried after backoff.
    #[error("{0}")]
    RefreshTransient(String),
    /// An OAuth token refresh failed permanently. Retrying a rotating refresh
    /// token after this point can invalidate a newer token in the same family.
    #[error("{0}")]
    RefreshTerminal(String),
    /// A usage audit failed. `transient` distinguishes infrastructure hiccups
    /// (timeouts, rate limits, upstream 5xx) that say nothing about the
    /// credential from failures that mean the credential itself is unusable
    /// (401/403, missing scope, missing account id).
    #[error("{message}")]
    Audit { transient: bool, message: String },
    #[error("{0}")]
    Message(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Json(#[from] serde_json::Error),
}

impl Error {
    pub(crate) fn refresh_transient(message: impl Into<String>) -> Self {
        Self::RefreshTransient(message.into())
    }

    pub(crate) fn refresh_terminal(message: impl Into<String>) -> Self {
        Self::RefreshTerminal(message.into())
    }

    pub(crate) fn refresh_is_terminal(&self) -> bool {
        matches!(self, Self::RefreshTerminal(_))
    }

    pub(crate) fn audit_transient(message: impl Into<String>) -> Self {
        Self::Audit {
            transient: true,
            message: message.into(),
        }
    }

    pub(crate) fn audit_fatal(message: impl Into<String>) -> Self {
        Self::Audit {
            transient: false,
            message: message.into(),
        }
    }

    pub(crate) fn kind(&self) -> ErrorKind {
        match self {
            Self::RefreshTransient(_) | Self::RefreshTerminal(_) => ErrorKind::Refresh,
            Self::Audit {
                transient: true, ..
            } => ErrorKind::TransientAudit,
            Self::Audit {
                transient: false, ..
            } => ErrorKind::FatalAudit,
            // Unknown failures must not make a credential look routable, so
            // anything unclassified counts as fatal for health purposes.
            Self::Message(_) | Self::Io(_) | Self::Json(_) => ErrorKind::FatalAudit,
        }
    }
}

/// Classification of a credential's last recorded failure, serialized into
/// the gateway status endpoint.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ErrorKind {
    Refresh,
    TransientAudit,
    FatalAudit,
    Inference,
}

/// A credential failure as stored in gateway health and reported over the
/// status endpoint: machine-readable kind plus human-readable message.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct CredentialError {
    pub kind: ErrorKind,
    pub message: String,
}

impl From<&Error> for CredentialError {
    fn from(error: &Error) -> Self {
        Self {
            kind: error.kind(),
            message: error.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_classify_routing_relevant_errors() {
        assert_eq!(Error::refresh_transient("x").kind(), ErrorKind::Refresh);
        assert_eq!(Error::refresh_terminal("x").kind(), ErrorKind::Refresh);
        assert!(Error::refresh_terminal("x").refresh_is_terminal());
        assert!(!Error::refresh_transient("x").refresh_is_terminal());
        assert_eq!(
            Error::audit_transient("x").kind(),
            ErrorKind::TransientAudit
        );
        assert_eq!(Error::audit_fatal("x").kind(), ErrorKind::FatalAudit);
        // Unclassified errors must not look routable.
        assert_eq!(Error::Message("x".into()).kind(), ErrorKind::FatalAudit);
    }

    #[test]
    fn error_kind_serializes_snake_case() {
        assert_eq!(
            serde_json::to_value(ErrorKind::TransientAudit).unwrap(),
            serde_json::json!("transient_audit")
        );
    }
}
