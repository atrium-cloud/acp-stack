//! Runtime security self-check.
//!
//! Examines effective config plus a recent auth-failure count and returns a
//! list of `SecurityFinding`s. The same helper is called by both
//! `GET /v1/security/check` on the admin-tier HTTP API and `acpctl security
//! check` on the local UDS surface, so the policy stays in one place.

use std::net::SocketAddr;

use serde::Serialize;

use crate::config::SecurityHttpConfig;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SecurityFinding {
    pub code: String,
    pub severity: String,
    pub message: String,
}

impl SecurityFinding {
    pub fn warning(code: &str, message: &str) -> Self {
        Self {
            code: code.to_owned(),
            severity: "warning".to_owned(),
            message: message.to_owned(),
        }
    }

    pub fn critical(code: &str, message: &str) -> Self {
        Self {
            code: code.to_owned(),
            severity: "critical".to_owned(),
            message: message.to_owned(),
        }
    }
}

/// Inputs to the security self-check. Decoupled from `AppState` so the helper
/// can live outside `api.rs` without pulling that module into `security.rs`.
pub struct SecurityCheckInputs<'a> {
    pub effective_bind: &'a str,
    pub http: &'a SecurityHttpConfig,
    pub session_key_empty: bool,
    pub admin_key_empty: bool,
    pub recent_auth_failures: i64,
    /// Count of `sink_outbox` rows that are stuck in pending+failing state.
    /// Non-zero means the Supabase sink has unfinished work and has retried
    /// at least once; surfaced so operators know external logging is lagging.
    pub sink_open_failures: i64,
    /// Most recent error captured by the sink worker, taken verbatim from
    /// `sink_failures_summary.last_error`. Truncated by the worker before
    /// it lands in the table, so passing through here is safe.
    pub sink_last_error: Option<&'a str>,
}

/// Compute the list of security findings for the running daemon.
pub fn check(inputs: SecurityCheckInputs<'_>) -> Vec<SecurityFinding> {
    let mut findings = Vec::new();
    let bind_is_public = inputs
        .effective_bind
        .parse::<SocketAddr>()
        .map(|addr| addr.ip().is_unspecified())
        .unwrap_or(false);

    if bind_is_public {
        findings.push(SecurityFinding::warning(
            "api.public_bind",
            "API bind address listens on all interfaces",
        ));
    }

    if bind_is_public
        && inputs
            .http
            .allowed_origins
            .iter()
            .any(|origin| origin == "*")
    {
        findings.push(SecurityFinding::critical(
            "http.wildcard_origin_public_bind",
            "wildcard CORS origin is configured on a public bind address",
        ));
    }

    if inputs.http.trust_proxy_headers && inputs.http.trusted_proxies.is_empty() {
        findings.push(SecurityFinding::critical(
            "http.trust_proxy_without_trusted_proxies",
            "proxy headers are trusted but no trusted proxy allowlist is configured",
        ));
    }

    if inputs.session_key_empty {
        findings.push(SecurityFinding::critical(
            "auth.session_key_empty",
            "session API key is empty",
        ));
    }

    if inputs.admin_key_empty {
        findings.push(SecurityFinding::critical(
            "auth.admin_key_empty",
            "admin API key is empty",
        ));
    }

    let threshold = inputs.http.auth_failures_per_minute;
    if threshold > 0 && inputs.recent_auth_failures >= i64::try_from(threshold).unwrap_or(i64::MAX)
    {
        findings.push(SecurityFinding::warning(
            "auth.failure_threshold",
            "auth failure count meets or exceeds the configured per-minute threshold",
        ));
    }

    if inputs.sink_open_failures > 0 {
        let suffix = inputs
            .sink_last_error
            .filter(|s| !s.is_empty())
            .map(|err| format!(" (last error: {err})"))
            .unwrap_or_default();
        findings.push(SecurityFinding::warning(
            "logging.supabase.delivery_failing",
            &format!(
                "Supabase sink has {} pending rows with retry failures{suffix}",
                inputs.sink_open_failures
            ),
        ));
    }

    findings
}

#[cfg(test)]
mod tests {
    use super::*;

    fn baseline_http() -> SecurityHttpConfig {
        SecurityHttpConfig {
            max_request_bytes: 1_048_576,
            rate_limit_per_minute: 60,
            burst: 10,
            auth_failures_per_minute: 10,
            auth_block_duration: "5m".to_owned(),
            allowed_origins: Vec::new(),
            trust_proxy_headers: false,
            trusted_proxies: Vec::new(),
        }
    }

    fn baseline_inputs<'a>(http: &'a SecurityHttpConfig) -> SecurityCheckInputs<'a> {
        SecurityCheckInputs {
            effective_bind: "127.0.0.1:8080",
            http,
            session_key_empty: false,
            admin_key_empty: false,
            recent_auth_failures: 0,
            sink_open_failures: 0,
            sink_last_error: None,
        }
    }

    #[test]
    fn loopback_bind_with_keys_returns_no_findings() {
        let http = baseline_http();
        let findings = check(baseline_inputs(&http));
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn sink_open_failures_surface_warning_with_last_error() {
        let http = baseline_http();
        let mut inputs = baseline_inputs(&http);
        inputs.sink_open_failures = 4;
        inputs.sink_last_error = Some("HTTP 503: gateway down");
        let findings = check(inputs);
        let finding = findings
            .iter()
            .find(|f| f.code == "logging.supabase.delivery_failing")
            .expect("sink finding present");
        assert!(finding.message.contains("HTTP 503"));
        assert!(finding.message.contains("4 pending"));
    }

    #[test]
    fn sink_zero_failures_does_not_warn() {
        let http = baseline_http();
        let findings = check(baseline_inputs(&http));
        assert!(
            !findings
                .iter()
                .any(|f| f.code == "logging.supabase.delivery_failing"),
            "{findings:?}"
        );
    }

    #[test]
    fn unspecified_bind_flags_public_warning() {
        let http = baseline_http();
        let mut inputs = baseline_inputs(&http);
        inputs.effective_bind = "0.0.0.0:8080";
        let findings = check(inputs);
        assert!(findings.iter().any(|f| f.code == "api.public_bind"));
    }

    #[test]
    fn wildcard_origin_on_public_bind_is_critical() {
        let mut http = baseline_http();
        http.allowed_origins = vec!["*".to_owned()];
        let mut inputs = baseline_inputs(&http);
        inputs.effective_bind = "0.0.0.0:8080";
        let findings = check(inputs);
        assert!(
            findings
                .iter()
                .any(|f| f.code == "http.wildcard_origin_public_bind")
        );
    }

    #[test]
    fn proxy_trust_without_allowlist_is_critical() {
        let mut http = baseline_http();
        http.trust_proxy_headers = true;
        let findings = check(baseline_inputs(&http));
        assert!(
            findings
                .iter()
                .any(|f| f.code == "http.trust_proxy_without_trusted_proxies")
        );
    }

    #[test]
    fn proxy_trust_with_allowlist_does_not_flag() {
        let mut http = baseline_http();
        http.trust_proxy_headers = true;
        http.trusted_proxies = vec!["10.0.0.1".to_owned()];
        let findings = check(baseline_inputs(&http));
        assert!(
            !findings
                .iter()
                .any(|f| f.code == "http.trust_proxy_without_trusted_proxies"),
            "{findings:?}"
        );
    }

    #[test]
    fn empty_session_key_is_critical() {
        let http = baseline_http();
        let mut inputs = baseline_inputs(&http);
        inputs.session_key_empty = true;
        let findings = check(inputs);
        assert!(findings.iter().any(|f| f.code == "auth.session_key_empty"));
    }

    #[test]
    fn auth_failure_threshold_warns_when_met() {
        let http = baseline_http();
        let mut inputs = baseline_inputs(&http);
        inputs.recent_auth_failures = 10;
        let findings = check(inputs);
        assert!(findings.iter().any(|f| f.code == "auth.failure_threshold"));
    }
}
