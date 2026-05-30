//! Runtime security self-check.
//!
//! Examines effective config plus a recent auth-failure count and returns a
//! list of `SecurityFinding`s. The same helper is called by both
//! `GET /v1/security/check` on the admin-tier HTTP API and `acpctl security
//! check` on the local UDS surface, so the policy stays in one place.
//!
//! The individual checks live under `rules::*`; this file is the orchestrator
//! that wires inputs through the rules in the order the original linear
//! implementation emitted findings.

mod findings;
mod rules;

pub use self::findings::{PathInspectionIssue, SecurityFinding};

use crate::config::CloudflareEdgeConfig;
use crate::config::SecurityHttpConfig;
use crate::ownership::PathPosture;
use crate::runtime::dependencies::deps::DepsReport;

/// Minimum acceptable length for the session and admin API keys. Set to 32
/// because the keys generated at `acps init` time are 32-byte random values
/// rendered as 43-char base64. Shorter keys typically come from operator
/// edits and are flagged with `auth.*_key_weak`.
pub const MIN_API_KEY_LEN: usize = 32;

/// Maximum dependency rows copied into the `deps.required_unavailable` details
/// payload. Operators still get the complete report from `acps deps check`;
/// the self-check detail stays bounded so one oversized config cannot bloat
/// security history rows indefinitely.
pub const MAX_DEPENDENCY_FINDING_DETAILS: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencySecurityFailure {
    pub name: String,
    pub kind: String,
    pub feature: Option<String>,
    pub reason: Option<String>,
}

pub fn dependency_security_failures(report: &DepsReport) -> Vec<DependencySecurityFailure> {
    report
        .dependencies
        .iter()
        .filter(|dependency| dependency.required && !dependency.available)
        .map(|dependency| DependencySecurityFailure {
            name: dependency.name.clone(),
            kind: format!("{:?}", dependency.kind).to_ascii_lowercase(),
            feature: dependency.feature.clone(),
            reason: dependency.reason.clone(),
        })
        .collect()
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
    /// Paths that could not be inspected at all. Missing or unreadable
    /// runtime-managed files are security findings too; otherwise a deleted
    /// age key or state DB can make the check look cleaner than reality.
    pub path_issues: &'a [PathInspectionIssue],
    /// `geteuid()` of the daemon process. Used to verify path ownership
    /// matches the running user.
    pub process_euid: u32,
    /// Uid resolved from `workspace.runtime_user` via `getpwnam_r`. `None`
    /// when the user does not exist in the password database.
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
    /// True when the daemon is running inside Railway's managed runtime.
    /// Railway terminates public HTTP at its edge and may run the container as
    /// root when a persistent volume is attached, even though the image's
    /// normal Docker posture remains `USER acp`.
    pub railway_platform: bool,
    pub cloudflare: Option<&'a CloudflareEdgeConfig>,
    pub cloudflared_available: bool,
    pub recent_direct_cloudflare_mode_requests: i64,
    pub recent_missing_cloudflare_header_requests: i64,
    pub dependency_failures: &'a [DependencySecurityFailure],
}

/// Compute the list of security findings for the running daemon.
pub fn check(inputs: SecurityCheckInputs<'_>) -> Vec<SecurityFinding> {
    let mut findings = Vec::new();
    rules::check_bind(&inputs, &mut findings);
    rules::check_cors(&inputs, &mut findings);
    rules::check_proxy(&inputs, &mut findings);
    rules::check_cloudflare(&inputs, &mut findings);
    rules::check_keys(&inputs, &mut findings);
    rules::check_paths(&inputs, &mut findings);
    rules::check_runtime_user(&inputs, &mut findings);
    rules::check_deps(&inputs, &mut findings);
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
            path_issues: &[],
            process_euid: 1000,
            runtime_user_uid: Some(1000),
            runtime_user_name: "acp",
            workspace_writable: true,
            workspace_root: "/workspace",
            railway_platform: false,
            cloudflare: None,
            cloudflared_available: true,
            recent_direct_cloudflare_mode_requests: 0,
            recent_missing_cloudflare_header_requests: 0,
            dependency_failures: &[],
        }
    }

    fn long_key() -> String {
        // 64-char value, well above MIN_API_KEY_LEN.
        "a".repeat(64)
    }

    fn cloudflare_edge() -> CloudflareEdgeConfig {
        CloudflareEdgeConfig {
            enabled: true,
            mode: "generated".to_owned(),
            exposure: "tunnel".to_owned(),
            hostname: "agent.example.com".to_owned(),
            api_token_ref: None,
            account_id_ref: None,
            tunnel_name: Some("acp-stack".to_owned()),
            tunnel_id: None,
            cloudflared_deployment: "host".to_owned(),
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
        assert!(
            finding
                .remediation
                .as_deref()
                .is_some_and(|r| r.contains("[logging.supabase]"))
        );
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
        let finding = findings
            .iter()
            .find(|f| f.code == "api.public_bind")
            .expect("public_bind finding");
        assert!(
            finding
                .remediation
                .as_deref()
                .is_some_and(|r| r.contains("loopback") || r.contains("reverse proxy"))
        );
    }

    #[test]
    fn railway_platform_suppresses_public_bind_warning() {
        let http = baseline_http();
        let mut inputs = baseline_inputs(&http);
        inputs.effective_bind = "0.0.0.0:8080";
        inputs.railway_platform = true;
        let findings = check(inputs);
        assert!(
            !findings.iter().any(|f| f.code == "api.public_bind"),
            "{findings:?}"
        );
    }

    #[test]
    fn wildcard_origin_on_public_bind_is_critical() {
        let mut http = baseline_http();
        http.allowed_origins = vec!["*".to_owned()];
        let mut inputs = baseline_inputs(&http);
        inputs.effective_bind = "0.0.0.0:8080";
        let findings = check(inputs);
        let finding = findings
            .iter()
            .find(|f| f.code == "http.wildcard_origin_public_bind")
            .expect("wildcard_origin finding");
        assert!(
            finding
                .remediation
                .as_deref()
                .is_some_and(|r| r.contains("allowed_origins"))
        );
    }

    #[test]
    fn proxy_trust_without_allowlist_is_critical() {
        let mut http = baseline_http();
        http.trust_proxy_headers = true;
        let findings = check(baseline_inputs(&http));
        let finding = findings
            .iter()
            .find(|f| f.code == "http.trust_proxy_without_trusted_proxies")
            .expect("trust_proxy finding");
        assert!(
            finding
                .remediation
                .as_deref()
                .is_some_and(|r| r.contains("trusted_proxies"))
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
    fn cloudflare_tunnel_posture_checks_static_and_recent_traffic() {
        let mut http = baseline_http();
        http.allowed_origins = vec!["*".to_owned()];
        let edge = cloudflare_edge();
        let mut inputs = baseline_inputs(&http);
        inputs.effective_bind = "0.0.0.0:7700";
        inputs.cloudflare = Some(&edge);
        inputs.cloudflared_available = false;
        inputs.recent_direct_cloudflare_mode_requests = 1;
        inputs.recent_missing_cloudflare_header_requests = 1;
        let findings = check(inputs);
        for code in [
            "edge.cloudflare.public_bind_tunnel",
            "edge.cloudflare.unsafe_origins",
            "edge.cloudflare.missing_local_trusted_proxies",
            "edge.cloudflare.cloudflared_missing",
            "edge.cloudflare.headers_missing",
            "edge.cloudflare.direct_public_requests",
        ] {
            assert!(
                findings.iter().any(|finding| finding.code == code),
                "{code}: {findings:?}"
            );
        }
    }

    #[test]
    fn empty_session_key_is_critical() {
        let http = baseline_http();
        let mut inputs = baseline_inputs(&http);
        inputs.session_key_empty = true;
        let findings = check(inputs);
        let finding = findings
            .iter()
            .find(|f| f.code == "auth.session_key_empty")
            .expect("session_key_empty finding");
        assert!(
            finding
                .remediation
                .as_deref()
                .is_some_and(|r| r.contains("regenerate-session-key"))
        );
    }

    #[test]
    fn empty_admin_key_is_critical_with_reset_hint() {
        let http = baseline_http();
        let mut inputs = baseline_inputs(&http);
        inputs.admin_key_empty = true;
        let findings = check(inputs);
        let finding = findings
            .iter()
            .find(|f| f.code == "auth.admin_key_empty")
            .expect("admin_key_empty finding");
        assert!(
            finding
                .remediation
                .as_deref()
                .is_some_and(|r| r.contains("acps reset"))
        );
    }

    #[test]
    fn auth_failure_threshold_warns_when_met() {
        let http = baseline_http();
        let mut inputs = baseline_inputs(&http);
        inputs.recent_auth_failures = 10;
        let findings = check(inputs);
        let finding = findings
            .iter()
            .find(|f| f.code == "auth.failure_threshold")
            .expect("auth_failure_threshold finding");
        assert!(
            finding
                .remediation
                .as_deref()
                .is_some_and(|r| r.contains("/v1/logs/security"))
        );
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
        assert!(!finding.remediation.as_deref().unwrap_or("").contains(short));
        assert!(
            finding
                .remediation
                .as_deref()
                .is_some_and(|r| r.contains("regenerate-session-key"))
        );
    }

    #[test]
    fn weak_admin_key_placeholder_is_flagged() {
        let http = baseline_http();
        let mut inputs = baseline_inputs(&http);
        inputs.admin_key_value = Some("CHANGEME");
        let findings = check(inputs);
        let finding = findings
            .iter()
            .find(|f| f.code == "auth.admin_key_weak")
            .expect("admin_key_weak finding");
        assert!(
            finding
                .remediation
                .as_deref()
                .is_some_and(|r| r.contains("acps reset"))
        );
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
            is_symlink: false,
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
        let remediation = finding
            .remediation
            .as_deref()
            .expect("path_mode_loose remediation");
        assert!(
            remediation.contains("chmod 0700 -- '/home/acp/.config/acp-stack'"),
            "expected shell-quoted chmod hint with `--` option terminator, got: {remediation}"
        );
    }

    /// A symlinked managed path must NOT receive the `chmod` remediation —
    /// Linux `chmod` follows symlinks and would mutate the target, leaving
    /// the finding unresolved. The hint instead directs the operator to
    /// remove the link and recreate the path.
    #[test]
    fn path_mode_loose_symlink_directs_to_recreate_not_chmod() {
        let http = baseline_http();
        let posture = PathPosture {
            path: std::path::PathBuf::from("/home/acp/.config/acp-stack"),
            kind: PathKind::ConfigDir,
            uid: 1000,
            mode: 0o777,
            is_symlink: true,
        };
        let postures = [posture];
        let mut inputs = baseline_inputs(&http);
        inputs.path_postures = &postures;
        let findings = check(inputs);
        let finding = findings
            .iter()
            .find(|f| f.code == "runtime.path_mode_loose")
            .expect("mode-loose finding");
        let remediation = finding
            .remediation
            .as_deref()
            .expect("path_mode_loose remediation");
        // The remediation must not contain a `chmod ...` command to run — it
        // may still mention `chmod` to explain why the command isn't safe
        // here. So we look specifically for the "Run `chmod" command-form.
        assert!(
            !remediation.contains("Run `chmod"),
            "symlink remediation must not suggest a `chmod` command (would \
             follow link), got: {remediation}"
        );
        assert!(
            remediation.contains("symlink"),
            "remediation should call out the symlink, got: {remediation}"
        );
        assert!(
            remediation.contains("Remove") || remediation.contains("recreate"),
            "remediation should direct the operator to remove + recreate, got: {remediation}"
        );
    }

    #[test]
    fn path_mode_correct_does_not_flag() {
        let http = baseline_http();
        let posture = PathPosture {
            path: std::path::PathBuf::from("/home/acp/.config/acp-stack/age.key"),
            kind: PathKind::AgeKey,
            uid: 1000,
            mode: 0o600,
            is_symlink: false,
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
    fn path_mode_loose_for_config_file_is_critical() {
        let http = baseline_http();
        let posture = PathPosture {
            path: std::path::PathBuf::from("/home/acp/.config/acp-stack/acp-stack.toml"),
            kind: PathKind::ConfigFile,
            uid: 1000,
            mode: 0o644,
            is_symlink: false,
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
        assert!(finding.message.contains("config file"));
        assert!(finding.message.contains("0o644"));
        assert!(finding.message.contains("0o600"));
        let remediation = finding
            .remediation
            .as_deref()
            .expect("path_mode_loose remediation");
        assert!(
            remediation.contains("chmod 0600 -- '/home/acp/.config/acp-stack/acp-stack.toml'"),
            "expected config file chmod hint, got: {remediation}"
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
            is_symlink: false,
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
        let remediation = finding
            .remediation
            .as_deref()
            .expect("path_ownership remediation");
        // Hint must name the daemon's effective uid (the value the check
        // enforces), not the configured runtime_user_name. `chown {uid}`
        // (without a gid) leaves the group unchanged, matching what the
        // check actually validates.
        assert!(
            remediation.contains("chown -h 1000 -- '/workspace'"),
            "expected symlink-safe shell-quoted chown hint with `--` option \
             terminator, got: {remediation}"
        );
        // We must not nudge operators to relaunch under whatever owns the
        // path — for root-owned paths that violates the project's no-root
        // execution policy.
        assert!(
            !remediation.contains("relaunch"),
            "remediation must not suggest relaunching the daemon under the \
             current owner, got: {remediation}"
        );
    }

    #[test]
    fn path_ownership_mismatch_for_state_db_is_critical() {
        let http = baseline_http();
        let posture = PathPosture {
            path: std::path::PathBuf::from("/home/acp/.local/share/acp-stack/state.sqlite"),
            kind: PathKind::StateDb,
            uid: 0,
            mode: 0o600,
            is_symlink: false,
        };
        let postures = [posture];
        let mut inputs = baseline_inputs(&http);
        inputs.path_postures = &postures;
        let findings = check(inputs);
        let finding = findings
            .iter()
            .find(|f| f.code == "runtime.path_ownership")
            .expect("ownership finding");
        assert!(finding.message.contains("state database"));
        assert!(finding.message.contains("uid 0"));
        assert!(finding.message.contains("uid 1000"));
    }

    #[test]
    fn railway_root_volume_reports_workspace_and_default_user_mismatch() {
        let http = baseline_http();
        let posture = PathPosture {
            path: std::path::PathBuf::from("/workspace"),
            kind: PathKind::WorkspaceRoot,
            uid: 1000,
            mode: 0o755,
            is_symlink: false,
        };
        let postures = [posture];
        let mut inputs = baseline_inputs(&http);
        inputs.railway_platform = true;
        inputs.process_euid = 0;
        inputs.runtime_user_uid = Some(1000);
        inputs.path_postures = &postures;
        let findings = check(inputs);
        assert!(
            findings.iter().any(|f| f.code == "runtime.path_ownership"),
            "{findings:?}"
        );
        assert!(
            findings.iter().any(|f| f.code == "runtime.user_mismatch"),
            "{findings:?}"
        );
    }

    #[test]
    fn railway_root_volume_still_reports_managed_file_ownership() {
        let http = baseline_http();
        let posture = PathPosture {
            path: std::path::PathBuf::from("/home/acp/.config/acp-stack/acp-stack.toml"),
            kind: PathKind::ConfigFile,
            uid: 1000,
            mode: 0o600,
            is_symlink: false,
        };
        let postures = [posture];
        let mut inputs = baseline_inputs(&http);
        inputs.railway_platform = true;
        inputs.process_euid = 0;
        inputs.runtime_user_uid = Some(1000);
        inputs.path_postures = &postures;
        let findings = check(inputs);
        let finding = findings
            .iter()
            .find(|f| f.code == "runtime.path_ownership")
            .expect("managed file ownership finding");
        assert!(finding.message.contains("config file"));
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
            is_symlink: false,
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
        let remediation = finding
            .remediation
            .as_deref()
            .expect("user_mismatch remediation");
        assert!(
            remediation.contains("systemd") || remediation.contains("USER"),
            "expected systemd/container hint, got: {remediation}"
        );
        assert!(remediation.contains("'acp'"));
    }

    /// Paths in remediation strings must be POSIX shell-escaped so a
    /// workspace.root or HOME containing a single quote can't produce a
    /// pasted command injection. `'foo'` becomes `'foo'\''bar'` after escape.
    #[test]
    fn path_remediation_escapes_embedded_single_quotes() {
        let http = baseline_http();
        let posture = PathPosture {
            path: std::path::PathBuf::from("/work'; touch /tmp/pwn #"),
            kind: PathKind::WorkspaceRoot,
            uid: 0,
            mode: 0o755,
            is_symlink: false,
        };
        let postures = [posture];
        let mut inputs = baseline_inputs(&http);
        inputs.path_postures = &postures;
        let findings = check(inputs);
        let finding = findings
            .iter()
            .find(|f| f.code == "runtime.path_ownership")
            .expect("ownership finding");
        let remediation = finding
            .remediation
            .as_deref()
            .expect("path_ownership remediation");
        // POSIX single-quote escape: each embedded `'` becomes `'\''`. With
        // the escape applied AND a `--` option terminator before the path,
        // `/work'; touch …` renders as `-- '/work'\''; touch …'` — a quoted
        // token after option parsing ends, not an open-quote + injection.
        assert!(
            remediation.contains("-- '/work'\\''; touch /tmp/pwn #'"),
            "expected POSIX shell escape and `--` terminator for the path, got: {remediation}"
        );
    }

    /// When the configured runtime_user resolves to uid 0, the hint must not
    /// tell operators to relaunch the daemon as root — that breaks the
    /// no-root-execution policy. Direct them to update the config instead.
    #[test]
    fn runtime_user_mismatch_root_directs_to_update_config_not_relaunch() {
        let http = baseline_http();
        let mut inputs = baseline_inputs(&http);
        inputs.runtime_user_uid = Some(0);
        inputs.runtime_user_name = "root";
        inputs.process_euid = 1000;
        let findings = check(inputs);
        let finding = findings
            .iter()
            .find(|f| f.code == "runtime.user_mismatch")
            .expect("user mismatch finding");
        let remediation = finding
            .remediation
            .as_deref()
            .expect("user_mismatch remediation");
        assert!(
            !remediation.contains("Relaunch"),
            "remediation must not tell operator to relaunch as root, got: {remediation}"
        );
        assert!(
            remediation.contains("[workspace].runtime_user"),
            "remediation should direct operator to update workspace.runtime_user, got: {remediation}"
        );
    }

    #[test]
    fn runtime_user_unresolved_warns() {
        let http = baseline_http();
        let mut inputs = baseline_inputs(&http);
        inputs.runtime_user_uid = None;
        let findings = check(inputs);
        let finding = findings
            .iter()
            .find(|f| f.code == "runtime.user_unresolved")
            .expect("runtime user unresolved finding");
        assert_eq!(finding.severity, "warning");
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
        let remediation = finding
            .remediation
            .as_deref()
            .expect("workspace_not_writable remediation");
        assert!(remediation.contains("/workspace"));
        // Hint must name the daemon's effective uid (matches what the
        // `workspace_writable` probe actually tests), not the configured
        // runtime_user — see the comment on the finding emission.
        assert!(
            remediation.contains("uid 1000"),
            "expected effective-uid hint, got: {remediation}"
        );
    }

    #[test]
    fn uninspectable_path_is_critical_with_remediation() {
        let http = baseline_http();
        let issue = PathInspectionIssue {
            path: std::path::PathBuf::from("/home/acp/.config/acp-stack/acp-stack.toml"),
            kind: PathKind::ConfigFile,
            error: "No such file or directory".to_owned(),
        };
        let issues = [issue];
        let mut inputs = baseline_inputs(&http);
        inputs.path_issues = &issues;
        let findings = check(inputs);
        let finding = findings
            .iter()
            .find(|f| f.code == "runtime.path_uninspectable")
            .expect("uninspectable finding");
        assert_eq!(finding.severity, "critical");
        assert!(finding.message.contains("config file"));
        assert!(finding.message.contains("No such file or directory"));
        let remediation = finding
            .remediation
            .as_deref()
            .expect("path_uninspectable remediation");
        assert!(!remediation.trim().is_empty());
        assert!(remediation.contains("acps init"));
        assert!(remediation.contains("uid 1000"));
    }

    #[test]
    fn required_dependency_failures_emit_bounded_finding() {
        let http = baseline_http();
        let failures = [DependencySecurityFailure {
            name: "cloudflared".to_owned(),
            kind: "command".to_owned(),
            feature: Some("cloudflare-tunnel".to_owned()),
            reason: Some("`cloudflared` not found on PATH".to_owned()),
        }];
        let mut inputs = baseline_inputs(&http);
        inputs.dependency_failures = &failures;
        let findings = check(inputs);
        let finding = findings
            .iter()
            .find(|finding| finding.code == "deps.required_unavailable")
            .expect("dependency finding");
        assert_eq!(finding.severity, "warning");
        let remediation = finding.remediation.as_deref().expect("remediation");
        assert!(remediation.contains("acps deps check"));
        assert!(remediation.contains("acps deps apply"));
        let details = finding.details.as_ref().expect("details");
        assert_eq!(details["total"], 1);
        assert_eq!(details["truncated"], false);
        assert_eq!(details["dependencies"][0]["name"], "cloudflared");
        assert_eq!(details["dependencies"][0]["kind"], "command");
    }

    /// Every finding produced by `check()` must carry a non-empty remediation.
    /// Phase 4 ("Add remediation hints for incorrect ownership") is broader
    /// than ownership findings — operators benefit from an actionable hint on
    /// every finding, so we lint the entire set in one place.
    #[test]
    fn every_finding_has_a_non_empty_remediation() {
        // Construct an input that triggers every check at once. Some checks
        // are mutually exclusive (empty vs weak for the same key) so we run
        // two passes: one for "empty" findings, one for "weak" findings, and
        // assert remediation across both.
        let mut http = baseline_http();
        http.allowed_origins = vec!["*".to_owned()];
        http.trust_proxy_headers = true;
        http.trusted_proxies = Vec::new();

        let path_loose = PathPosture {
            path: std::path::PathBuf::from("/home/acp/.config/acp-stack"),
            kind: PathKind::ConfigDir,
            uid: 0,
            mode: 0o755,
            is_symlink: false,
        };
        let postures = [path_loose];
        let path_issue = PathInspectionIssue {
            path: std::path::PathBuf::from("/home/acp/.local/share/acp-stack/state.sqlite"),
            kind: PathKind::StateDb,
            error: "Permission denied".to_owned(),
        };
        let path_issues = [path_issue];
        let dependency_failures = [DependencySecurityFailure {
            name: "cloudflared".to_owned(),
            kind: "command".to_owned(),
            feature: Some("cloudflare-tunnel".to_owned()),
            reason: Some("`cloudflared` not found on PATH".to_owned()),
        }];

        // Pass 1: empty keys.
        let mut empty_inputs = SecurityCheckInputs {
            effective_bind: "0.0.0.0:8080",
            http: &http,
            session_key_empty: true,
            admin_key_empty: true,
            recent_auth_failures: 50,
            sink_open_failures: 2,
            sink_last_error: Some("HTTP 503"),
            session_key_value: None,
            admin_key_value: None,
            path_postures: &postures,
            path_issues: &path_issues,
            process_euid: 1000,
            runtime_user_uid: Some(2000),
            runtime_user_name: "acp",
            workspace_writable: false,
            workspace_root: "/workspace",
            railway_platform: false,
            cloudflare: None,
            cloudflared_available: true,
            recent_direct_cloudflare_mode_requests: 0,
            recent_missing_cloudflare_header_requests: 0,
            dependency_failures: &dependency_failures,
        };
        let empty_findings = check(empty_inputs.clone_for_test());
        assert_remediations_present(&empty_findings);

        // Pass 2: weak (non-empty) keys.
        let weak_key = "CHANGEME";
        empty_inputs.session_key_empty = false;
        empty_inputs.admin_key_empty = false;
        empty_inputs.session_key_value = Some(weak_key);
        empty_inputs.admin_key_value = Some(weak_key);
        let weak_findings = check(empty_inputs);
        assert_remediations_present(&weak_findings);

        // Make sure both passes together cover every code that `check()` can
        // emit, so a future code with no remediation can't sneak past.
        let mut codes: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        for f in empty_findings.iter().chain(weak_findings.iter()) {
            codes.insert(f.code.as_str());
        }
        for expected in [
            "api.public_bind",
            "http.wildcard_origin_public_bind",
            "http.trust_proxy_without_trusted_proxies",
            "auth.session_key_empty",
            "auth.admin_key_empty",
            "auth.failure_threshold",
            "logging.supabase.delivery_failing",
            "auth.session_key_weak",
            "auth.admin_key_weak",
            "runtime.path_ownership",
            "runtime.path_mode_loose",
            "runtime.path_uninspectable",
            "runtime.user_mismatch",
            "runtime.workspace_not_writable",
            "deps.required_unavailable",
        ] {
            assert!(
                codes.contains(expected),
                "every_finding_has_a_non_empty_remediation must exercise {expected}; got {codes:?}"
            );
        }
    }

    fn assert_remediations_present(findings: &[SecurityFinding]) {
        for finding in findings {
            let remediation = finding
                .remediation
                .as_deref()
                .unwrap_or_else(|| panic!("finding {} has no remediation", finding.code));
            assert!(
                !remediation.trim().is_empty(),
                "finding {} has empty remediation",
                finding.code
            );
        }
    }

    impl<'a> SecurityCheckInputs<'a> {
        /// Test-only shallow copy. `SecurityCheckInputs` holds non-`Copy`
        /// borrowed slices so the standard `Clone` derive does not fit; we
        /// reconstruct the borrows verbatim.
        fn clone_for_test(&self) -> SecurityCheckInputs<'a> {
            SecurityCheckInputs {
                effective_bind: self.effective_bind,
                http: self.http,
                session_key_empty: self.session_key_empty,
                admin_key_empty: self.admin_key_empty,
                recent_auth_failures: self.recent_auth_failures,
                sink_open_failures: self.sink_open_failures,
                sink_last_error: self.sink_last_error,
                session_key_value: self.session_key_value,
                admin_key_value: self.admin_key_value,
                path_postures: self.path_postures,
                path_issues: self.path_issues,
                process_euid: self.process_euid,
                runtime_user_uid: self.runtime_user_uid,
                runtime_user_name: self.runtime_user_name,
                workspace_writable: self.workspace_writable,
                workspace_root: self.workspace_root,
                railway_platform: self.railway_platform,
                cloudflare: self.cloudflare,
                cloudflared_available: self.cloudflared_available,
                recent_direct_cloudflare_mode_requests: self.recent_direct_cloudflare_mode_requests,
                recent_missing_cloudflare_header_requests: self
                    .recent_missing_cloudflare_header_requests,
                dependency_failures: self.dependency_failures,
            }
        }
    }
}
