//! Time-and-sequence-based ID generation for durable records.
//!
//! Every record table gets monotonically-sortable IDs of the form
//! `{prefix}_{nanos:020}_{sequence:010}_{pid:010}`. The atomics reset on
//! process start; the PID disambiguates IDs generated in the same nanosecond
//! by concurrent `acps` invocations.

use chrono::{SecondsFormat, Utc};
use std::sync::atomic::{AtomicU64, Ordering};

static EVENT_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static AUTH_FAILURE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static AGENT_LIFECYCLE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static INSTALLER_RUN_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static PROMPT_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static SESSION_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static COMMAND_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static PERMISSION_REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static PERMISSION_DECISION_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(super) fn current_timestamp() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true)
}

pub(super) fn next_event_id() -> String {
    // timestamp_nanos_opt() returns Option; for real clocks since 1970 it is always
    // Some and positive. Falling back to 0 keeps IDs sortable on a wildly skewed clock.
    let nanos = Utc::now().timestamp_nanos_opt().unwrap_or(0).max(0) as u128;
    let sequence = EVENT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    // PID disambiguates events from concurrent acps invocations that land in the same
    // nanosecond with the same per-process sequence value, since EVENT_SEQUENCE resets
    // on every process start.
    let pid = std::process::id();
    format!("evt_{nanos:020}_{sequence:010}_{pid:010}")
}

pub(super) fn next_auth_failure_id() -> String {
    let nanos = Utc::now().timestamp_nanos_opt().unwrap_or(0).max(0) as u128;
    let sequence = AUTH_FAILURE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    format!("af_{nanos:020}_{sequence:010}_{pid:010}")
}

pub(super) fn next_agent_lifecycle_id() -> String {
    let nanos = Utc::now().timestamp_nanos_opt().unwrap_or(0).max(0) as u128;
    let sequence = AGENT_LIFECYCLE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    format!("agl_{nanos:020}_{sequence:010}_{pid:010}")
}

pub(super) fn next_installer_run_id() -> String {
    let nanos = Utc::now().timestamp_nanos_opt().unwrap_or(0).max(0) as u128;
    let sequence = INSTALLER_RUN_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    format!("ins_{nanos:020}_{sequence:010}_{pid:010}")
}

pub fn next_session_id() -> String {
    let nanos = Utc::now().timestamp_nanos_opt().unwrap_or(0).max(0) as u128;
    let sequence = SESSION_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    format!("sess_{nanos:020}_{sequence:010}_{pid:010}")
}

pub fn next_prompt_id() -> String {
    let nanos = Utc::now().timestamp_nanos_opt().unwrap_or(0).max(0) as u128;
    let sequence = PROMPT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    format!("prm_{nanos:020}_{sequence:010}_{pid:010}")
}

pub fn next_command_id() -> String {
    let nanos = Utc::now().timestamp_nanos_opt().unwrap_or(0).max(0) as u128;
    let sequence = COMMAND_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    format!("cmd_{nanos:020}_{sequence:010}_{pid:010}")
}

pub fn next_permission_request_id() -> String {
    let nanos = Utc::now().timestamp_nanos_opt().unwrap_or(0).max(0) as u128;
    let sequence = PERMISSION_REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    format!("perm_{nanos:020}_{sequence:010}_{pid:010}")
}

pub fn next_permission_decision_id() -> String {
    let nanos = Utc::now().timestamp_nanos_opt().unwrap_or(0).max(0) as u128;
    let sequence = PERMISSION_DECISION_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    format!("pdec_{nanos:020}_{sequence:010}_{pid:010}")
}
