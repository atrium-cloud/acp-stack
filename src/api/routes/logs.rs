use axum::extract::{Query, State};
use serde::{Deserialize, Serialize};

use super::super::core::AppState;
use crate::envelope::ApiSuccess;
use crate::error::StackError;
use crate::state::{AuthFailureFilter, EventFilter};

/// Per-request cap on `GET /v1/logs/events?limit=`. An authenticated session
/// could otherwise request billions of rows and turn a log query into a
/// memory-pressure attack. Operators with longer-tail queries should page.
pub(super) const MAX_LOGS_LIMIT: u32 = 1000;

#[derive(Deserialize, Default)]
#[serde(default)]
pub(crate) struct LogsEventsParams {
    #[serde(default = "default_logs_limit")]
    limit: u32,
    level: Option<String>,
    kind: Option<String>,
    source: Option<String>,
    session_id: Option<String>,
    command_id: Option<String>,
    permission_id: Option<String>,
    since: Option<String>,
    until: Option<String>,
    after: Option<String>,
}

pub(super) fn default_logs_limit() -> u32 {
    100
}

/// Split a `kind` query param into either an exact match or a dotted prefix.
/// A trailing `.` (e.g. `command.`) is treated as a prefix; anything else is
/// matched exactly.
pub(super) fn split_kind_filter(kind: Option<&str>) -> (Option<&str>, Option<&str>) {
    match kind {
        Some(k) if k.ends_with('.') => (None, Some(k)),
        Some(k) => (Some(k), None),
        None => (None, None),
    }
}

#[derive(Serialize)]
pub(crate) struct LogsEventsResponse {
    events: Vec<LogEventJson>,
    next_cursor: Option<String>,
}

#[derive(Serialize)]
pub(super) struct LogEventJson {
    id: String,
    created_at: String,
    level: String,
    kind: String,
    message: String,
    payload_json: String,
    source: String,
}

impl From<crate::state::Event> for LogEventJson {
    fn from(e: crate::state::Event) -> Self {
        Self {
            id: e.id,
            created_at: e.created_at,
            level: e.level,
            kind: e.kind,
            message: e.message,
            payload_json: e.payload_json,
            source: e.source,
        }
    }
}

pub(crate) async fn logs_events_handler(
    Query(params): Query<LogsEventsParams>,
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<LogsEventsResponse>, StackError> {
    let limit = params.limit.min(MAX_LOGS_LIMIT);
    let (kind_exact, kind_prefix) = split_kind_filter(params.kind.as_deref());
    let store = state.state.lock().await;
    let events = store.query_events(EventFilter {
        limit,
        level: params.level.as_deref(),
        kind: kind_exact,
        kind_prefix,
        source: params.source.as_deref(),
        session_id: params.session_id.as_deref(),
        command_id: params.command_id.as_deref(),
        permission_id: params.permission_id.as_deref(),
        since: params.since.as_deref(),
        until: params.until.as_deref(),
        after_id: params.after.as_deref(),
    })?;
    drop(store);
    let next_cursor = paging_cursor(&events, limit);
    let events = events.into_iter().map(LogEventJson::from).collect();
    Ok(ApiSuccess::new(LogsEventsResponse {
        events,
        next_cursor,
    }))
}

/// Return the cursor for the next page when `rows` saturated `limit`. When the
/// caller asked for `limit` rows and we returned fewer, there is no next page.
pub(super) fn paging_cursor<T>(rows: &[T], limit: u32) -> Option<String>
where
    T: HasRowId,
{
    if (rows.len() as u32) < limit {
        return None;
    }
    rows.last().map(|row| row.row_id().to_owned())
}

pub(super) trait HasRowId {
    fn row_id(&self) -> &str;
}

impl HasRowId for crate::state::Event {
    fn row_id(&self) -> &str {
        &self.id
    }
}

impl HasRowId for crate::state::SessionRecord {
    fn row_id(&self) -> &str {
        &self.id
    }
}

impl HasRowId for crate::state::CommandRecord {
    fn row_id(&self) -> &str {
        &self.id
    }
}

impl HasRowId for crate::state::AuthFailure {
    fn row_id(&self) -> &str {
        &self.id
    }
}

#[derive(Serialize)]
pub(crate) struct LogsSessionsResponse {
    sessions: Vec<SessionLogJson>,
    next_cursor: Option<String>,
}

#[derive(Serialize)]
struct SessionLogJson {
    id: String,
    created_at: String,
    updated_at: String,
    status: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub(crate) struct LogsSessionsParams {
    #[serde(default = "default_logs_limit")]
    limit: u32,
    since: Option<String>,
    until: Option<String>,
    status: Option<String>,
    after: Option<String>,
}

pub(crate) async fn logs_sessions_handler(
    Query(params): Query<LogsSessionsParams>,
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<LogsSessionsResponse>, StackError> {
    let limit = params.limit.min(MAX_LOGS_LIMIT);
    let store = state.state.lock().await;
    let sessions = store.query_sessions(crate::state::SessionFilter {
        limit,
        since: params.since.as_deref(),
        until: params.until.as_deref(),
        status: params.status.as_deref(),
        after_id: params.after.as_deref(),
    })?;
    drop(store);
    let next_cursor = paging_cursor(&sessions, limit);
    Ok(ApiSuccess::new(LogsSessionsResponse {
        sessions: sessions
            .into_iter()
            .map(|session| SessionLogJson {
                id: session.id,
                created_at: session.created_at,
                updated_at: session.updated_at,
                status: session.status,
            })
            .collect(),
        next_cursor,
    }))
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub(crate) struct LogsLimitParams {
    #[serde(default = "default_logs_limit")]
    pub(super) limit: u32,
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub(crate) struct LogsCommandsParams {
    #[serde(default = "default_logs_limit")]
    limit: u32,
    since: Option<String>,
    until: Option<String>,
    status: Option<String>,
    after: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct LogsCommandsResponse {
    commands: Vec<CommandLogJson>,
    next_cursor: Option<String>,
}

#[derive(Serialize)]
struct CommandLogJson {
    id: String,
    created_at: String,
    updated_at: String,
    status: String,
    command: String,
    exit_status: Option<i64>,
}

pub(crate) async fn logs_commands_handler(
    Query(params): Query<LogsCommandsParams>,
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<LogsCommandsResponse>, StackError> {
    let limit = params.limit.min(MAX_LOGS_LIMIT);
    let store = state.state.lock().await;
    let commands = store.query_commands(crate::state::CommandFilter {
        limit,
        since: params.since.as_deref(),
        until: params.until.as_deref(),
        status: params.status.as_deref(),
        after_id: params.after.as_deref(),
    })?;
    drop(store);
    let next_cursor = paging_cursor(&commands, limit);
    Ok(ApiSuccess::new(LogsCommandsResponse {
        commands: commands
            .into_iter()
            .map(|command| CommandLogJson {
                id: command.id,
                created_at: command.created_at,
                updated_at: command.updated_at,
                status: command.status,
                command: command.command,
                exit_status: command.exit_status,
            })
            .collect(),
        next_cursor,
    }))
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub(crate) struct LogsPermissionsParams {
    #[serde(default = "default_logs_limit")]
    limit: u32,
    kind: Option<String>,
    source: Option<String>,
    since: Option<String>,
    until: Option<String>,
    after: Option<String>,
    permission_id: Option<String>,
}

pub(crate) async fn logs_permissions_handler(
    Query(params): Query<LogsPermissionsParams>,
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<LogsEventsResponse>, StackError> {
    let limit = params.limit.min(MAX_LOGS_LIMIT);
    let (kind_exact, kind_prefix) = split_kind_filter(params.kind.as_deref());
    let store = state.state.lock().await;
    let events = store.query_permission_events(EventFilter {
        limit,
        level: None,
        kind: kind_exact,
        kind_prefix,
        source: params.source.as_deref(),
        session_id: None,
        command_id: None,
        permission_id: params.permission_id.as_deref(),
        since: params.since.as_deref(),
        until: params.until.as_deref(),
        after_id: params.after.as_deref(),
    })?;
    drop(store);
    let next_cursor = paging_cursor(&events, limit);
    let events = events.into_iter().map(LogEventJson::from).collect();
    Ok(ApiSuccess::new(LogsEventsResponse {
        events,
        next_cursor,
    }))
}

#[derive(Serialize)]
pub(crate) struct LogsSecurityResponse {
    auth_failures: Vec<AuthFailureJson>,
    events: Vec<LogEventJson>,
    auth_failures_next_cursor: Option<String>,
    events_next_cursor: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub(crate) struct LogsSecurityParams {
    #[serde(default = "default_logs_limit")]
    limit: u32,
    since: Option<String>,
    until: Option<String>,
    after: Option<String>,
    auth_failures_after: Option<String>,
    events_after: Option<String>,
}

#[derive(Serialize)]
struct AuthFailureJson {
    id: String,
    created_at: String,
    key_kind: String,
    reason: String,
    client_ip: Option<String>,
    route: Option<String>,
    payload_json: String,
}

/// `GET /v1/logs/security` returns both `auth_failures` rows (durable record
/// of every rejected authentication) and `events` rows whose `kind` starts
/// with `security.*` (rate-limit hits, IP blocks, denied origins, oversized
/// requests, etc.). Two independent streams keep their existing schemas;
/// clients merge them on `created_at` for a unified timeline.
pub(crate) async fn logs_security_handler(
    Query(params): Query<LogsSecurityParams>,
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<LogsSecurityResponse>, StackError> {
    let limit = params.limit.min(MAX_LOGS_LIMIT);
    let auth_failures_after = params
        .auth_failures_after
        .as_deref()
        .or(params.after.as_deref());
    let events_after = params.events_after.as_deref().or(params.after.as_deref());
    let store = state.state.lock().await;
    let auth_failures = store.query_auth_failures(AuthFailureFilter {
        limit,
        since: params.since.as_deref(),
        until: params.until.as_deref(),
        after_id: auth_failures_after,
    })?;
    let security_events = store.query_security_events(EventFilter {
        limit,
        since: params.since.as_deref(),
        until: params.until.as_deref(),
        after_id: events_after,
        ..EventFilter::default()
    })?;
    drop(store);
    let auth_failures_next_cursor = paging_cursor(&auth_failures, limit);
    let events_next_cursor = paging_cursor(&security_events, limit);
    Ok(ApiSuccess::new(LogsSecurityResponse {
        auth_failures: auth_failures
            .into_iter()
            .map(|failure| AuthFailureJson {
                id: failure.id,
                created_at: failure.created_at,
                key_kind: failure.key_kind,
                reason: failure.reason,
                client_ip: failure.client_ip,
                route: failure.route,
                payload_json: failure.payload_json,
            })
            .collect(),
        events: security_events
            .into_iter()
            .map(LogEventJson::from)
            .collect(),
        auth_failures_next_cursor,
        events_next_cursor,
    }))
}
