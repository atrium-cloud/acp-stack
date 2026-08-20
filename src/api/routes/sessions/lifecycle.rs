use super::*;

#[derive(Deserialize, Default, schemars::JsonSchema)]
pub(crate) struct SessionsCreateBody {
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default, alias = "target")]
    target_id: Option<String>,
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
    let target = state
        .session_agent_target(payload.target_id.as_deref())
        .await?;
    ensure_agent_started(&state, &target.target_id).await?;
    // Read the agent block from the live cache instead of the cached
    // `state.config.agent`. After `POST /v1/agent/restart` updates
    // the cache, this is how subsequent session creates see the new
    // `agent.model` / `agent.mode` / `agent.provider`. Without this,
    // a post-restart session would still receive the stale config
    // and silently downgrade to the prior model.
    let agent_for_session = target.live_agent_config.lock().await.clone();
    let outcome = target
        .supervisor
        .create_session(
            &target.target_id,
            &agent_for_session,
            &state.config.workspace.root,
            Some(cwd),
            mcp_servers,
            &state.state,
        )
        .await?;
    persist_mcp_attached(&state, &outcome.record.id, &outcome.attached_mcp).await;
    Ok(ApiSuccess::new(SessionResponse::from(outcome)))
}

pub(crate) async fn sessions_get_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<SessionsTargetParams>,
) -> std::result::Result<ApiSuccess<SessionResponse>, StackError> {
    let store = state.state.lock().await;
    let record = store.get_session(&id)?;
    drop(store);
    let record = record.ok_or(StackError::SessionNotFound { id })?;
    if let Some(asserted) = params.target_id.as_deref()
        && asserted != record.target_id
    {
        return Err(StackError::InvalidParam {
            field: "target",
            reason: format!(
                "session `{}` belongs to target `{}`, not `{asserted}`",
                record.id, record.target_id
            ),
        });
    }
    Ok(ApiSuccess::new(SessionResponse::from(record)))
}

#[derive(Deserialize, Default, schemars::JsonSchema)]
pub(crate) struct SessionsLoadBody {
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default, alias = "target")]
    target_id: Option<String>,
    // See `SessionsCreateBody`: MCP servers come from admin config, not
    // session-tier request bodies, until a proper policy surface lands.
}

pub(crate) async fn sessions_load_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Option<Json<SessionsLoadBody>>,
) -> std::result::Result<ApiSuccess<SessionResponse>, StackError> {
    let Json(payload) = body.unwrap_or_default();
    let target = target_for_existing_session(&state, &id, payload.target_id.as_deref()).await?;
    ensure_agent_started(&state, &target.target_id).await?;
    let cwd = payload
        .cwd
        .map(|raw| resolve_session_cwd(Some(raw), &state.config.workspace.root))
        .transpose()?;
    let mcp_servers = open_mcp_servers(&state.config)?;
    let outcome = target
        .supervisor
        .load_session(
            &id,
            cwd,
            mcp_servers,
            &state.config.workspace.root,
            &state.state,
        )
        .await?;
    persist_mcp_attached(&state, &outcome.record.id, &outcome.attached_mcp).await;
    Ok(ApiSuccess::new(SessionResponse::from(outcome)))
}

pub(crate) async fn sessions_resume_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Option<Json<SessionsLoadBody>>,
) -> std::result::Result<ApiSuccess<SessionResponse>, StackError> {
    let Json(payload) = body.unwrap_or_default();
    let target = target_for_existing_session(&state, &id, payload.target_id.as_deref()).await?;
    ensure_agent_started(&state, &target.target_id).await?;
    let cwd = payload
        .cwd
        .map(|raw| resolve_session_cwd(Some(raw), &state.config.workspace.root))
        .transpose()?;
    let mcp_servers = open_mcp_servers(&state.config)?;
    let outcome = target
        .supervisor
        .resume_session(
            &id,
            cwd,
            mcp_servers,
            &state.config.workspace.root,
            &state.state,
        )
        .await?;
    persist_mcp_attached(&state, &outcome.record.id, &outcome.attached_mcp).await;
    Ok(ApiSuccess::new(SessionResponse::from(outcome)))
}

#[derive(Deserialize, Default, schemars::JsonSchema)]
pub(crate) struct SessionsForkBody {
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    message_id: Option<String>,
    #[serde(default, alias = "target")]
    target_id: Option<String>,
}

pub(crate) async fn sessions_fork_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Option<Json<SessionsForkBody>>,
) -> std::result::Result<ApiSuccess<SessionResponse>, StackError> {
    let Json(payload) = body.unwrap_or_default();
    let target = target_for_existing_session(&state, &id, payload.target_id.as_deref()).await?;
    ensure_agent_started(&state, &target.target_id).await?;
    let cwd = payload
        .cwd
        .map(|raw| resolve_session_cwd(Some(raw), &state.config.workspace.root))
        .transpose()?;
    let mcp_servers = open_mcp_servers(&state.config)?;
    let outcome = target
        .supervisor
        .fork_session(
            &id,
            cwd,
            mcp_servers,
            &state.config.workspace.root,
            payload.message_id,
            &state.state,
        )
        .await?;
    persist_mcp_attached(&state, &outcome.record.id, &outcome.attached_mcp).await;
    Ok(ApiSuccess::new(SessionResponse::from(outcome)))
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
