//! Auth-failure rate threshold and Supabase logging-sink health checks.

use crate::security::SecurityCheckInputs;
use crate::security::findings::SecurityFinding;

pub(in crate::security) fn check_keys(
    inputs: &SecurityCheckInputs<'_>,
    findings: &mut Vec<SecurityFinding>,
) {
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
}
