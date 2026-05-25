//! API-key posture checks: empty keys, the auth-failure rate threshold, the
//! Supabase logging-sink health summary, and weak/placeholder key values.
//!
//! The `sink_open_failures` check sits in this module because the original
//! linear `check()` interleaved it between the auth-failure threshold and the
//! weak-key checks; keeping the rule order identical here preserves the
//! finding order asserted by the test suite.

use crate::security::SecurityCheckInputs;
use crate::security::findings::{SecurityFinding, key_is_weak};

pub(in crate::security) fn check_keys(
    inputs: &SecurityCheckInputs<'_>,
    findings: &mut Vec<SecurityFinding>,
) {
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
}
