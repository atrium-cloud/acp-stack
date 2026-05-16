use axum::extract::{Query, State};
use serde::{Deserialize, Serialize};

use super::super::core::AppState;
use crate::envelope::ApiSuccess;
use crate::error::StackError;

#[derive(Deserialize, Default)]
#[serde(default)]
pub(crate) struct MetricsSummaryParams {
    /// Window start. Accepts RFC3339 (e.g. `2026-05-16T00:00:00Z`) or a
    /// duration suffix (`1h`, `30m`, `2d`). Defaults to 24h ago.
    since: Option<String>,
    /// Window end. Same format as `since`. Defaults to "now".
    until: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct MetricsSummaryResponse {
    window: MetricsWindowJson,
    counts: MetricsCountsJson,
    sessions: MetricsSessionsJson,
    turns: MetricsTurnsJson,
    commands: MetricsCommandsJson,
    permissions: MetricsPermissionsJson,
    security: MetricsSecurityJson,
    api_connections: MetricsApiConnectionsJson,
    ws_connections: MetricsWsConnectionsJson,
    usage: MetricsUsageJson,
}

#[derive(Serialize)]
pub(crate) struct MetricsWindowJson {
    since: String,
    until: String,
}

#[derive(Serialize)]
pub(crate) struct MetricsCountsJson {
    events: i64,
    sessions: i64,
    commands: i64,
    auth_failures: i64,
    agent_lifecycle: i64,
    installer_runs: i64,
    agent_capabilities: i64,
    prompts: i64,
    permission_requests: i64,
    permission_decisions: i64,
}

#[derive(Serialize)]
pub(crate) struct MetricsSessionsJson {
    active: i64,
    closed: i64,
    average_duration_ms: Option<i64>,
    p50_duration_ms: Option<i64>,
    p95_duration_ms: Option<i64>,
}

#[derive(Serialize)]
pub(crate) struct MetricsTurnsJson {
    total: i64,
    by_status: std::collections::BTreeMap<String, i64>,
    average_per_session: Option<f64>,
}

#[derive(Serialize)]
pub(crate) struct MetricsCommandsJson {
    total: i64,
    by_status: std::collections::BTreeMap<String, i64>,
    average_duration_ms: Option<i64>,
    p50_duration_ms: Option<i64>,
    p95_duration_ms: Option<i64>,
    truncated_count: i64,
}

#[derive(Serialize)]
pub(crate) struct MetricsPermissionsJson {
    total: i64,
    by_outcome: std::collections::BTreeMap<String, i64>,
    average_response_ms: Option<i64>,
    p50_response_ms: Option<i64>,
    p95_response_ms: Option<i64>,
}

#[derive(Serialize)]
pub(crate) struct MetricsSecurityJson {
    auth_failures: i64,
    by_reason: std::collections::BTreeMap<String, i64>,
    events_by_kind: std::collections::BTreeMap<String, i64>,
}

#[derive(Serialize)]
pub(crate) struct MetricsApiConnectionsJson {
    request_count: Option<i64>,
    by_status: std::collections::BTreeMap<String, i64>,
    average_duration_ms: Option<i64>,
}

#[derive(Serialize)]
pub(crate) struct MetricsWsConnectionsJson {
    connections_opened: Option<i64>,
    connections_closed: Option<i64>,
    average_duration_ms: Option<i64>,
}

#[derive(Serialize)]
pub(crate) struct MetricsUsageJson {
    tokens_input: Option<i64>,
    tokens_output: Option<i64>,
    context_window_max: Option<i64>,
}

pub(crate) async fn metrics_summary_handler(
    Query(params): Query<MetricsSummaryParams>,
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<MetricsSummaryResponse>, StackError> {
    let now = chrono::Utc::now();
    let until = match params.until.as_deref() {
        Some(raw) => parse_metrics_bound(raw, now)?,
        None => now,
    };
    let since = match params.since.as_deref() {
        Some(raw) => parse_metrics_bound(raw, now)?,
        None => now - chrono::Duration::hours(24),
    };
    if since > until {
        return Err(StackError::InvalidParam {
            field: "since",
            reason: "must be earlier than until".to_owned(),
        });
    }
    let window = crate::state::MetricsWindow {
        since: format_rfc3339_nanos(since),
        until: format_rfc3339_nanos(until),
    };
    let store = state.state.lock().await;
    let summary = store.metrics_summary(window)?;
    drop(store);
    Ok(ApiSuccess::new(MetricsSummaryResponse::from(summary)))
}

/// Parse either an RFC3339 timestamp or a duration suffix relative to `now`.
/// Accepts `Ns`, `Nm`, `Nh`, `Nd`. The duration suffix subtracts from `now`
/// so callers can pass `since=1h` to mean "an hour ago".
fn parse_metrics_bound(
    raw: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> std::result::Result<chrono::DateTime<chrono::Utc>, StackError> {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(raw) {
        return Ok(dt.with_timezone(&chrono::Utc));
    }
    let duration =
        crate::time_util::parse_duration_suffix(raw).ok_or_else(|| StackError::InvalidParam {
            field: "since/until",
            reason: format!("not a valid RFC3339 timestamp or duration: {raw}"),
        })?;
    Ok(now - duration)
}

fn format_rfc3339_nanos(dt: chrono::DateTime<chrono::Utc>) -> String {
    dt.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}

impl From<crate::state::MetricsSummary> for MetricsSummaryResponse {
    fn from(summary: crate::state::MetricsSummary) -> Self {
        Self {
            window: MetricsWindowJson {
                since: summary.window.since,
                until: summary.window.until,
            },
            counts: MetricsCountsJson {
                events: summary.counts.events,
                sessions: summary.counts.sessions,
                commands: summary.counts.commands,
                auth_failures: summary.counts.auth_failures,
                agent_lifecycle: summary.counts.agent_lifecycle,
                installer_runs: summary.counts.installer_runs,
                agent_capabilities: summary.counts.agent_capabilities,
                prompts: summary.counts.prompts,
                permission_requests: summary.counts.permission_requests,
                permission_decisions: summary.counts.permission_decisions,
            },
            sessions: MetricsSessionsJson {
                active: summary.sessions.active,
                closed: summary.sessions.closed,
                average_duration_ms: summary.sessions.average_duration_ms,
                p50_duration_ms: summary.sessions.p50_duration_ms,
                p95_duration_ms: summary.sessions.p95_duration_ms,
            },
            turns: MetricsTurnsJson {
                total: summary.turns.total,
                by_status: summary.turns.by_status,
                average_per_session: summary.turns.average_per_session,
            },
            commands: MetricsCommandsJson {
                total: summary.commands.total,
                by_status: summary.commands.by_status,
                average_duration_ms: summary.commands.average_duration_ms,
                p50_duration_ms: summary.commands.p50_duration_ms,
                p95_duration_ms: summary.commands.p95_duration_ms,
                truncated_count: summary.commands.truncated_count,
            },
            permissions: MetricsPermissionsJson {
                total: summary.permissions.total,
                by_outcome: summary.permissions.by_outcome,
                average_response_ms: summary.permissions.average_response_ms,
                p50_response_ms: summary.permissions.p50_response_ms,
                p95_response_ms: summary.permissions.p95_response_ms,
            },
            security: MetricsSecurityJson {
                auth_failures: summary.security.auth_failures,
                by_reason: summary.security.by_reason,
                events_by_kind: summary.security.events_by_kind,
            },
            api_connections: MetricsApiConnectionsJson {
                request_count: summary.api_connections.request_count,
                by_status: summary.api_connections.by_status,
                average_duration_ms: summary.api_connections.average_duration_ms,
            },
            ws_connections: MetricsWsConnectionsJson {
                connections_opened: summary.ws_connections.connections_opened,
                connections_closed: summary.ws_connections.connections_closed,
                average_duration_ms: summary.ws_connections.average_duration_ms,
            },
            usage: MetricsUsageJson {
                tokens_input: summary.usage.tokens_input,
                tokens_output: summary.usage.tokens_output,
                context_window_max: summary.usage.context_window_max,
            },
        }
    }
}
