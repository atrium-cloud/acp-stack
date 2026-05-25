//! Finding types and the small helpers shared across rule modules.
//!
//! `SecurityFinding` is the wire/CLI shape; `PathInspectionIssue` carries a
//! per-path inspection failure from `ownership::inspect`. The `shell_quote`
//! and `key_is_weak` helpers are used by multiple rule modules to render
//! remediation strings and detect weak operator-supplied keys.

use std::net::SocketAddr;
use std::path::PathBuf;

use serde::Serialize;

use crate::security::MIN_API_KEY_LEN;

/// Returns true if `bind` parses to a socket address with an unspecified IP
/// (e.g. `0.0.0.0` / `[::]`). Used by the bind/cors/cloudflare rules to gate
/// public-bind-only checks; defaults to false if `bind` does not parse.
pub(super) fn bind_is_public(bind: &str) -> bool {
    bind.parse::<SocketAddr>()
        .map(|addr| addr.ip().is_unspecified())
        .unwrap_or(false)
}

/// Case-insensitive placeholder values rejected by the `auth.*_key_weak`
/// check. Compared against the trimmed key value.
pub(super) const WEAK_PLACEHOLDERS: &[&str] = &[
    "changeme",
    "default",
    "placeholder",
    "replace_me",
    "replaceme",
    "test",
    "example",
    "secret",
];

pub(super) fn key_is_weak(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.len() < MIN_API_KEY_LEN {
        return true;
    }
    let lower = trimmed.to_ascii_lowercase();
    WEAK_PLACEHOLDERS.iter().any(|p| lower == *p)
}

/// POSIX shell single-quote escape. Wraps `s` in `'...'` and replaces each
/// embedded `'` with `'\''` so the produced token is safe to paste into a
/// shell, even when the path itself was attacker-controlled (e.g. a workspace
/// root from imported config). Used to render copy-pastable `chown`/`chmod`
/// commands inside `SecurityFinding::remediation`.
pub(super) fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SecurityFinding {
    pub code: String,
    pub severity: String,
    pub message: String,
    /// Operator-actionable remediation hint. `message` describes *what* is
    /// wrong; `remediation` describes *how to fix it*. Kept as a separate
    /// field so CLI/UI surfaces can render it distinct from the diagnostic
    /// text and so callers can lint findings for "must have a hint".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remediation: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PathInspectionIssue {
    pub path: PathBuf,
    pub kind: crate::ownership::PathKind,
    pub error: String,
}

impl SecurityFinding {
    pub fn warning(code: &str, message: &str) -> Self {
        Self {
            code: code.to_owned(),
            severity: "warning".to_owned(),
            message: message.to_owned(),
            remediation: None,
        }
    }

    pub fn critical(code: &str, message: &str) -> Self {
        Self {
            code: code.to_owned(),
            severity: "critical".to_owned(),
            message: message.to_owned(),
            remediation: None,
        }
    }

    pub fn with_remediation(mut self, remediation: impl Into<String>) -> Self {
        self.remediation = Some(remediation.into());
        self
    }
}
