//! Derived metrics: counts, durations, percentiles aggregated over a window.
//!
//! The wire-facing JSON shape for `/v1/metrics/summary` lives in `api`; this
//! module is the boundary between raw SQLite aggregation and downstream
//! consumers (HTTP handler, CLI pretty-printer, future Supabase mirror).
//! Percentiles are computed in Rust because SQLite has no `percentile_cont`.

use crate::error::Result;
use rusqlite::{OptionalExtension, params};

use super::core::StateStore;
use super::sessions::{EVENT_KIND_PROMPT_INFERENCE_FAILED, FailureClass};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StateCounts {
    pub events: i64,
    pub sessions: i64,
    pub commands: i64,
    pub auth_failures: i64,
    pub agent_lifecycle: i64,
    pub installer_runs: i64,
    pub agent_capabilities: i64,
    pub prompts: i64,
    pub permission_requests: i64,
    pub permission_decisions: i64,
}

/// Time window for `metrics_summary`. Both bounds are inclusive on `since`,
/// exclusive on `until`, RFC3339 with 9-digit subseconds (matches the
/// `current_timestamp` format used throughout the schema).
#[derive(Debug, Clone)]
pub struct MetricsWindow {
    pub since: String,
    pub until: String,
}

/// Derived per-window metrics. All counts/aggregates are scoped to the same
/// `MetricsWindow`; percentiles are `None` when the underlying sample is
/// empty.
#[derive(Debug, Clone)]
pub struct MetricsSummary {
    pub window: MetricsWindow,
    pub counts: StateCounts,
    pub sessions: SessionMetrics,
    pub turns: TurnMetrics,
    pub prompt_failures: PromptFailureMetrics,
    pub commands: CommandMetrics,
    pub permissions: PermissionMetrics,
    pub security: SecurityMetrics,
    pub api_connections: ApiConnectionMetrics,
    pub ws_connections: WsConnectionMetrics,
    pub usage: UsageMetrics,
}

#[derive(Debug, Clone, Default)]
pub struct SessionMetrics {
    pub active: i64,
    pub closed: i64,
    pub average_duration_ms: Option<i64>,
    pub p50_duration_ms: Option<i64>,
    pub p95_duration_ms: Option<i64>,
}

#[derive(Debug, Clone, Default)]
pub struct TurnMetrics {
    pub total: i64,
    pub by_status: std::collections::BTreeMap<String, i64>,
    pub average_per_session: Option<f64>,
}

#[derive(Debug, Clone, Default)]
pub struct PromptFailureMetrics {
    pub total: i64,
    pub inference_5xx: i64,
    pub inference_4xx: i64,
    pub agent_request: i64,
    pub vm: i64,
    pub sqlite: i64,
    pub daemon: i64,
    pub agent_process: i64,
    pub stalled: i64,
    pub by_class: std::collections::BTreeMap<String, i64>,
    pub by_status_code: std::collections::BTreeMap<String, i64>,
    pub by_reason_category: std::collections::BTreeMap<String, i64>,
}

#[derive(Debug, Clone, Default)]
pub struct CommandMetrics {
    pub total: i64,
    pub by_status: std::collections::BTreeMap<String, i64>,
    pub average_duration_ms: Option<i64>,
    pub p50_duration_ms: Option<i64>,
    pub p95_duration_ms: Option<i64>,
    pub truncated_count: i64,
}

#[derive(Debug, Clone, Default)]
pub struct PermissionMetrics {
    pub total: i64,
    pub by_outcome: std::collections::BTreeMap<String, i64>,
    pub average_response_ms: Option<i64>,
    pub p50_response_ms: Option<i64>,
    pub p95_response_ms: Option<i64>,
}

#[derive(Debug, Clone, Default)]
pub struct SecurityMetrics {
    pub auth_failures: i64,
    pub by_reason: std::collections::BTreeMap<String, i64>,
    pub events_by_kind: std::collections::BTreeMap<String, i64>,
}

#[derive(Debug, Clone, Default)]
pub struct ApiConnectionMetrics {
    /// `None` when no `api.request` events were emitted in the window. The
    /// middleware that emits these events landed in 0.0.3; on a runtime that
    /// pre-dates it, callers can still tell the difference between "instrument
    /// not installed" (None) and "instrument installed, no requests" (Some(0)).
    pub request_count: Option<i64>,
    pub by_status: std::collections::BTreeMap<String, i64>,
    pub average_duration_ms: Option<i64>,
}

#[derive(Debug, Clone, Default)]
pub struct WsConnectionMetrics {
    pub connections_opened: Option<i64>,
    pub connections_closed: Option<i64>,
    pub average_duration_ms: Option<i64>,
}

#[derive(Debug, Clone, Default)]
pub struct UsageMetrics {
    pub tokens_input: Option<i64>,
    pub tokens_output: Option<i64>,
    pub context_window_max: Option<i64>,
}

/// Convert an RFC3339 timestamp to an `Option<DateTime<Utc>>`. Returns None
/// on parse failure rather than propagating an error: metrics aggregations
/// tolerate (and skip) malformed rows so a single bad input cannot blank out
/// an entire summary.
fn parse_rfc3339(input: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(input)
        .ok()
        .map(|dt| dt.with_timezone(&chrono::Utc))
}

/// Return the wall-clock duration between two RFC3339 timestamps in
/// milliseconds (b - a). Returns None when either side fails to parse OR the
/// computed duration is negative (clock skew across rows).
fn duration_ms_between(start: &str, end: &str) -> Option<i64> {
    let start = parse_rfc3339(start)?;
    let end = parse_rfc3339(end)?;
    let delta = end.signed_duration_since(start).num_milliseconds();
    if delta < 0 { None } else { Some(delta) }
}

fn average_i64(values: &[i64]) -> Option<i64> {
    if values.is_empty() {
        return None;
    }
    let sum: i128 = values.iter().map(|v| *v as i128).sum();
    Some((sum / values.len() as i128) as i64)
}

/// Linear-interpolated percentile (matches numpy default). For a near-empty
/// or single-value sample this collapses to that value.
fn percentile_i64(values: &[i64], q: f64) -> Option<i64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted: Vec<i64> = values.to_vec();
    sorted.sort_unstable();
    if sorted.len() == 1 {
        return Some(sorted[0]);
    }
    let rank = q * (sorted.len() - 1) as f64;
    let lower = rank.floor() as usize;
    let upper = rank.ceil() as usize;
    if lower == upper {
        return Some(sorted[lower]);
    }
    let weight = rank - lower as f64;
    let interp = sorted[lower] as f64 + weight * (sorted[upper] - sorted[lower]) as f64;
    Some(interp.round() as i64)
}

impl StateStore {
    pub fn counts(&self) -> Result<StateCounts> {
        Ok(StateCounts {
            events: self.count_table("events")?,
            sessions: self.count_table("sessions")?,
            commands: self.count_table("commands")?,
            auth_failures: self.count_table("auth_failures")?,
            agent_lifecycle: self.count_table("agent_lifecycle")?,
            installer_runs: self.count_table("installer_runs")?,
            agent_capabilities: self.count_table("agent_capabilities")?,
            prompts: self.count_table("prompts")?,
            permission_requests: self.count_table("permission_requests")?,
            permission_decisions: self.count_table("permission_decisions")?,
        })
    }

    /// Counts scoped to a `[since, until)` window keyed on the supplied
    /// timestamp column. SQLite optimizes the range scan via the
    /// `created_at` indexes added in migration 007.
    fn count_table_in_window(
        &self,
        table: &'static str,
        timestamp_column: &'static str,
        window: &MetricsWindow,
    ) -> Result<i64> {
        let sql = format!(
            "SELECT COUNT(*) FROM {table} WHERE {timestamp_column} >= ?1 AND {timestamp_column} < ?2"
        );
        Ok(self
            .connection()
            .query_row(&sql, params![window.since, window.until], |row| row.get(0))?)
    }

    /// Compute the full metrics summary for a `[since, until)` window. The
    /// caller (HTTP handler / CLI) decides the window; pure-SQLite aggregation
    /// keeps this function side-effect-free and re-runnable. Percentiles are
    /// computed in Rust because SQLite has no `percentile_cont`; the row
    /// counts in a reasonable window (hours-to-days × thousands) are small
    /// enough that loading them into memory once per query is fine.
    pub fn metrics_summary(&self, window: MetricsWindow) -> Result<MetricsSummary> {
        let counts = StateCounts {
            events: self.count_table_in_window("events", "created_at", &window)?,
            sessions: self.count_table_in_window("sessions", "created_at", &window)?,
            commands: self.count_table_in_window("commands", "created_at", &window)?,
            auth_failures: self.count_table_in_window("auth_failures", "created_at", &window)?,
            agent_lifecycle: self.count_table_in_window(
                "agent_lifecycle",
                "created_at",
                &window,
            )?,
            installer_runs: self.count_table_in_window("installer_runs", "started_at", &window)?,
            agent_capabilities: self.count_table_in_window(
                "agent_capabilities",
                "captured_at",
                &window,
            )?,
            prompts: self.count_table_in_window("prompts", "created_at", &window)?,
            permission_requests: self.count_table_in_window(
                "permission_requests",
                "created_at",
                &window,
            )?,
            permission_decisions: self.count_table_in_window(
                "permission_decisions",
                "created_at",
                &window,
            )?,
        };

        let sessions = self.session_metrics(&window)?;
        let turns = self.turn_metrics(&window, sessions.active + sessions.closed)?;
        let prompt_failures = self.prompt_failure_metrics(&window)?;
        let commands = self.command_metrics(&window)?;
        let permissions = self.permission_metrics(&window)?;
        let security = self.security_metrics(&window)?;
        let api_connections = self.api_connection_metrics(&window)?;
        let ws_connections = self.ws_connection_metrics(&window)?;
        let usage = self.usage_metrics(&window)?;

        Ok(MetricsSummary {
            window,
            counts,
            sessions,
            turns,
            prompt_failures,
            commands,
            permissions,
            security,
            api_connections,
            ws_connections,
            usage,
        })
    }

    fn session_metrics(&self, window: &MetricsWindow) -> Result<SessionMetrics> {
        let active: i64 = self.connection().query_row(
            "SELECT COUNT(*) FROM sessions \
             WHERE status = 'active' AND created_at >= ?1 AND created_at < ?2",
            params![window.since, window.until],
            |row| row.get(0),
        )?;
        let closed: i64 = self.connection().query_row(
            "SELECT COUNT(*) FROM sessions \
             WHERE status != 'active' AND created_at >= ?1 AND created_at < ?2",
            params![window.since, window.until],
            |row| row.get(0),
        )?;
        let mut statement = self.connection().prepare(
            "SELECT created_at, updated_at FROM sessions \
             WHERE status != 'active' AND created_at >= ?1 AND created_at < ?2",
        )?;
        let durations: Vec<i64> = statement
            .query_map(params![window.since, window.until], |row| {
                let created_at: String = row.get(0)?;
                let updated_at: String = row.get(1)?;
                Ok(duration_ms_between(&created_at, &updated_at))
            })?
            .filter_map(|res| match res {
                Ok(Some(ms)) => Some(Ok(ms)),
                Ok(None) => None,
                Err(err) => Some(Err(err)),
            })
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(SessionMetrics {
            active,
            closed,
            average_duration_ms: average_i64(&durations),
            p50_duration_ms: percentile_i64(&durations, 0.50),
            p95_duration_ms: percentile_i64(&durations, 0.95),
        })
    }

    fn turn_metrics(&self, window: &MetricsWindow, session_count: i64) -> Result<TurnMetrics> {
        let mut by_status = std::collections::BTreeMap::new();
        let mut statement = self.connection().prepare(
            "SELECT status, COUNT(*) FROM prompts \
             WHERE created_at >= ?1 AND created_at < ?2 GROUP BY status",
        )?;
        let rows = statement.query_map(params![window.since, window.until], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        let mut total: i64 = 0;
        for entry in rows {
            let (status, count) = entry?;
            total += count;
            by_status.insert(status, count);
        }
        let average_per_session = if session_count > 0 {
            Some(total as f64 / session_count as f64)
        } else {
            None
        };
        Ok(TurnMetrics {
            total,
            by_status,
            average_per_session,
        })
    }

    fn prompt_failure_metrics(&self, window: &MetricsWindow) -> Result<PromptFailureMetrics> {
        let mut out = PromptFailureMetrics::default();
        let mut statement = self.connection().prepare(
            "SELECT failure_class, COUNT(*) FROM prompts \
             WHERE status IN ('errored', 'stalled') \
               AND failure_class IS NOT NULL \
               AND updated_at >= ?1 AND updated_at < ?2 \
             GROUP BY failure_class",
        )?;
        let rows = statement.query_map(params![window.since, window.until], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        for entry in rows {
            let (failure_class, count) = entry?;
            out.total += count;
            match failure_class.as_str() {
                value if value == FailureClass::Inference5xx.as_str() => out.inference_5xx = count,
                value if value == FailureClass::Inference4xx.as_str() => out.inference_4xx = count,
                value if value == FailureClass::AgentRequest.as_str() => out.agent_request = count,
                value if value == FailureClass::Vm.as_str() => out.vm = count,
                value if value == FailureClass::Sqlite.as_str() => out.sqlite = count,
                value if value == FailureClass::Daemon.as_str() => out.daemon = count,
                value if value == FailureClass::AgentProcess.as_str() => out.agent_process = count,
                value if value == FailureClass::Stalled.as_str() => out.stalled = count,
                _ => {}
            }
            out.by_class.insert(failure_class, count);
        }

        let mut event_statement = self.connection().prepare(
            "SELECT payload_json FROM events \
             WHERE kind = ?1 AND created_at >= ?2 AND created_at < ?3",
        )?;
        let event_rows = event_statement.query_map(
            params![
                EVENT_KIND_PROMPT_INFERENCE_FAILED,
                window.since,
                window.until
            ],
            |row| row.get::<_, String>(0),
        )?;
        for entry in event_rows {
            let payload_json = entry?;
            let Ok(payload) = serde_json::from_str::<serde_json::Value>(&payload_json) else {
                continue;
            };
            if let Some(status_code) = payload.get("status_code").and_then(|value| value.as_u64()) {
                let key = status_code.to_string();
                *out.by_status_code.entry(key).or_insert(0) += 1;
            }
            if let Some(reason) = payload
                .get("reason_category")
                .and_then(serde_json::Value::as_str)
            {
                *out.by_reason_category.entry(reason.to_owned()).or_insert(0) += 1;
            }
        }
        Ok(out)
    }

    fn command_metrics(&self, window: &MetricsWindow) -> Result<CommandMetrics> {
        let mut by_status = std::collections::BTreeMap::new();
        let mut statement = self.connection().prepare(
            "SELECT status, COUNT(*) FROM commands \
             WHERE created_at >= ?1 AND created_at < ?2 GROUP BY status",
        )?;
        let rows = statement.query_map(params![window.since, window.until], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        let mut total: i64 = 0;
        for entry in rows {
            let (status, count) = entry?;
            total += count;
            by_status.insert(status, count);
        }
        let truncated_count: i64 = self.connection().query_row(
            "SELECT COUNT(*) FROM commands \
             WHERE truncated = 1 AND created_at >= ?1 AND created_at < ?2",
            params![window.since, window.until],
            |row| row.get(0),
        )?;
        let mut durations_statement = self.connection().prepare(
            "SELECT duration_ms FROM commands \
             WHERE duration_ms IS NOT NULL AND created_at >= ?1 AND created_at < ?2",
        )?;
        let durations: Vec<i64> = durations_statement
            .query_map(params![window.since, window.until], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(CommandMetrics {
            total,
            by_status,
            average_duration_ms: average_i64(&durations),
            p50_duration_ms: percentile_i64(&durations, 0.50),
            p95_duration_ms: percentile_i64(&durations, 0.95),
            truncated_count,
        })
    }

    fn permission_metrics(&self, window: &MetricsWindow) -> Result<PermissionMetrics> {
        let total: i64 = self.connection().query_row(
            "SELECT COUNT(*) FROM permission_requests \
             WHERE created_at >= ?1 AND created_at < ?2",
            params![window.since, window.until],
            |row| row.get(0),
        )?;
        let mut by_outcome = std::collections::BTreeMap::new();
        let mut statement = self.connection().prepare(
            "SELECT status, COUNT(*) FROM permission_requests \
             WHERE created_at >= ?1 AND created_at < ?2 GROUP BY status",
        )?;
        let rows = statement.query_map(params![window.since, window.until], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        for entry in rows {
            let (status, count) = entry?;
            by_outcome.insert(status, count);
        }
        // Response time = time between request created_at and decision
        // created_at. Joining on the decision row covers approve/deny/cancel/
        // expired terminal outcomes; pending rows are excluded (no decision).
        let mut response_statement = self.connection().prepare(
            "SELECT r.created_at, d.created_at FROM permission_requests r \
             JOIN permission_decisions d ON d.request_id = r.id \
             WHERE r.created_at >= ?1 AND r.created_at < ?2",
        )?;
        let response_durations: Vec<i64> = response_statement
            .query_map(params![window.since, window.until], |row| {
                let req: String = row.get(0)?;
                let dec: String = row.get(1)?;
                Ok(duration_ms_between(&req, &dec))
            })?
            .filter_map(|res| match res {
                Ok(Some(ms)) => Some(Ok(ms)),
                Ok(None) => None,
                Err(err) => Some(Err(err)),
            })
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(PermissionMetrics {
            total,
            by_outcome,
            average_response_ms: average_i64(&response_durations),
            p50_response_ms: percentile_i64(&response_durations, 0.50),
            p95_response_ms: percentile_i64(&response_durations, 0.95),
        })
    }

    fn security_metrics(&self, window: &MetricsWindow) -> Result<SecurityMetrics> {
        let auth_failures: i64 = self.connection().query_row(
            "SELECT COUNT(*) FROM auth_failures \
             WHERE created_at >= ?1 AND created_at < ?2",
            params![window.since, window.until],
            |row| row.get(0),
        )?;
        let mut by_reason = std::collections::BTreeMap::new();
        let mut reason_statement = self.connection().prepare(
            "SELECT reason, COUNT(*) FROM auth_failures \
             WHERE created_at >= ?1 AND created_at < ?2 GROUP BY reason",
        )?;
        let reason_rows = reason_statement
            .query_map(params![window.since, window.until], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?;
        for entry in reason_rows {
            let (reason, count) = entry?;
            by_reason.insert(reason, count);
        }
        let mut events_by_kind = std::collections::BTreeMap::new();
        let mut kind_statement = self.connection().prepare(
            "SELECT kind, COUNT(*) FROM events \
             WHERE kind LIKE 'security.%' AND created_at >= ?1 AND created_at < ?2 GROUP BY kind",
        )?;
        let kind_rows = kind_statement.query_map(params![window.since, window.until], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        for entry in kind_rows {
            let (kind, count) = entry?;
            events_by_kind.insert(kind, count);
        }
        Ok(SecurityMetrics {
            auth_failures,
            by_reason,
            events_by_kind,
        })
    }

    fn api_connection_metrics(&self, window: &MetricsWindow) -> Result<ApiConnectionMetrics> {
        let request_count: i64 = self.connection().query_row(
            "SELECT COUNT(*) FROM events \
             WHERE kind = 'api.request' AND created_at >= ?1 AND created_at < ?2",
            params![window.since, window.until],
            |row| row.get(0),
        )?;
        if request_count == 0 {
            return Ok(ApiConnectionMetrics::default());
        }
        let mut by_status = std::collections::BTreeMap::new();
        let mut status_statement = self.connection().prepare(
            "SELECT \
                CASE \
                    WHEN CAST(json_extract(payload_json, '$.status') AS INTEGER) BETWEEN 200 AND 299 THEN '2xx' \
                    WHEN CAST(json_extract(payload_json, '$.status') AS INTEGER) BETWEEN 300 AND 399 THEN '3xx' \
                    WHEN CAST(json_extract(payload_json, '$.status') AS INTEGER) BETWEEN 400 AND 499 THEN '4xx' \
                    WHEN CAST(json_extract(payload_json, '$.status') AS INTEGER) BETWEEN 500 AND 599 THEN '5xx' \
                    ELSE 'other' \
                END AS bucket, COUNT(*) \
             FROM events \
             WHERE kind = 'api.request' AND created_at >= ?1 AND created_at < ?2 \
             GROUP BY bucket",
        )?;
        let status_rows = status_statement
            .query_map(params![window.since, window.until], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?;
        for entry in status_rows {
            let (bucket, count) = entry?;
            by_status.insert(bucket, count);
        }
        let mut duration_statement = self.connection().prepare(
            "SELECT CAST(json_extract(payload_json, '$.duration_ms') AS INTEGER) FROM events \
             WHERE kind = 'api.request' \
               AND json_extract(payload_json, '$.duration_ms') IS NOT NULL \
               AND created_at >= ?1 AND created_at < ?2",
        )?;
        let durations: Vec<i64> = duration_statement
            .query_map(params![window.since, window.until], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(ApiConnectionMetrics {
            request_count: Some(request_count),
            by_status,
            average_duration_ms: average_i64(&durations),
        })
    }

    fn ws_connection_metrics(&self, window: &MetricsWindow) -> Result<WsConnectionMetrics> {
        let opened: i64 = self.connection().query_row(
            "SELECT COUNT(*) FROM events \
             WHERE kind = 'ws.client_connected' AND created_at >= ?1 AND created_at < ?2",
            params![window.since, window.until],
            |row| row.get(0),
        )?;
        let closed: i64 = self.connection().query_row(
            "SELECT COUNT(*) FROM events \
             WHERE kind = 'ws.client_disconnected' AND created_at >= ?1 AND created_at < ?2",
            params![window.since, window.until],
            |row| row.get(0),
        )?;
        if opened == 0 && closed == 0 {
            return Ok(WsConnectionMetrics::default());
        }
        let mut duration_statement = self.connection().prepare(
            "SELECT CAST(json_extract(payload_json, '$.duration_ms') AS INTEGER) FROM events \
             WHERE kind = 'ws.client_disconnected' \
               AND json_extract(payload_json, '$.duration_ms') IS NOT NULL \
               AND created_at >= ?1 AND created_at < ?2",
        )?;
        let durations: Vec<i64> = duration_statement
            .query_map(params![window.since, window.until], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(WsConnectionMetrics {
            connections_opened: Some(opened),
            connections_closed: Some(closed),
            average_duration_ms: average_i64(&durations),
        })
    }

    fn usage_metrics(&self, window: &MetricsWindow) -> Result<UsageMetrics> {
        // Sum across all `usage.reported` events. Agents that don't emit these
        // leave every field None. The `MAX(context_window_max)` keeps the
        // largest context size seen in the window — most agents emit this as
        // a static capability per session.
        let row = self
            .connection()
            .query_row(
                "SELECT \
                    SUM(CAST(json_extract(payload_json, '$.input_tokens') AS INTEGER)), \
                    SUM(CAST(json_extract(payload_json, '$.output_tokens') AS INTEGER)), \
                    MAX(CAST(json_extract(payload_json, '$.context_window_max') AS INTEGER)) \
                 FROM events \
                 WHERE kind = 'usage.reported' AND created_at >= ?1 AND created_at < ?2",
                params![window.since, window.until],
                |row| {
                    Ok(UsageMetrics {
                        tokens_input: row.get(0)?,
                        tokens_output: row.get(1)?,
                        context_window_max: row.get(2)?,
                    })
                },
            )
            .optional()?;
        Ok(row.unwrap_or_default())
    }

    pub(super) fn count_table(&self, table: &'static str) -> Result<i64> {
        let sql = match table {
            "events" => "SELECT COUNT(*) FROM events",
            "sessions" => "SELECT COUNT(*) FROM sessions",
            "commands" => "SELECT COUNT(*) FROM commands",
            "auth_failures" => "SELECT COUNT(*) FROM auth_failures",
            "agent_lifecycle" => "SELECT COUNT(*) FROM agent_lifecycle",
            "installer_runs" => "SELECT COUNT(*) FROM installer_runs",
            "agent_capabilities" => "SELECT COUNT(*) FROM agent_capabilities",
            "prompts" => "SELECT COUNT(*) FROM prompts",
            "permission_requests" => "SELECT COUNT(*) FROM permission_requests",
            "permission_decisions" => "SELECT COUNT(*) FROM permission_decisions",
            _ => unreachable!("count_table only accepts known migrated tables"),
        };
        Ok(self.connection().query_row(sql, [], |row| row.get(0))?)
    }
}
