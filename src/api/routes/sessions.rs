use axum::Json;
use axum::extract::{Path, Query, State};
use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

use super::super::core::AppState;
use super::agent::open_mcp_servers;
use super::logs::{LogEventJson, MAX_LOGS_LIMIT, default_logs_limit};
use crate::envelope::ApiSuccess;
use crate::error::{Result, StackError};
use crate::runtime::agent::supervisor::SessionListSyncResult;
use crate::runtime::agent::supervisor::parse_prompt_blocks;
use crate::state::{
    DEFAULT_SESSION_ACTIVITY_THRESHOLD, PromptRecord, SESSION_STATUS_ACTIVE, SessionActivityRecord,
    SessionRecord, SessionUpdateBounds,
};

#[derive(Serialize)]
pub(crate) struct SessionResponse {
    id: String,
    created_at: String,
    updated_at: String,
    status: String,
    agent_id: String,
    cwd: String,
    title: Option<String>,
    metadata_json: String,
}

impl From<SessionRecord> for SessionResponse {
    fn from(record: SessionRecord) -> Self {
        Self {
            id: record.id,
            created_at: record.created_at,
            updated_at: record.updated_at,
            status: record.status,
            agent_id: record.agent_id,
            cwd: record.cwd,
            title: record.title,
            metadata_json: record.metadata_json,
        }
    }
}

#[derive(Serialize)]
pub(crate) struct SessionsListResponse {
    sessions: Vec<SessionResponse>,
    agent_sync: SessionsAgentSyncResponse,
}

#[derive(Serialize)]
pub(crate) struct SessionsAgentSyncResponse {
    attempted: bool,
    status: String,
    upserted: u32,
    updated: u32,
}

impl From<SessionListSyncResult> for SessionsAgentSyncResponse {
    fn from(result: SessionListSyncResult) -> Self {
        Self {
            attempted: result.attempted,
            status: result.status.as_str().to_owned(),
            upserted: result.upserted,
            updated: result.updated,
        }
    }
}

#[derive(Deserialize, Default)]
pub(crate) struct SessionsListParams {
    #[serde(default = "default_logs_limit")]
    limit: u32,
    since: Option<String>,
    until: Option<String>,
    range: Option<String>,
    #[serde(default)]
    resolve_bounds: bool,
}

pub(crate) async fn sessions_list_handler(
    Query(params): Query<SessionsListParams>,
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<SessionsListResponse>, StackError> {
    let limit = params.limit.min(MAX_LOGS_LIMIT);
    let now = Utc::now();
    let agent_for_session = state.live_agent_config.lock().await.clone();
    let agent_sync = state
        .agent_supervisor
        .sync_listed_sessions(&agent_for_session, &state.state)
        .await?;
    let store = state.state.lock().await;
    let bounds = store.session_update_bounds()?;
    let (since, until) = resolve_session_list_bounds(&params, bounds.as_ref(), now)?;
    let sessions = store.query_sessions(crate::state::SessionFilter {
        limit,
        since: since.as_deref(),
        until: until.as_deref(),
        ..Default::default()
    })?;
    drop(store);
    Ok(ApiSuccess::new(SessionsListResponse {
        sessions: sessions.into_iter().map(SessionResponse::from).collect(),
        agent_sync: agent_sync.into(),
    }))
}

fn resolve_session_list_bounds(
    params: &SessionsListParams,
    bounds: Option<&SessionUpdateBounds>,
    now: chrono::DateTime<Utc>,
) -> Result<(Option<String>, Option<String>)> {
    let until = match params.until.as_deref() {
        Some(raw) => resolve_time_bound(Some(raw), "until", now)?,
        None if params.resolve_bounds => default_until_bound(bounds, now)?,
        None => None,
    };
    let since = match params.since.as_deref() {
        Some(raw) => resolve_time_bound(Some(raw), "since", now)?,
        None if params.resolve_bounds => bounds.map(|b| b.first_updated_at.clone()),
        None => params
            .range
            .as_deref()
            .map(|range| resolve_range_start(range, now))
            .transpose()?
            .flatten(),
    };
    Ok((since, until))
}

fn default_until_bound(
    bounds: Option<&SessionUpdateBounds>,
    now: chrono::DateTime<Utc>,
) -> Result<Option<String>> {
    let Some(bounds) = bounds else {
        return Ok(None);
    };
    if bounds.latest_status == SESSION_STATUS_ACTIVE {
        return Ok(Some(now.to_rfc3339_opts(SecondsFormat::Nanos, true)));
    }
    let latest = parse_normalized_time_bound(&bounds.latest_updated_at, "latest_updated_at")?;
    Ok(Some(
        (latest + chrono::Duration::nanoseconds(1)).to_rfc3339_opts(SecondsFormat::Nanos, true),
    ))
}

fn resolve_range_start(raw: &str, now: chrono::DateTime<Utc>) -> Result<Option<String>> {
    if raw == "all" {
        return Ok(None);
    }
    let duration = session_range_duration(raw).ok_or_else(|| StackError::InvalidParam {
        field: "range",
        reason: format!(
            "expected day, week, month, year, all, or a duration like 30m, 60d, 6mo, or 1y; got {raw}"
        ),
    })?;
    let resolved =
        crate::time_util::resolve_since_after_unix_epoch(duration, now).ok_or_else(|| {
            StackError::InvalidParam {
                field: "range",
                reason: "duration range must not begin before 1970-01-01T00:00:00Z".to_owned(),
            }
        })?;
    Ok(Some(resolved.to_rfc3339_opts(SecondsFormat::Nanos, true)))
}

fn session_range_duration(raw: &str) -> Option<chrono::Duration> {
    match raw {
        "day" => Some(chrono::Duration::days(1)),
        "week" => Some(chrono::Duration::weeks(1)),
        "month" => Some(chrono::Duration::days(30)),
        "year" => Some(chrono::Duration::days(365)),
        other => crate::time_util::parse_coarse_duration_suffix(other),
    }
}

fn parse_normalized_time_bound(raw: &str, field: &'static str) -> Result<chrono::DateTime<Utc>> {
    chrono::DateTime::parse_from_rfc3339(raw)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|err| StackError::InvalidParam {
            field,
            reason: format!("not a valid RFC3339 timestamp: {err}"),
        })
}

#[derive(Deserialize)]
pub(crate) struct SessionsStatusParams {
    #[serde(default = "default_session_status_threshold")]
    threshold: String,
    #[serde(default = "default_session_status_limit")]
    limit: u32,
}

fn default_session_status_threshold() -> String {
    DEFAULT_SESSION_ACTIVITY_THRESHOLD.to_owned()
}

fn default_session_status_limit() -> u32 {
    MAX_LOGS_LIMIT
}

#[derive(Serialize)]
pub(crate) struct SessionsStatusResponse {
    generated_at: String,
    threshold: String,
    active_count: usize,
    truncated: bool,
    sessions: Vec<SessionStatusSessionResponse>,
}

#[derive(Serialize)]
pub(crate) struct SessionStatusSessionResponse {
    id: String,
    status: String,
    agent_id: String,
    cwd: String,
    title: Option<String>,
    last_activity_at: String,
    last_activity_from: String,
    recent: bool,
}

impl SessionStatusSessionResponse {
    fn from_record(
        record: SessionActivityRecord,
        cutoff: chrono::DateTime<Utc>,
    ) -> std::result::Result<Self, StackError> {
        let last_activity = chrono::DateTime::parse_from_rfc3339(&record.last_activity_at)
            .map_err(|err| StackError::InvalidParam {
                field: "last_activity_at",
                reason: format!("stored session activity timestamp is invalid: {err}"),
            })?
            .with_timezone(&Utc);
        Ok(Self {
            id: record.id,
            status: record.status,
            agent_id: record.agent_id,
            cwd: record.cwd,
            title: record.title,
            last_activity_at: record.last_activity_at,
            last_activity_from: record.last_activity_from,
            recent: last_activity >= cutoff,
        })
    }
}

pub(crate) async fn sessions_status_handler(
    Query(params): Query<SessionsStatusParams>,
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<SessionsStatusResponse>, StackError> {
    let threshold =
        crate::time_util::parse_duration_suffix(&params.threshold).ok_or_else(|| {
            StackError::InvalidParam {
                field: "threshold",
                reason: format!(
                    "not a valid duration; expected values like `{}` or `30m`",
                    DEFAULT_SESSION_ACTIVITY_THRESHOLD
                ),
            }
        })?;
    let generated_at = Utc::now();
    let cutoff = generated_at - threshold;
    let limit = params.limit.min(MAX_LOGS_LIMIT);
    let query_limit = limit.saturating_add(1);
    let store = state.state.lock().await;
    let mut rows = store.query_active_session_activity(query_limit)?;
    drop(store);
    let truncated = rows.len() > limit as usize;
    if truncated {
        rows.truncate(limit as usize);
    }
    let sessions = rows
        .into_iter()
        .map(|row| SessionStatusSessionResponse::from_record(row, cutoff))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(ApiSuccess::new(SessionsStatusResponse {
        generated_at: generated_at.to_rfc3339_opts(SecondsFormat::Nanos, true),
        threshold: params.threshold,
        active_count: sessions.len(),
        truncated,
        sessions,
    }))
}

fn resolve_time_bound(
    raw: Option<&str>,
    field: &'static str,
    now: chrono::DateTime<Utc>,
) -> Result<Option<String>> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(raw) {
        return Ok(Some(
            dt.with_timezone(&Utc)
                .to_rfc3339_opts(SecondsFormat::Nanos, true),
        ));
    }
    let duration = crate::time_util::parse_coarse_duration_suffix(raw).ok_or_else(|| {
        StackError::InvalidParam {
            field,
            reason: format!("not a valid RFC3339 timestamp or duration (m, h, d, w, mo, y): {raw}"),
        }
    })?;
    let resolved =
        crate::time_util::resolve_since_after_unix_epoch(duration, now).ok_or_else(|| {
            StackError::InvalidParam {
                field,
                reason: "duration range must not begin before 1970-01-01T00:00:00Z".to_owned(),
            }
        })?;
    Ok(Some(resolved.to_rfc3339_opts(SecondsFormat::Nanos, true)))
}

#[derive(Deserialize, Default)]
pub(crate) struct SessionsCreateBody {
    #[serde(default)]
    cwd: Option<String>,
    // `mcp_servers` is intentionally omitted from the public surface in this
    // batch. The spec (`docs/specs/acp/acp-bridge.md`) declares MCP servers
    // through admin-controlled config, not the session API. Accepting an
    // ad-hoc list from session-tier callers would let any session-key
    // holder request arbitrary agent-side process execution.
}

pub(crate) async fn sessions_create_handler(
    State(state): State<AppState>,
    body: Option<Json<SessionsCreateBody>>,
) -> std::result::Result<ApiSuccess<SessionResponse>, StackError> {
    let Json(payload) = body.unwrap_or_default();
    let cwd = resolve_session_cwd(payload.cwd, &state.config.workspace.root)?;
    let mcp_servers = open_mcp_servers(&state.config)?;
    let server_names = crate::runtime::agent::mcp::server_names(&mcp_servers);
    // Read the agent block from the live cache instead of the cached
    // `state.config.agent`. After `POST /v1/agent/restart` updates
    // the cache, this is how subsequent session creates see the new
    // `agent.model` / `agent.mode` / `agent.provider`. Without this,
    // a post-restart session would still receive the stale config
    // and silently downgrade to the prior model.
    let agent_for_session = state.live_agent_config.lock().await.clone();
    let record = state
        .agent_supervisor
        .create_session(
            &agent_for_session,
            &state.config.workspace.root,
            Some(cwd),
            mcp_servers,
            &state.state,
        )
        .await?;
    persist_mcp_attached(&state, &record.id, &server_names).await;
    Ok(ApiSuccess::new(SessionResponse::from(record)))
}

pub(crate) async fn sessions_get_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> std::result::Result<ApiSuccess<SessionResponse>, StackError> {
    let store = state.state.lock().await;
    let record = store.get_session(&id)?;
    drop(store);
    let record = record.ok_or(StackError::SessionNotFound { id })?;
    Ok(ApiSuccess::new(SessionResponse::from(record)))
}

#[derive(Deserialize, Default)]
pub(crate) struct SessionsLoadBody {
    #[serde(default)]
    cwd: Option<String>,
    // See `SessionsCreateBody`: MCP servers come from admin config, not
    // session-tier request bodies, until a proper policy surface lands.
}

pub(crate) async fn sessions_load_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Option<Json<SessionsLoadBody>>,
) -> std::result::Result<ApiSuccess<SessionResponse>, StackError> {
    let Json(payload) = body.unwrap_or_default();
    let cwd = payload
        .cwd
        .map(|raw| resolve_session_cwd(Some(raw), &state.config.workspace.root))
        .transpose()?;
    let mcp_servers = open_mcp_servers(&state.config)?;
    let server_names = crate::runtime::agent::mcp::server_names(&mcp_servers);
    let record = state
        .agent_supervisor
        .load_session(
            &id,
            cwd,
            mcp_servers,
            &state.config.workspace.root,
            &state.state,
        )
        .await?;
    persist_mcp_attached(&state, &record.id, &server_names).await;
    Ok(ApiSuccess::new(SessionResponse::from(record)))
}

pub(crate) async fn sessions_resume_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Option<Json<SessionsLoadBody>>,
) -> std::result::Result<ApiSuccess<SessionResponse>, StackError> {
    let Json(payload) = body.unwrap_or_default();
    let cwd = payload
        .cwd
        .map(|raw| resolve_session_cwd(Some(raw), &state.config.workspace.root))
        .transpose()?;
    let mcp_servers = open_mcp_servers(&state.config)?;
    let server_names = crate::runtime::agent::mcp::server_names(&mcp_servers);
    let record = state
        .agent_supervisor
        .resume_session(
            &id,
            cwd,
            mcp_servers,
            &state.config.workspace.root,
            &state.state,
        )
        .await?;
    persist_mcp_attached(&state, &record.id, &server_names).await;
    Ok(ApiSuccess::new(SessionResponse::from(record)))
}

#[derive(Deserialize, Default)]
pub(crate) struct SessionsForkBody {
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    message_id: Option<String>,
}

pub(crate) async fn sessions_fork_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Option<Json<SessionsForkBody>>,
) -> std::result::Result<ApiSuccess<SessionResponse>, StackError> {
    let Json(payload) = body.unwrap_or_default();
    let cwd = payload
        .cwd
        .map(|raw| resolve_session_cwd(Some(raw), &state.config.workspace.root))
        .transpose()?;
    let mcp_servers = open_mcp_servers(&state.config)?;
    let server_names = crate::runtime::agent::mcp::server_names(&mcp_servers);
    let record = state
        .agent_supervisor
        .fork_session(
            &id,
            cwd,
            mcp_servers,
            &state.config.workspace.root,
            payload.message_id,
            &state.state,
        )
        .await?;
    persist_mcp_attached(&state, &record.id, &server_names).await;
    Ok(ApiSuccess::new(SessionResponse::from(record)))
}

async fn persist_mcp_attached(state: &AppState, session_id: &str, names: &[String]) {
    if names.is_empty() {
        return;
    }
    let payload = serde_json::json!({
        "session_id": session_id,
        "server_names": names,
    });
    let payload_text = payload.to_string();
    let store = state.state.lock().await;
    if let Err(err) = store.append_session_event_with_source(
        session_id,
        "info",
        "mcp.session_attached",
        crate::state::EVENT_SOURCE_API,
        "mcp servers attached to session",
        &payload_text,
    ) {
        tracing::warn!(error = %err, session_id, "failed to record mcp.session_attached event");
    }
}

/// Resolve and validate a session `cwd` against `workspace.root`. Returns
/// the canonical (no `..`) string. Rejects anything outside the workspace
/// boundary; this is the same containment the Workspace API will share when
/// it lands.
fn resolve_session_cwd(raw: Option<String>, workspace_root: &str) -> Result<String> {
    let candidate = raw.unwrap_or_else(|| workspace_root.to_owned());
    let root_path = std::path::PathBuf::from(workspace_root);
    let candidate_path = std::path::PathBuf::from(&candidate);
    if !candidate_path.is_absolute() {
        return Err(StackError::PromptBodyInvalid(
            "session cwd must be an absolute path".to_owned(),
        ));
    }
    // Reject `..` segments before normalization rather than relying on
    // canonicalize, because the path may not exist yet and canonicalize would
    // fail. The spec already forbids traversal; we enforce it lexically.
    if candidate_path
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(StackError::PromptBodyInvalid(
            "session cwd must not contain `..` segments".to_owned(),
        ));
    }
    if !candidate_path.starts_with(&root_path) {
        return Err(StackError::PromptBodyInvalid(format!(
            "session cwd must be under workspace.root ({workspace_root})"
        )));
    }
    Ok(candidate)
}

#[derive(Deserialize)]
pub(crate) struct SessionsPromptBody {
    prompt: serde_json::Value,
}

#[derive(Serialize)]
pub(crate) struct PromptSubmitResponse {
    prompt_id: String,
    session_id: String,
    status: String,
    created_at: String,
    message_id: Option<String>,
}

pub(crate) async fn sessions_prompt_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(payload): Json<SessionsPromptBody>,
) -> std::result::Result<ApiSuccess<PromptSubmitResponse>, StackError> {
    let blocks = parse_prompt_blocks(&payload.prompt)?;
    if blocks.is_empty() {
        return Err(StackError::PromptBodyEmpty);
    }
    // Canonical JSON of the parsed blocks is durable storage; the original
    // request body shape is what the agent sees, so we serialize the typed
    // ACP value (consistent with how we read it back).
    let prompt_json = serde_json::to_string(&blocks).map_err(|err| {
        StackError::PromptBodyInvalid(format!("failed to canonicalize prompt: {err}"))
    })?;
    let record = state
        .agent_supervisor
        .submit_prompt(&id, blocks, prompt_json, &state.state)
        .await?;
    Ok(ApiSuccess::new(PromptSubmitResponse {
        prompt_id: record.id,
        session_id: record.session_id,
        status: record.status,
        created_at: record.created_at,
        message_id: record.message_id,
    }))
}

#[derive(Serialize)]
pub(crate) struct PromptStatusResponse {
    id: String,
    session_id: String,
    created_at: String,
    updated_at: String,
    status: String,
    stop_reason: Option<String>,
    error_code: Option<String>,
    error_message: Option<String>,
    message_id: Option<String>,
    message_id_acknowledged: bool,
}

impl From<PromptRecord> for PromptStatusResponse {
    fn from(r: PromptRecord) -> Self {
        Self {
            id: r.id,
            session_id: r.session_id,
            created_at: r.created_at,
            updated_at: r.updated_at,
            status: r.status,
            stop_reason: r.stop_reason,
            error_code: r.error_code,
            error_message: r.error_message,
            message_id: r.message_id,
            message_id_acknowledged: r.message_id_acknowledged,
        }
    }
}

pub(crate) async fn sessions_prompt_status_handler(
    State(state): State<AppState>,
    Path((session_id, prompt_id)): Path<(String, String)>,
) -> std::result::Result<ApiSuccess<PromptStatusResponse>, StackError> {
    let store = state.state.lock().await;
    let record = store.get_prompt(&prompt_id)?;
    drop(store);
    let record = record.ok_or_else(|| StackError::PromptNotFound {
        id: prompt_id.clone(),
    })?;
    if record.session_id != session_id {
        return Err(StackError::PromptSessionMismatch {
            session_id,
            prompt_id,
        });
    }
    Ok(ApiSuccess::new(PromptStatusResponse::from(record)))
}

#[derive(Deserialize, Default)]
pub(crate) struct SessionsEventsParams {
    #[serde(default = "default_logs_limit")]
    limit: u32,
    #[serde(default)]
    after: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct SessionsEventsResponse {
    events: Vec<LogEventJson>,
}

pub(crate) async fn sessions_events_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<SessionsEventsParams>,
) -> std::result::Result<ApiSuccess<SessionsEventsResponse>, StackError> {
    let limit = params.limit.min(MAX_LOGS_LIMIT);
    let store = state.state.lock().await;
    let exists = store.get_session(&id)?.is_some();
    if !exists {
        return Err(StackError::SessionNotFound { id });
    }
    let events = store.query_session_events(&id, params.after.as_deref(), limit)?;
    drop(store);
    Ok(ApiSuccess::new(SessionsEventsResponse {
        events: events.into_iter().map(LogEventJson::from).collect(),
    }))
}

/// Max number of recent events returned by the snapshot endpoint. Chosen to
/// fit a typical reconnect bootstrap (one prompt-turn's worth of updates)
/// without bloating the response; callers that need more should follow up
/// with `GET /v1/sessions/{id}/events` paginated by cursor.
const SNAPSHOT_RECENT_EVENTS_LIMIT: u32 = 50;

/// Max number of in-flight prompts surfaced in a snapshot. Normal sessions
/// have one in-flight prompt at a time; the cap is defense-in-depth against
/// pathological cases (e.g. a misbehaving client submitting faster than
/// settles) so the snapshot stays bounded.
const SNAPSHOT_IN_FLIGHT_PROMPTS_CAP: usize = 25;

#[derive(Serialize)]
pub(crate) struct SessionSnapshotResponse {
    session: SessionResponse,
    in_flight_prompts: Vec<PromptStatusResponse>,
    last_event_id: Option<String>,
    recent_events: Vec<LogEventJson>,
}

pub(crate) async fn sessions_snapshot_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> std::result::Result<ApiSuccess<SessionSnapshotResponse>, StackError> {
    let store = state.state.lock().await;
    let session = store
        .get_session(&id)?
        .ok_or_else(|| StackError::SessionNotFound { id: id.clone() })?;
    let in_flight = store.in_flight_prompts_for_session(&id)?;
    let recent = store.latest_session_events(&id, SNAPSHOT_RECENT_EVENTS_LIMIT)?;
    drop(store);
    // `latest_session_events` returns newest-first; the cursor for the next
    // refresh is the id at the head of the slice (or null when empty).
    let last_event_id = recent.first().map(|event| event.id.clone());
    let recent_events = recent.into_iter().map(LogEventJson::from).collect();
    let in_flight_prompts = in_flight
        .into_iter()
        .take(SNAPSHOT_IN_FLIGHT_PROMPTS_CAP)
        .map(PromptStatusResponse::from)
        .collect();
    Ok(ApiSuccess::new(SessionSnapshotResponse {
        session: SessionResponse::from(session),
        in_flight_prompts,
        last_event_id,
        recent_events,
    }))
}

#[derive(Serialize)]
pub(crate) struct SessionsCancelResponse {
    session_id: String,
}

pub(crate) async fn sessions_cancel_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> std::result::Result<ApiSuccess<SessionsCancelResponse>, StackError> {
    state
        .agent_supervisor
        .cancel_session(&id, &state.state)
        .await?;
    cancel_pending_acp_permissions_for_session(&state, &id, "session-canceled").await;
    Ok(ApiSuccess::new(SessionsCancelResponse { session_id: id }))
}

pub(crate) async fn sessions_close_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> std::result::Result<ApiSuccess<SessionResponse>, StackError> {
    let record = state
        .agent_supervisor
        .close_session(&id, &state.state)
        .await?;
    cancel_pending_acp_permissions_for_session(&state, &id, "session-closed").await;
    Ok(ApiSuccess::new(SessionResponse::from(record)))
}

/// When a session closes or is canceled, any in-flight ACP-source permission
/// rows for that session must be settled — otherwise the operator UI shows
/// stale "pending" rows that won't resolve until the per-request timer fires
/// (default 5 minutes). The ACP-side prompt-turn is already dead; the durable
/// row should reflect that immediately.
async fn cancel_pending_acp_permissions_for_session(
    state: &AppState,
    session_id: &str,
    reason: &str,
) {
    // Read every pending row, filter by source=acp + subject_id=session.
    // The list is small in practice (one prompt turn at a time); no need to
    // push the filter into SQL.
    let pending = match state.permissions.pending(MAX_LOGS_LIMIT).await {
        Ok(rows) => rows,
        Err(err) => {
            tracing::warn!(error = %err, session_id, "failed to load pending permissions for session close");
            return;
        }
    };
    for row in pending {
        if row.source != "acp" {
            continue;
        }
        if row.subject_id.as_deref() != Some(session_id) {
            continue;
        }
        if let Err(err) = state.permissions.cancel(&row.id, reason).await {
            tracing::warn!(
                error = %err,
                permission_id = %row.id,
                session_id,
                "failed to cancel pending ACP permission on session teardown",
            );
        }
    }
}
