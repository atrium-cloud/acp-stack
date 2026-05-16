use axum::Json;
use axum::extract::{Path, Query, State};
use serde::{Deserialize, Serialize};

use super::super::core::AppState;
use super::agent::open_mcp_servers;
use super::logs::{LogEventJson, MAX_LOGS_LIMIT, default_logs_limit};
use crate::envelope::ApiSuccess;
use crate::error::{Result, StackError};
use crate::state::{PromptRecord, SessionRecord};
use crate::supervisor::parse_prompt_blocks;

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
}

#[derive(Deserialize, Default)]
pub(crate) struct SessionsListParams {
    #[serde(default = "default_logs_limit")]
    limit: u32,
}

pub(crate) async fn sessions_list_handler(
    Query(params): Query<SessionsListParams>,
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<SessionsListResponse>, StackError> {
    let limit = params.limit.min(MAX_LOGS_LIMIT);
    let store = state.state.lock().await;
    let sessions = store.query_sessions(crate::state::SessionFilter {
        limit,
        ..Default::default()
    })?;
    drop(store);
    Ok(ApiSuccess::new(SessionsListResponse {
        sessions: sessions.into_iter().map(SessionResponse::from).collect(),
    }))
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
    let server_names = crate::mcp::server_names(&mcp_servers);
    let record = state
        .agent_supervisor
        .create_session(
            &state.config.agent,
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
    let server_names = crate::mcp::server_names(&mcp_servers);
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
    let server_names = crate::mcp::server_names(&mcp_servers);
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
