//! Finding types and the small helpers shared across rule modules.
//!
//! `SecurityFinding` is the wire/CLI shape; `PathInspectionIssue` carries a
//! per-path inspection failure from `ownership::inspect`. The `shell_quote`
//! helper is used by multiple rule modules to render remediation strings.

use std::net::SocketAddr;
use std::path::PathBuf;

use serde::Serialize;

/// Returns true if `bind` parses to a socket address with an unspecified IP
/// (e.g. `0.0.0.0` / `[::]`). Used by the bind/cors/cloudflare rules to gate
/// public-bind-only checks; defaults to false if `bind` does not parse.
pub(super) fn bind_is_public(bind: &str) -> bool {
    bind.parse::<SocketAddr>()
        .map(|addr| addr.ip().is_unspecified())
        .unwrap_or(false)
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
    /// Optional structured payload attached to the finding. `message` is the
    /// human-readable summary; `details` carries the same facts in machine-
    /// readable form so the history view, future dashboards, or downstream
    /// observability can aggregate without parsing the message string.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
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
            details: None,
            remediation: None,
        }
    }

    pub fn critical(code: &str, message: &str) -> Self {
        Self {
            code: code.to_owned(),
            severity: "critical".to_owned(),
            message: message.to_owned(),
            details: None,
            remediation: None,
        }
    }

    pub fn with_remediation(mut self, remediation: impl Into<String>) -> Self {
        self.remediation = Some(remediation.into());
        self
    }

    pub fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = Some(details);
        self
    }
}
