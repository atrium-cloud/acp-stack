//! Small reusable validators and helpers used across the per-domain validators.
//!
//! These intentionally avoid pulling in domain knowledge: they validate types
//! (durations, sockets, paths, sha256, env names, secret refs) and surface
//! generic `StackError` variants keyed on a field name supplied by the caller.

use std::net::SocketAddr;
use std::path::Path;

use crate::config::schema::AuthConfig;
use crate::error::{Result, StackError};

/// Parse a duration string like "10m", "5s", "2h", "1d", "4w", "750ms". Returns `None` on
/// any invalid input. Empty string and pure-numeric inputs (no suffix) are
/// rejected so config typos surface at load time rather than meaning seconds.
pub fn parse_duration_string(input: &str) -> Option<std::time::Duration> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    let (number_part, unit_part) = trimmed
        .find(|c: char| !c.is_ascii_digit())
        .map(|idx| trimmed.split_at(idx))?;
    if number_part.is_empty() {
        return None;
    }
    let value: u64 = number_part.parse().ok()?;
    match unit_part {
        "ms" => Some(std::time::Duration::from_millis(value)),
        "s" => Some(std::time::Duration::from_secs(value)),
        "m" => Some(std::time::Duration::from_secs(value.checked_mul(60)?)),
        "h" => Some(std::time::Duration::from_secs(value.checked_mul(3_600)?)),
        "d" => Some(std::time::Duration::from_secs(value.checked_mul(86_400)?)),
        "w" => Some(std::time::Duration::from_secs(value.checked_mul(604_800)?)),
        _ => None,
    }
}

/// Compare two `[auth]` blocks and return an error if either ref name changed.
/// Used by both `acps config import` (CLI) and `POST /v1/config/import` to
/// uphold the "admin key never regenerable in place" + "session key only
/// rotated via `acps auth regenerate-session-key`" invariants.
pub fn compare_auth_refs(current: &AuthConfig, incoming: &AuthConfig) -> Result<()> {
    if current.session_key_ref != incoming.session_key_ref {
        return Err(StackError::ImportChangesAuthRef {
            field: "session_key_ref",
            current: current.session_key_ref.clone(),
            incoming: incoming.session_key_ref.clone(),
        });
    }
    if current.admin_key_ref != incoming.admin_key_ref {
        return Err(StackError::ImportChangesAuthRef {
            field: "admin_key_ref",
            current: current.admin_key_ref.clone(),
            incoming: incoming.admin_key_ref.clone(),
        });
    }
    Ok(())
}

pub(crate) fn validate_auth_refs(auth: &AuthConfig) -> Result<()> {
    let session = auth.session_key_ref.trim();
    let admin = auth.admin_key_ref.trim();
    if session.is_empty() {
        return Err(StackError::MissingField {
            field: "auth.session_key_ref",
        });
    }
    if admin.is_empty() {
        return Err(StackError::MissingField {
            field: "auth.admin_key_ref",
        });
    }
    // Distinct refs are a hard invariant: if they alias, generating both keys
    // writes the second over the first, and `acps auth regenerate-session-key`
    // rotates the admin key, collapsing the session/admin boundary.
    if session == admin {
        return Err(StackError::AuthRefsNotDistinct);
    }
    // Auth refs are themselves stored in the secret store under these names,
    // so they must follow the same identifier rules as every other ref.
    // Otherwise an auth_ref like "weird name" could silently fail to round-
    // trip through the store on init.
    validate_secret_ref_name_value(session)?;
    validate_secret_ref_name_value(admin)?;
    Ok(())
}

pub(crate) fn validate_socket_address(field: &'static str, value: &str) -> Result<()> {
    value
        .parse::<SocketAddr>()
        .map(|_| ())
        .map_err(|_| StackError::InvalidSocketAddress { field })
}

pub(crate) fn validate_nonzero(field: &'static str, value: u64) -> Result<()> {
    if value == 0 {
        return Err(StackError::NonZeroRequired { field });
    }

    Ok(())
}

pub(crate) fn validate_absolute_path(field: &'static str, value: &str) -> Result<()> {
    if !Path::new(value).is_absolute() {
        return Err(StackError::PathMustBeAbsolute { field });
    }

    Ok(())
}

/// `Path::starts_with` is purely lexical — `/workspace/../etc/uploads`
/// "starts with" `/workspace` even though it resolves outside. Reject `..`
/// segments in the configured paths up front so the workspace-root/uploads
/// containment check below cannot be tricked, and so request-time path
/// resolution does not have to canonicalize the config paths repeatedly.
pub(crate) fn validate_no_parent_dir_segments(field: &'static str, value: &str) -> Result<()> {
    for component in Path::new(value).components() {
        if matches!(component, std::path::Component::ParentDir) {
            return Err(StackError::PathContainsParentDir { field });
        }
    }
    Ok(())
}

pub(crate) fn require_present<'a>(field: &'static str, value: Option<&'a str>) -> Result<&'a str> {
    match value {
        Some(value) if !value.trim().is_empty() => Ok(value),
        _ => Err(StackError::MissingField { field }),
    }
}

pub(crate) fn validate_expected_sha256(value: &str) -> Result<()> {
    if value.len() == 64 && value.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')) {
        Ok(())
    } else {
        Err(StackError::InvalidExpectedSha256)
    }
}

pub(crate) fn validate_nonempty(field: &'static str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(StackError::MissingField { field });
    }
    Ok(())
}

pub(crate) fn validate_non_empty_trimmed(field: &'static str, value: &str) -> Result<()> {
    if value.trim().is_empty() || value.trim().len() != value.len() {
        return Err(StackError::MissingField { field });
    }
    Ok(())
}

pub(crate) fn validate_http_url_prefix(field: &'static str, value: &str) -> Result<()> {
    if value.starts_with("http://") || value.starts_with("https://") {
        return Ok(());
    }
    Err(StackError::UrlMustBeHttp { field })
}

pub(crate) fn validate_optional_config_path(field: &'static str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(StackError::MissingField { field });
    }
    if !Path::new(value).is_absolute() {
        return Err(StackError::PathMustBeAbsolute { field });
    }
    for component in Path::new(value).components() {
        if matches!(component, std::path::Component::ParentDir) {
            return Err(StackError::PathContainsParentDir { field });
        }
    }
    Ok(())
}

pub(crate) fn validate_secret_ref_name_value(name: &str) -> Result<()> {
    if !is_valid_secret_ref_name(name) {
        return Err(StackError::InvalidSecretRefName {
            name: name.to_owned(),
        });
    }
    Ok(())
}

/// Accept identifier-like names: ASCII letters, digits, and underscores; must
/// not be empty and must not start with a digit. Matches the spirit of POSIX
/// env-var names and the auth-key naming used elsewhere in the project.
pub fn is_valid_secret_ref_name(name: &str) -> bool {
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed.len() != name.len() {
        return false;
    }
    let mut chars = trimmed.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

pub(crate) fn secret_ref_looks_like_value(name: &str) -> bool {
    if name.len() > 128 {
        return true;
    }
    if name.len() > 40 && name.chars().all(|c| c.is_ascii_hexdigit()) {
        return true;
    }
    if name.starts_with("acps_")
        || name.starts_with("sk-")
        || name.starts_with("ghp_")
        || name.starts_with("github_pat_")
        || name.starts_with("xoxb-")
        || name.starts_with("xoxp-")
        || name.starts_with("xoxa-")
    {
        return true;
    }
    let jwt_parts = name.split('.').collect::<Vec<_>>();
    if jwt_parts.len() == 3
        && jwt_parts
            .iter()
            .all(|part| part.len() >= 10 && part.chars().all(is_base64url_char))
    {
        return true;
    }
    false
}

fn is_base64url_char(value: char) -> bool {
    value.is_ascii_alphanumeric() || value == '_' || value == '-'
}
