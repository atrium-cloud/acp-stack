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

/// The largest duration any config field may express: the time elapsed since
/// the Unix epoch. Durations are used as `now - duration` windows (staleness,
/// auto-update skip), so a span longer than this would place the computed
/// cutoff before 1970-01-01 — meaningless for the timestamps this runtime
/// records. The bound grows with wall-clock time, so a config that validates
/// once stays valid. A system clock set before 1970 (degenerate) yields
/// `Duration::MAX`, which disables the cap rather than failing every load.
fn max_duration_since_epoch() -> std::time::Duration {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or(std::time::Duration::MAX)
}

/// Validate a duration-valued config field: it must parse via
/// [`parse_duration_string`] and must not exceed [`max_duration_since_epoch`].
/// Returns the parsed `Duration` so callers can apply their own extra checks
/// (e.g. non-zero). This is the single place the 1970 hardstop is enforced;
/// every duration field routes through it.
pub(crate) fn validate_duration_field(
    field: &'static str,
    raw: &str,
) -> Result<std::time::Duration> {
    let duration = parse_duration_string(raw).ok_or(StackError::InvalidDurationField { field })?;
    if duration > max_duration_since_epoch() {
        return Err(StackError::InvalidParam {
            field,
            reason: format!(
                "`{raw}` exceeds the maximum interval (the time since 1970-01-01); a longer span would place a `now - {raw}` cutoff before the Unix epoch"
            ),
        });
    }
    Ok(duration)
}

pub(crate) fn normalize_day_or_week_duration(field: &'static str, raw: &str) -> Result<String> {
    let value = raw.trim();
    let unit_index = value
        .find(|character: char| !character.is_ascii_digit())
        .ok_or(StackError::InvalidParam {
            field,
            reason: format!("expected a count and a day/week unit (e.g. 1d, 3w), got `{value}`"),
        })?;
    let (digits, unit) = value.split_at(unit_index);
    if !matches!(unit, "d" | "w") {
        return Err(StackError::InvalidParam {
            field,
            reason: format!(
                "use a day (d) or week (w) unit; the minimum update granularity is a day, got `{value}`"
            ),
        });
    }
    let count: u64 = digits.parse().map_err(|_| StackError::InvalidParam {
        field,
        reason: format!("`{value}` is not a valid count + unit"),
    })?;
    if count == 0 {
        return Err(StackError::InvalidParam {
            field,
            reason: "frequency must be at least 1 day".to_owned(),
        });
    }
    // Apply the representability + 1970 hardstop shared by every duration field
    // (this subsumes the raw-`Duration` overflow guard). Validating here keeps
    // config load in agreement with the runtime, which re-parses the same string
    // with `parse_duration_string` when it schedules the next update.
    validate_duration_field(field, value)?;
    Ok(value.to_owned())
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
