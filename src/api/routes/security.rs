use axum::extract::State;
use chrono::{Duration, SecondsFormat, Utc};
use serde::Serialize;

use super::super::core::AppState;
use crate::envelope::ApiSuccess;
use crate::error::StackError;

#[derive(Serialize)]
pub(crate) struct SecurityCheckResponse {
    ok: bool,
    findings: Vec<crate::security::SecurityFinding>,
    auth_failure_count: i64,
}

pub(crate) async fn security_check_handler(
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<SecurityCheckResponse>, StackError> {
    let store = state.state.lock().await;
    let counts = store.counts()?;
    let recent_cutoff =
        (Utc::now() - Duration::minutes(1)).to_rfc3339_opts(SecondsFormat::Nanos, true);
    let recent_auth_failures = store.count_auth_failures_since(&recent_cutoff)?;
    let sink_open_failures = store.sink_open_failure_count()?;
    let sink_last_error = store
        .latest_sink_failure_summary()?
        .and_then(|(_window, _count, last_error, _observed)| last_error);
    drop(store);
    let findings = crate::security::check(crate::security::SecurityCheckInputs {
        effective_bind: state.effective_bind.as_str(),
        http: &state.config.security.http,
        session_key_empty: state.session_key.is_empty(),
        admin_key_empty: state.admin_key.is_empty(),
        recent_auth_failures,
        sink_open_failures,
        sink_last_error: sink_last_error.as_deref(),
    });
    Ok(ApiSuccess::new(SecurityCheckResponse {
        ok: findings.is_empty(),
        findings,
        auth_failure_count: counts.auth_failures,
    }))
}
