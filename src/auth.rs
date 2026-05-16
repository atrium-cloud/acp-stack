//! API key generation, constant-time comparison, and structured auth-failure
//! logging.
//!
//! Keys are 32 random bytes from the system CSPRNG, base64url-encoded without
//! padding, prefixed with `acps_`. The prefix makes leaked keys identifiable
//! in logs and grep without leaking the secret bytes themselves; the format
//! matches widely-used patterns (GitHub, Stripe).
//!
//! `record_auth_failure` is the single entry point for writing rows into the
//! `auth_failures` table. The HTTP layer (next batch) calls this whenever it
//! rejects a key; never store the attempted key value itself, only the kind
//! that was expected and the reason it failed (per
//! `docs/specs/security.md:23`).

use base64::Engine;
use rand::RngExt;
use serde::Serialize;
use subtle::ConstantTimeEq;

use crate::error::Result;
use crate::state::{AuthFailure, StateStore};

pub const API_KEY_PREFIX: &str = "acps_";
pub const API_KEY_ENTROPY_BYTES: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum KeyKind {
    Session,
    Admin,
    /// Stamped by the Unix-domain-socket listener that serves `acpctl`. It is
    /// not derivable from any bearer; route handlers reuse the same code path
    /// as session/admin requests but `enforce_tier` will never accept Local on
    /// the TCP router, so a Local tag leaking into the public API is a 401.
    Local,
    Unknown,
}

impl KeyKind {
    pub fn as_wire_str(self) -> &'static str {
        match self {
            KeyKind::Session => "session",
            KeyKind::Admin => "admin",
            KeyKind::Local => "local",
            KeyKind::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthFailureReason {
    Missing,
    Invalid,
    WrongKind,
    MalformedHeader,
}

impl AuthFailureReason {
    pub fn as_wire_str(self) -> &'static str {
        match self {
            AuthFailureReason::Missing => "missing",
            AuthFailureReason::Invalid => "invalid",
            AuthFailureReason::WrongKind => "wrong_kind",
            AuthFailureReason::MalformedHeader => "malformed_header",
        }
    }
}

/// Generate a fresh API key. The bytes come from `rand::rng()`, which is the
/// thread-local CSPRNG and is reseeded periodically from the system entropy
/// source.
pub fn generate_api_key() -> String {
    let mut bytes = [0u8; API_KEY_ENTROPY_BYTES];
    rand::rng().fill(&mut bytes);
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    format!("{API_KEY_PREFIX}{encoded}")
}

/// Constant-time equality on byte slices of equal length. Returns false when
/// the lengths differ; this length check is NOT itself constant-time, but the
/// API key length is fixed and public, so a length mismatch reveals nothing a
/// caller cannot observe by inspecting the key format. For two same-length
/// slices, the byte-by-byte comparison is constant-time via `subtle`.
pub fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.ct_eq(right).into()
}

/// Persist an auth-failure record. The payload is a small JSON object so
/// future fields (rate-limit context, header parse details, etc.) can be
/// added without a migration. Never write the attempted key value here.
pub fn record_auth_failure(
    state: &StateStore,
    kind: KeyKind,
    reason: AuthFailureReason,
    client_ip: Option<&str>,
    route: Option<&str>,
) -> Result<AuthFailure> {
    let payload = serde_json::json!({
        "key_kind": kind.as_wire_str(),
        "reason": reason.as_wire_str(),
        "client_ip": client_ip,
        "route": route,
    });
    state.append_auth_failure(
        kind.as_wire_str(),
        reason.as_wire_str(),
        client_ip,
        route,
        &payload.to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_keys_have_acps_prefix_and_43_char_body() {
        let key = generate_api_key();
        assert!(key.starts_with(API_KEY_PREFIX));
        let body = &key[API_KEY_PREFIX.len()..];
        // 32 bytes -> ceil(32 * 4 / 3) = 43 base64url chars, no padding.
        assert_eq!(body.len(), 43);
        for c in body.chars() {
            assert!(
                c.is_ascii_alphanumeric() || c == '-' || c == '_',
                "non-base64url char: {c}"
            );
        }
    }

    #[test]
    fn generated_keys_differ() {
        let a = generate_api_key();
        let b = generate_api_key();
        assert_ne!(a, b);
    }

    #[test]
    fn constant_time_eq_matches_equal_slices() {
        assert!(constant_time_eq(b"hello", b"hello"));
    }

    #[test]
    fn constant_time_eq_rejects_different_slices() {
        assert!(!constant_time_eq(b"hello", b"world"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"", b"a"));
    }

    #[test]
    fn key_kind_wire_strings_are_stable() {
        assert_eq!(KeyKind::Session.as_wire_str(), "session");
        assert_eq!(KeyKind::Admin.as_wire_str(), "admin");
        assert_eq!(KeyKind::Local.as_wire_str(), "local");
        assert_eq!(KeyKind::Unknown.as_wire_str(), "unknown");
    }

    #[test]
    fn auth_failure_reason_wire_strings_are_stable() {
        assert_eq!(AuthFailureReason::Missing.as_wire_str(), "missing");
        assert_eq!(AuthFailureReason::Invalid.as_wire_str(), "invalid");
        assert_eq!(AuthFailureReason::WrongKind.as_wire_str(), "wrong_kind");
        assert_eq!(
            AuthFailureReason::MalformedHeader.as_wire_str(),
            "malformed_header"
        );
    }
}
