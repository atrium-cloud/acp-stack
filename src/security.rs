//! Runtime security self-check.
//!
//! Examines effective config plus a recent auth-failure count and returns a
//! list of `SecurityFinding`s. The same helper is called by both
//! `GET /v1/security/check` on the admin-tier HTTP API and `acpctl security
//! check` on the local UDS surface, so the policy stays in one place.

use std::net::SocketAddr;
use std::path::PathBuf;

use serde::Serialize;

use crate::config::CloudflareEdgeConfig;
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

/// POSIX shell single-quote escape. Wraps `s` in `'...'` and replaces each
/// embedded `'` with `'\''` so the produced token is safe to paste into a
/// shell, even when the path itself was attacker-controlled (e.g. a workspace
/// root from imported config). Used to render copy-pastable `chown`/`chmod`
/// commands inside `SecurityFinding::remediation`.
fn shell_quote(s: &str) -> String {
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
    /// True when the daemon is running inside Railway's managed runtime.
    /// Railway terminates public HTTP at its edge and may run the container as
    /// root when a persistent volume is attached, even though the image's
    /// normal Docker posture remains `USER acp`.
    pub railway_platform: bool,
    pub cloudflare: Option<&'a CloudflareEdgeConfig>,
    pub cloudflared_available: bool,
    pub recent_direct_cloudflare_mode_requests: i64,
    pub recent_missing_cloudflare_header_requests: i64,
}

/// Compute the list of security findings for the running daemon.
pub fn check(inputs: SecurityCheckInputs<'_>) -> Vec<SecurityFinding> {
    let mut findings = Vec::new();
    let bind_is_public = inputs
        .effective_bind
        .parse::<SocketAddr>()
        .map(|addr| addr.ip().is_unspecified())
        .unwrap_or(false);
    let railway_root_volume_profile = inputs.railway_platform && inputs.process_euid == 0;

    if bind_is_public && !inputs.railway_platform {
        findings.push(
            SecurityFinding::warning(
                "api.public_bind",
                "API bind address listens on all interfaces",
            )
            .with_remediation(
                "Bind to a loopback or private interface, or front the daemon with a \
                 reverse proxy that terminates TLS and enforces auth before traffic \
                 reaches `acps`.",
            ),
        );
    }

    if bind_is_public
        && inputs
            .http
            .allowed_origins
            .iter()
            .any(|origin| origin == "*")
    {
        findings.push(
            SecurityFinding::critical(
                "http.wildcard_origin_public_bind",
                "wildcard CORS origin is configured on a public bind address",
            )
            .with_remediation(
                "Set `[security.http].allowed_origins` to an explicit allow-list of \
                 origins before exposing the bind publicly.",
            ),
        );
    }

    if inputs.http.trust_proxy_headers && inputs.http.trusted_proxies.is_empty() {
        findings.push(
            SecurityFinding::critical(
                "http.trust_proxy_without_trusted_proxies",
                "proxy headers are trusted but no trusted proxy allowlist is configured",
            )
            .with_remediation(
                "Populate `[security.http].trusted_proxies` with the addresses of the \
                 reverse proxies in front of the daemon, or set \
                 `trust_proxy_headers = false`.",
            ),
        );
    }

    if let Some(cloudflare) = inputs.cloudflare
        && cloudflare.enabled
        && cloudflare.exposure == "tunnel"
    {
        if bind_is_public {
            findings.push(
                SecurityFinding::critical(
                    "edge.cloudflare.public_bind_tunnel",
                    "Cloudflare Tunnel mode is configured but the API bind is public",
                )
                .with_remediation(
                    "Set `[api].bind = \"127.0.0.1:7700\"` so only local \
                     cloudflared can reach the daemon.",
                ),
            );
        }
        if inputs.http.allowed_origins.is_empty()
            || inputs
                .http
                .allowed_origins
                .iter()
                .any(|origin| origin == "*")
        {
            findings.push(
                SecurityFinding::critical(
                    "edge.cloudflare.unsafe_origins",
                    "Cloudflare Tunnel mode requires explicit non-wildcard allowed origins",
                )
                .with_remediation(
                    "Set `[security.http].allowed_origins` to the exact \
                     `https://<hostname>` origin served by Cloudflare.",
                ),
            );
        }
        let has_localhost_proxy = inputs
            .http
            .trusted_proxies
            .iter()
            .any(|proxy| proxy == "127.0.0.1")
            && inputs
                .http
                .trusted_proxies
                .iter()
                .any(|proxy| proxy == "::1");
        if !inputs.http.trust_proxy_headers || !has_localhost_proxy {
            findings.push(
                SecurityFinding::critical(
                    "edge.cloudflare.missing_local_trusted_proxies",
                    "Cloudflare Tunnel mode requires localhost trusted proxies",
                )
                .with_remediation(
                    "Set `[security.http].trust_proxy_headers = true` and \
                     `[security.http].trusted_proxies = [\"127.0.0.1\", \"::1\"]`.",
                ),
            );
        }
        if cloudflare.cloudflared_deployment == "host" && !inputs.cloudflared_available {
            findings.push(
                SecurityFinding::warning(
                    "edge.cloudflare.cloudflared_missing",
                    "Cloudflare Tunnel host deployment is configured but cloudflared is unavailable",
                )
                .with_remediation(
                    "Install `cloudflared` on PATH, or set \
                     `cloudflared_deployment = \"docker\"` / \"external\" if it runs outside \
                     the daemon host.",
                ),
            );
        }
        if inputs.recent_missing_cloudflare_header_requests > 0 {
            findings.push(
                SecurityFinding::warning(
                    "edge.cloudflare.headers_missing",
                    "recent trusted-proxy requests were missing Cloudflare headers",
                )
                .with_remediation(
                    "Verify the public hostname routes through Cloudflare Tunnel and that \
                     Cloudflare visitor IP/location headers have not been stripped.",
                ),
            );
        }
        if inputs.recent_direct_cloudflare_mode_requests > 0 {
            findings.push(
                SecurityFinding::critical(
                    "edge.cloudflare.direct_public_requests",
                    "recent requests reached the daemon without the trusted Cloudflare proxy path",
                )
                .with_remediation(
                    "Keep the daemon bound to loopback and ensure firewall or container \
                     networking prevents direct public access.",
                ),
            );
        }
    }

    if inputs.session_key_empty {
        findings.push(
            SecurityFinding::critical("auth.session_key_empty", "session API key is empty")
                .with_remediation(
                    "Run `acps auth regenerate-session-key` to generate a fresh \
                     session key in the encrypted secret store.",
                ),
        );
    }

    if inputs.admin_key_empty {
        findings.push(
            SecurityFinding::critical("auth.admin_key_empty", "admin API key is empty")
                .with_remediation(
                    "Run `acps reset --yes` and re-run `acps init` to provision a new \
                     admin key; the admin key cannot be rotated in place.",
                ),
        );
    }

    let threshold = inputs.http.auth_failures_per_minute;
    if threshold > 0 && inputs.recent_auth_failures >= i64::try_from(threshold).unwrap_or(i64::MAX)
    {
        findings.push(
            SecurityFinding::warning(
                "auth.failure_threshold",
                "auth failure count meets or exceeds the configured per-minute threshold",
            )
            .with_remediation(
                "Inspect `/v1/logs/security` for the failing client (the \
                 durable `auth_failures` rows are surfaced there). If a \
                 session key looks compromised, rotate it with `acps auth \
                 regenerate-session-key`. If the admin key is implicated, \
                 run `acps reset --yes` and re-run `acps init` — the admin \
                 key cannot be rotated in place.",
            ),
        );
    }

    if inputs.sink_open_failures > 0 {
        let suffix = inputs
            .sink_last_error
            .filter(|s| !s.is_empty())
            .map(|err| format!(" (last error: {err})"))
            .unwrap_or_default();
        findings.push(
            SecurityFinding::warning(
                "logging.supabase.delivery_failing",
                &format!(
                    "Supabase sink has {} pending rows with retry failures{suffix}",
                    inputs.sink_open_failures
                ),
            )
            .with_remediation(
                "Check `[logging.supabase]` endpoint reachability and credentials, \
                 then inspect the `sink_outbox` table in the state DB for stuck rows.",
            ),
        );
    }

    // The weakness checks only fire when the empty check didn't already
    // catch the key — an empty key is already a `critical` finding, and
    // reporting both for the same key would be noise.
    if !inputs.session_key_empty
        && let Some(value) = inputs.session_key_value
        && key_is_weak(value)
    {
        findings.push(
            SecurityFinding::warning(
                "auth.session_key_weak",
                "session API key is too short or matches a known weak placeholder",
            )
            .with_remediation(
                "Run `acps auth regenerate-session-key` to replace the key \
                         with a 32-byte random value.",
            ),
        );
    }
    if !inputs.admin_key_empty
        && let Some(value) = inputs.admin_key_value
        && key_is_weak(value)
    {
        findings.push(
            SecurityFinding::warning(
                "auth.admin_key_weak",
                "admin API key is too short or matches a known weak placeholder",
            )
            .with_remediation(
                "Run `acps reset --yes` and re-run `acps init` to provision a \
                         new admin key; the admin key cannot be rotated in place.",
            ),
        );
    }

    for posture in inputs.path_postures {
        // Render the path through `shell_quote` so spaces, single quotes, or
        // other shell metacharacters in the runtime-managed path (which can
        // come from operator-controlled `workspace.root` config) cannot
        // produce an unsafe-to-paste command. `chown -h` also operates on
        // the symlink itself rather than following it — `ownership::inspect`
        // uses `symlink_metadata`, so a symlinked runtime path reports its
        // own posture and that's what we want to fix.
        let path_quoted = shell_quote(&posture.path.display().to_string());
        let railway_workspace_root_ownership = railway_root_volume_profile
            && posture.kind == crate::ownership::PathKind::WorkspaceRoot
            && posture.path == std::path::Path::new("/workspace")
            && inputs.workspace_root == "/workspace"
            && posture.uid == 1000;
        if posture.uid != inputs.process_euid && !railway_workspace_root_ownership {
            findings.push(
                SecurityFinding::critical(
                    "runtime.path_ownership",
                    &format!(
                        "{label} at {path} is owned by uid {actual}, expected uid {expected}",
                        label = posture.kind.label(),
                        path = posture.path.display(),
                        actual = posture.uid,
                        expected = inputs.process_euid,
                    ),
                )
                // The check compares `posture.uid` against `process_euid` (the
                // running daemon), so the hint must name the daemon's uid —
                // not `runtime_user_name`, which could resolve to a different
                // uid (in which case `runtime.user_mismatch` also fires and
                // the operator picks one side to fix). We only suggest
                // `chown`, never "relaunch under {actual}": the path could be
                // owned by root and `acps serve` explicitly refuses root
                // execution outside the disposable/dev profile.
                // `--` terminates option parsing so a path that happens to
                // start with `-` is not interpreted as a chown flag.
                .with_remediation(format!(
                    "Run `chown -h {uid} -- {path_quoted}` (as root); uid \
                     {uid} is the running daemon's effective uid. The `-h` \
                     flag keeps `chown` operating on the path itself if it \
                     is a symlink; the gid is left unchanged because the \
                     check validates owner uid only.",
                    uid = inputs.process_euid,
                )),
            );
        }
        if let Some(expected_mode) = posture.kind.expected_mode()
            && posture.mode != expected_mode
        {
            // Linux `chmod` follows symlinks and has no `-h` equivalent
            // for permissions, so the usual remediation would mutate the
            // wrong target. The runtime never installs symlinks at
            // managed paths (`fs_util::create_dir_owner_only` refuses);
            // an operator hitting this case is recovering from external
            // tampering and needs to remove the link, not chmod through
            // it. Emit a distinct remediation that says so.
            let remediation = if posture.is_symlink {
                format!(
                    "{label} at {path_quoted} is a symlink; \
                         `chmod` would follow it and mutate the wrong \
                         target. Remove the symlink and recreate the \
                         managed path as an owner-only \
                         file/directory.",
                    label = posture.kind.label(),
                )
            } else {
                format!(
                    "Run `chmod 0{expected_mode:o} -- {path_quoted}` to \
                         restore owner-only permissions."
                )
            };
            findings.push(
                SecurityFinding::critical(
                    "runtime.path_mode_loose",
                    &format!(
                        "{label} at {path} has mode 0o{actual:o}, expected 0o{expected:o}",
                        label = posture.kind.label(),
                        path = posture.path.display(),
                        actual = posture.mode,
                        expected = expected_mode,
                    ),
                )
                .with_remediation(remediation),
            );
        }
    }

    for issue in inputs.path_issues {
        let path = issue.path.display().to_string();
        let path_quoted = shell_quote(&path);
        findings.push(
            SecurityFinding::critical(
                "runtime.path_uninspectable",
                &format!(
                    "{label} at {path} could not be inspected: {error}",
                    label = issue.kind.label(),
                    error = issue.error,
                ),
            )
            .with_remediation(format!(
                "Restore {label} at {path_quoted} so the daemon uid {uid} can stat it. \
                 If the file was deleted, restore it from backup or run `acps init` to \
                 recreate missing runtime-managed files, then repair owner-only \
                 permissions with the matching `chmod` hint from `acps security check`.",
                label = issue.kind.label(),
                uid = inputs.process_euid,
            )),
        );
    }

    if let Some(uid) = inputs.runtime_user_uid {
        let railway_runtime_user_mismatch =
            railway_root_volume_profile && inputs.runtime_user_name == "acp" && uid != 0;
        if uid != inputs.process_euid && !railway_runtime_user_mismatch {
            // `[workspace].runtime_user = "root"` (uid 0) is permitted only
            // for the disposable/dev profile via `--allow-root` /
            // `ACP_STACK_ALLOW_ROOT=1`; production deploys must run as an
            // unprivileged user. The remediation reflects that — we never
            // tell an operator to "relaunch as root" to fix the mismatch.
            let remediation = if uid == 0 {
                format!(
                    "Update `[workspace].runtime_user` to an unprivileged \
                     user that matches the launching uid {euid}; root \
                     execution is reserved for the disposable/dev profile.",
                    euid = inputs.process_euid,
                )
            } else {
                format!(
                    "Relaunch the daemon as '{name}' (check the systemd \
                     `User=` directive or the container `USER` instruction), \
                     or update `[workspace].runtime_user` so it matches the \
                     launching uid {euid}.",
                    name = inputs.runtime_user_name,
                    euid = inputs.process_euid,
                )
            };
            findings.push(
                SecurityFinding::warning(
                    "runtime.user_mismatch",
                    &format!(
                        "daemon euid {euid} does not match configured runtime_user \
                         '{name}' (uid {uid})",
                        euid = inputs.process_euid,
                        name = inputs.runtime_user_name,
                    ),
                )
                .with_remediation(remediation),
            );
        }
    }

    if !inputs.workspace_writable {
        findings.push(
            SecurityFinding::critical(
                "runtime.workspace_not_writable",
                &format!(
                    "workspace root {root} is not writable by the running daemon \
                     (uid {euid})",
                    root = inputs.workspace_root,
                    euid = inputs.process_euid,
                ),
            )
            // The probe runs as the daemon's effective uid (see
            // `ownership::workspace_writable`), so the hint must reference
            // that uid — not `runtime_user_name`, which can resolve to a
            // different uid (`runtime.user_mismatch` would fire separately
            // and the operator picks which side to fix).
            .with_remediation(format!(
                "Ensure {root} exists and is writable by uid {euid} (the \
                 daemon's effective uid); check parent directory ownership \
                 and any read-only mount options.",
                root = shell_quote(inputs.workspace_root),
                euid = inputs.process_euid,
            )),
        );
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
    fn railway_root_volume_suppresses_workspace_and_default_user_mismatch() {
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
            !findings.iter().any(|f| f.code == "runtime.path_ownership"),
            "{findings:?}"
        );
        assert!(
            !findings.iter().any(|f| f.code == "runtime.user_mismatch"),
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
            }
        }
    }
}
