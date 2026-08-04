//! Crate-wide typed errors. Routing decisions depend on error *kinds*
//! (refresh vs transient vs fatal audit), never on matching message text.

use serde::{Deserialize, Serialize};

pub type Result<T> = std::result::Result<T, Error>;

/// Crate-wide error type.
///
/// Variants exist for the failures the gateway makes routing decisions on;
/// everything else is a `Message`. When adding a variant, decide how it maps
/// into [`ErrorKind`] so credential health reporting stays accurate.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// An OAuth token refresh failed. The credential likely needs
    /// re-authentication (`subhub add <name> --force`).
    #[error("{0}")]
    Refresh(String),
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
            Self::Refresh(_) => ErrorKind::Refresh,
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
        assert_eq!(Error::Refresh("x".into()).kind(), ErrorKind::Refresh);
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
