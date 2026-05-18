//! Runtime security self-check.
//!
//! Examines effective config plus a recent auth-failure count and returns a
//! list of `SecurityFinding`s. The same helper is called by both
//! `GET /v1/security/check` on the admin-tier HTTP API and `acpctl security
//! check` on the local UDS surface, so the policy stays in one place.

use std::net::SocketAddr;

use serde::Serialize;

use crate::config::SecurityHttpConfig;
use crate::ownership::PathPosture;

/// Minimum acceptable length for the session and admin API keys. Set to 32
/// because the keys generated at `acps init` time are 32-byte random values
/// rendered as 43-char base64. Shorter keys typically come from operator
/// edits and are flagged with `auth.*_key_weak`.
pub const MIN_API_KEY_LEN: usize = 32;

/// Case-insensitive placeholder values rejected by the `auth.*_key_weak`
/// check. Compared against the trimmed key value.
const WEAK_PLACEHOLDERS: &[&str] = &[
    "changeme",
    "default",
    "placeholder",
    "replace_me",
    "replaceme",
    "test",
    "example",
    "secret",
];

fn key_is_weak(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.len() < MIN_API_KEY_LEN {
        return true;
    }
    let lower = trimmed.to_ascii_lowercase();
    WEAK_PLACEHOLDERS.iter().any(|p| lower == *p)
}

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
    /// Plaintext session key, used to surface `auth.session_key_weak` when
    /// the operator has substituted a short or placeholder value. Never
    /// echoed in finding messages.
    pub session_key_value: Option<&'a str>,
    /// Plaintext admin key, same handling as `session_key_value`.
    pub admin_key_value: Option<&'a str>,
    /// Inspection result for each runtime-managed path; produced by the
    /// caller via `ownership::inspect`. The check emits one
    /// `runtime.path_mode_loose` / `runtime.path_ownership` finding per
    /// offending path.
    pub path_postures: &'a [PathPosture],
    /// `geteuid()` of the daemon process. Used to verify path ownership
    /// matches the running user.
    pub process_euid: u32,
    /// Uid resolved from `workspace.runtime_user` via `getpwnam_r`. `None`
    /// when the user does not exist in the password database (e.g. the
    /// installer has not run yet). The check skips `runtime.user_mismatch`
    /// in that case.
    pub runtime_user_uid: Option<u32>,
    /// Configured `workspace.runtime_user` string, included verbatim in the
    /// `runtime.user_mismatch` message so operators can correlate with their
    /// systemd `User=` directive.
    pub runtime_user_name: &'a str,
    /// Result of `ownership::workspace_writable(workspace.root)` — whether
    /// the daemon can create files inside the configured workspace root.
    pub workspace_writable: bool,
    /// Configured workspace root path, surfaced in
    /// `runtime.workspace_not_writable` for operator clarity.
    pub workspace_root: &'a str,
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

    // The weakness checks only fire when the empty check didn't already
    // catch the key — an empty key is already a `critical` finding, and
    // reporting both for the same key would be noise.
    if !inputs.session_key_empty {
        if let Some(value) = inputs.session_key_value {
            if key_is_weak(value) {
                findings.push(SecurityFinding::warning(
                    "auth.session_key_weak",
                    "session API key is too short or matches a known weak placeholder; \
                     rotate it via `acps auth regenerate-session-key`",
                ));
            }
        }
    }
    if !inputs.admin_key_empty {
        if let Some(value) = inputs.admin_key_value {
            if key_is_weak(value) {
                findings.push(SecurityFinding::warning(
                    "auth.admin_key_weak",
                    "admin API key is too short or matches a known weak placeholder; \
                     rotate it via `acps reset --yes` and re-init",
                ));
            }
        }
    }

    for posture in inputs.path_postures {
        if posture.uid != inputs.process_euid {
            findings.push(SecurityFinding::critical(
                "runtime.path_ownership",
                &format!(
                    "{label} at {path} is owned by uid {actual}, expected uid {expected}",
                    label = posture.kind.label(),
                    path = posture.path.display(),
                    actual = posture.uid,
                    expected = inputs.process_euid,
                ),
            ));
        }
        if let Some(expected_mode) = posture.kind.expected_mode() {
            if posture.mode != expected_mode {
                findings.push(SecurityFinding::critical(
                    "runtime.path_mode_loose",
                    &format!(
                        "{label} at {path} has mode 0o{actual:o}, expected 0o{expected:o}",
                        label = posture.kind.label(),
                        path = posture.path.display(),
                        actual = posture.mode,
                        expected = expected_mode,
                    ),
                ));
            }
        }
    }

    if let Some(uid) = inputs.runtime_user_uid {
        if uid != inputs.process_euid {
            findings.push(SecurityFinding::warning(
                "runtime.user_mismatch",
                &format!(
                    "daemon euid {euid} does not match configured runtime_user '{name}' (uid {uid}); \
                     check the systemd User= directive or container USER",
                    euid = inputs.process_euid,
                    name = inputs.runtime_user_name,
                ),
            ));
        }
    }

    if !inputs.workspace_writable {
        findings.push(SecurityFinding::critical(
            "runtime.workspace_not_writable",
            &format!(
                "workspace root {root} is not writable by the running daemon (uid {euid})",
                root = inputs.workspace_root,
                euid = inputs.process_euid,
            ),
        ));
    }

    findings
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ownership::PathKind;

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
            session_key_value: None,
            admin_key_value: None,
            path_postures: &[],
            process_euid: 1000,
            runtime_user_uid: Some(1000),
            runtime_user_name: "acp",
            workspace_writable: true,
            workspace_root: "/workspace",
        }
    }

    fn long_key() -> String {
        // 64-char value, well above MIN_API_KEY_LEN.
        "a".repeat(64)
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

    #[test]
    fn weak_session_key_short_value_is_flagged() {
        let http = baseline_http();
        let mut inputs = baseline_inputs(&http);
        let short = "abc";
        inputs.session_key_value = Some(short);
        let findings = check(inputs);
        let finding = findings
            .iter()
            .find(|f| f.code == "auth.session_key_weak")
            .expect("weak session key finding");
        // Never echo the key value.
        assert!(!finding.message.contains(short));
    }

    #[test]
    fn weak_admin_key_placeholder_is_flagged() {
        let http = baseline_http();
        let mut inputs = baseline_inputs(&http);
        inputs.admin_key_value = Some("CHANGEME");
        let findings = check(inputs);
        assert!(findings.iter().any(|f| f.code == "auth.admin_key_weak"));
    }

    #[test]
    fn weak_check_skipped_when_empty_already_flagged() {
        // The empty check is already a critical finding; the weak check must
        // not pile on with a duplicate warning for the same key.
        let http = baseline_http();
        let mut inputs = baseline_inputs(&http);
        inputs.session_key_empty = true;
        inputs.session_key_value = Some(""); // also "weak", but empty wins
        let findings = check(inputs);
        assert!(findings.iter().any(|f| f.code == "auth.session_key_empty"));
        assert!(
            !findings.iter().any(|f| f.code == "auth.session_key_weak"),
            "{findings:?}"
        );
    }

    #[test]
    fn long_random_key_passes() {
        let http = baseline_http();
        let mut inputs = baseline_inputs(&http);
        let key = long_key();
        inputs.session_key_value = Some(&key);
        inputs.admin_key_value = Some(&key);
        let findings = check(inputs);
        assert!(
            !findings
                .iter()
                .any(|f| f.code.starts_with("auth.") && f.code.ends_with("_weak"))
        );
    }

    #[test]
    fn path_mode_loose_for_directory_is_critical() {
        let http = baseline_http();
        let posture = PathPosture {
            path: std::path::PathBuf::from("/home/acp/.config/acp-stack"),
            kind: PathKind::ConfigDir,
            uid: 1000,
            mode: 0o755,
        };
        let postures = [posture];
        let mut inputs = baseline_inputs(&http);
        inputs.path_postures = &postures;
        let findings = check(inputs);
        let finding = findings
            .iter()
            .find(|f| f.code == "runtime.path_mode_loose")
            .expect("mode-loose finding");
        assert_eq!(finding.severity, "critical");
        assert!(finding.message.contains("0o755"));
        assert!(finding.message.contains("0o700"));
    }

    #[test]
    fn path_mode_correct_does_not_flag() {
        let http = baseline_http();
        let posture = PathPosture {
            path: std::path::PathBuf::from("/home/acp/.config/acp-stack/age.key"),
            kind: PathKind::AgeKey,
            uid: 1000,
            mode: 0o600,
        };
        let postures = [posture];
        let mut inputs = baseline_inputs(&http);
        inputs.path_postures = &postures;
        let findings = check(inputs);
        assert!(
            !findings.iter().any(|f| f.code == "runtime.path_mode_loose"),
            "{findings:?}"
        );
    }

    #[test]
    fn path_ownership_mismatch_is_critical() {
        let http = baseline_http();
        let posture = PathPosture {
            path: std::path::PathBuf::from("/workspace"),
            kind: PathKind::WorkspaceRoot,
            uid: 0,
            mode: 0o755,
        };
        let postures = [posture];
        let mut inputs = baseline_inputs(&http);
        inputs.path_postures = &postures;
        let findings = check(inputs);
        let finding = findings
            .iter()
            .find(|f| f.code == "runtime.path_ownership")
            .expect("ownership finding");
        assert_eq!(finding.severity, "critical");
        assert!(finding.message.contains("uid 0"));
        assert!(finding.message.contains("uid 1000"));
    }

    #[test]
    fn workspace_root_skips_mode_check() {
        // WorkspaceRoot.expected_mode() is None — the workspace's actual mode
        // must not produce a `runtime.path_mode_loose` finding even when the
        // mode is wide-open.
        let http = baseline_http();
        let posture = PathPosture {
            path: std::path::PathBuf::from("/workspace"),
            kind: PathKind::WorkspaceRoot,
            uid: 1000,
            mode: 0o777,
        };
        let postures = [posture];
        let mut inputs = baseline_inputs(&http);
        inputs.path_postures = &postures;
        let findings = check(inputs);
        assert!(
            !findings.iter().any(|f| f.code == "runtime.path_mode_loose"),
            "{findings:?}"
        );
    }

    #[test]
    fn runtime_user_mismatch_warns_when_uid_differs() {
        let http = baseline_http();
        let mut inputs = baseline_inputs(&http);
        inputs.runtime_user_uid = Some(2000);
        inputs.process_euid = 1000;
        let findings = check(inputs);
        let finding = findings
            .iter()
            .find(|f| f.code == "runtime.user_mismatch")
            .expect("user mismatch finding");
        assert_eq!(finding.severity, "warning");
        assert!(finding.message.contains("uid 2000"));
        assert!(finding.message.contains("euid 1000"));
        assert!(finding.message.contains("'acp'"));
    }

    #[test]
    fn runtime_user_mismatch_skipped_when_user_not_resolved() {
        let http = baseline_http();
        let mut inputs = baseline_inputs(&http);
        inputs.runtime_user_uid = None;
        let findings = check(inputs);
        assert!(
            !findings.iter().any(|f| f.code == "runtime.user_mismatch"),
            "{findings:?}"
        );
    }

    #[test]
    fn workspace_not_writable_is_critical() {
        let http = baseline_http();
        let mut inputs = baseline_inputs(&http);
        inputs.workspace_writable = false;
        let findings = check(inputs);
        let finding = findings
            .iter()
            .find(|f| f.code == "runtime.workspace_not_writable")
            .expect("workspace-not-writable finding");
        assert_eq!(finding.severity, "critical");
        assert!(finding.message.contains("/workspace"));
    }
}
