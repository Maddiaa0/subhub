//! Antigravity CLI (`agy`) provider specifics: token-file path resolution,
//! credential validation, identity matching, and atomic file I/O.
//!
//! Unlike Claude Code and Codex, Antigravity CLI is not routed through the
//! local gateway; Subhub manages multiple accounts by storing token snapshots
//! in the vault and atomically swapping `~/.gemini/antigravity-cli/antigravity-oauth-token`
//! when switching accounts with `subhub set`.

use crate::{Error, Result, save_json_file};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// Optional override for the active Antigravity CLI OAuth token file path.
pub(crate) const TOKEN_PATH_ENV: &str = "SUBHUB_ANTIGRAVITY_TOKEN_PATH";

/// Local status reported by `subhub audit` for file-switched Antigravity accounts.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct LocalStatus {
    pub mode: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_expires_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access_token_expired: Option<bool>,
}

impl LocalStatus {
    pub(crate) fn from_stored(credential: &crate::provider::StoredCredential) -> Self {
        let now_ms = Utc::now().timestamp_millis();
        Self {
            mode: "file_switched",
            email: credential.account_id.clone(),
            token_expires_at: credential.expires_at,
            access_token_expired: credential.expires_at.map(|expires_at| expires_at <= now_ms),
        }
    }
}

/// Resolve the active Antigravity CLI OAuth token file path.
///
/// Honors `SUBHUB_ANTIGRAVITY_TOKEN_PATH` when set and non-empty; otherwise
/// defaults to `$HOME/.gemini/antigravity-cli/antigravity-oauth-token`.
pub(crate) fn token_path() -> Result<PathBuf> {
    if let Some(override_path) = env::var_os(TOKEN_PATH_ENV).filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(override_path));
    }
    env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| token_path_in_home(&home))
        .ok_or_else(|| Error::Message("HOME is not set".into()))
}

pub(crate) fn token_path_in_home(home: &Path) -> PathBuf {
    home.join(".gemini")
        .join("antigravity-cli")
        .join("antigravity-oauth-token")
}

/// Validate the JSON payload stored in `antigravity-oauth-token`.
pub(crate) fn validate_credential(raw: &str) -> Result<Value> {
    let parsed: Value = serde_json::from_str(raw).map_err(|error| {
        Error::Message(format!("Antigravity credential is not valid JSON: {error}"))
    })?;
    validate_credential_value(&parsed)?;
    Ok(parsed)
}

pub(crate) fn validate_credential_value(parsed: &Value) -> Result<()> {
    if !parsed.is_object() {
        return Err(Error::Message(
            "Antigravity credential must be a JSON object".into(),
        ));
    }
    for key in ["access_token", "refresh_token"] {
        let present = parsed
            .pointer(&format!("/token/{key}"))
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty());
        if !present {
            return Err(Error::Message(format!(
                "Antigravity credential has no valid token.{key}"
            )));
        }
    }
    Ok(())
}

pub(crate) fn access_token(credential: &Value) -> Option<&str> {
    credential
        .pointer("/token/access_token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

pub(crate) fn refresh_token(credential: &Value) -> Option<&str> {
    credential
        .pointer("/token/refresh_token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

pub(crate) fn email(credential: &Value) -> Option<&str> {
    credential
        .get("id_token_email")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

pub(crate) fn expires_at_millis(credential: &Value) -> Option<i64> {
    credential
        .pointer("/token/expiry")
        .and_then(Value::as_str)
        .and_then(|expiry| DateTime::parse_from_rfc3339(expiry).ok())
        .map(|expiry| expiry.timestamp_millis())
}

pub(crate) fn same_identity(left: &Value, right: &Value) -> bool {
    if let (Some(left_email), Some(right_email)) = (email(left), email(right)) {
        return left_email.eq_ignore_ascii_case(right_email);
    }
    matches!(
        (refresh_token(left), refresh_token(right)),
        (Some(left_token), Some(right_token)) if left_token == right_token
    )
}

pub(crate) fn read_active_token(path: &Path) -> Result<Option<String>> {
    if !path.exists() {
        return Ok(None);
    }
    fs::read_to_string(path)
        .map(Some)
        .map_err(|error| Error::Message(format!("could not read {}: {error}", path.display())))
}

pub(crate) fn write_active_token(path: &Path, credential: &Value) -> Result<()> {
    validate_credential_value(credential)?;
    save_json_file(path, credential)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn sample_token(email: Option<&str>, refresh: &str) -> Value {
        let mut object = serde_json::json!({
            "token": {
                "access_token": "ya29.test-access",
                "token_type": "Bearer",
                "refresh_token": refresh,
                "expiry": "2030-08-01T13:00:00Z",
                "expires_in": 3599
            },
            "auth_method": "oauth"
        });
        if let Some(email) = email {
            object["id_token_email"] = Value::String(email.into());
        }
        object
    }

    #[test]
    fn validates_antigravity_oauth_token_shape() {
        let valid = sample_token(Some("user@example.com"), "1//refresh-a").to_string();
        assert!(validate_credential(&valid).is_ok());

        let missing_refresh = serde_json::json!({
            "token": {"access_token": "ya29.test"}
        })
        .to_string();
        assert!(validate_credential(&missing_refresh).is_err());

        let empty_access = serde_json::json!({
            "token": {"access_token": "", "refresh_token": "1//refresh"}
        })
        .to_string();
        assert!(validate_credential(&empty_access).is_err());

        assert!(validate_credential("[]").is_err());
    }

    #[test]
    fn extracts_email_and_rfc3339_expiry() {
        let token = sample_token(Some("user@example.com"), "1//refresh-a");
        assert_eq!(email(&token), Some("user@example.com"));
        assert_eq!(access_token(&token), Some("ya29.test-access"));
        assert_eq!(refresh_token(&token), Some("1//refresh-a"));
        assert_eq!(
            expires_at_millis(&token),
            Some(
                DateTime::parse_from_rfc3339("2030-08-01T13:00:00Z")
                    .unwrap()
                    .timestamp_millis()
            )
        );
    }

    #[test]
    fn identity_matches_email_case_insensitively_and_falls_back_to_refresh_token() {
        assert!(same_identity(
            &sample_token(Some("User@Example.com"), "1//old"),
            &sample_token(Some("user@example.com"), "1//new")
        ));
        assert!(!same_identity(
            &sample_token(Some("a@example.com"), "1//same"),
            &sample_token(Some("b@example.com"), "1//same")
        ));
        assert!(same_identity(
            &sample_token(None, "1//same"),
            &sample_token(None, "1//same")
        ));
        assert!(!same_identity(
            &sample_token(None, "1//one"),
            &sample_token(None, "1//two")
        ));
    }

    #[test]
    fn write_and_read_active_token_round_trips_with_private_permissions() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = env::temp_dir().join(format!(
            "subhub-antigravity-test-{}-{unique}",
            std::process::id()
        ));
        let path = token_path_in_home(&dir);
        let token = sample_token(Some("user@example.com"), "1//refresh-a");

        assert_eq!(read_active_token(&path).unwrap(), None);
        write_active_token(&path, &token).unwrap();

        let raw = read_active_token(&path).unwrap().unwrap();
        let parsed = validate_credential(&raw).unwrap();
        assert_eq!(parsed, token);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        fs::remove_dir_all(dir).unwrap();
    }
}
