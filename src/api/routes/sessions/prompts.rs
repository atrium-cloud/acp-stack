use super::*;

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
    Query(params): Query<SessionsTargetParams>,
    Json(payload): Json<SessionsPromptBody>,
) -> std::result::Result<ApiSuccess<PromptSubmitResponse>, StackError> {
    let blocks = parse_prompt_blocks(&payload.prompt)?;
    if blocks.is_empty() {
        return Err(StackError::PromptBodyEmpty);
    }
    let target = target_for_existing_session(&state, &id, params.target_id.as_deref()).await?;
    ensure_agent_started(&state, &target.target_id).await?;
    let agent_for_prompt = target.live_agent_config.lock().await.clone();
    state
        .model_catalog
        .ensure_prompt_supported(selected_agent_model(&agent_for_prompt), &blocks)
        .await?;
    // Canonical JSON of the parsed blocks is durable storage; the original
    // request body shape is what the agent sees, so we serialize the typed
    // ACP value (consistent with how we read it back).
    let prompt_json = serde_json::to_string(&blocks).map_err(|err| {
        StackError::PromptBodyInvalid(format!("failed to canonicalize prompt: {err}"))
    })?;
    let record = target
        .supervisor
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

pub(crate) async fn sessions_prompt_status_handler(
    State(state): State<AppState>,
    Path((session_id, prompt_id)): Path<(String, String)>,
    Query(params): Query<SessionsTargetParams>,
) -> std::result::Result<ApiSuccess<PromptStatusResponse>, StackError> {
    let _target =
        target_for_existing_session(&state, &session_id, params.target_id.as_deref()).await?;
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
