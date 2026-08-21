use super::*;

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct SessionCommandsResponse {
    /// Slash commands the agent last advertised for this session via ACP
    /// `available_commands_update` (latest-wins). Empty when nothing has been
    /// advertised; may be stale until the agent re-advertises.
    available_commands: Vec<AvailableCommandResponse>,
    /// When the stored list was last replaced. `null` when no list has ever
    /// been advertised.
    updated_at: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub(crate) struct SessionCommandRunBody {
    /// Command name as advertised (no leading slash required; one is
    /// stripped if present).
    command: String,
    /// Optional free-form arguments appended after the command name.
    #[serde(default)]
    args: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct SessionCommandRunResponse {
    prompt_id: String,
    session_id: String,
    #[schemars(extend("enum" = ["pending", "running", "completed", "errored", "cancelled", "stalled"]))]
    status: String,
    created_at: String,
    message_id: Option<String>,
    /// Whether the command matched the agent's last advertised list. `false`
    /// means the agent may ignore or misinterpret the invocation; omitted
    /// when no list has ever been advertised. Advisory only — the prompt is
    /// submitted regardless, because agents accept unadvertised commands and
    /// the stored list can be stale.
    #[serde(skip_serializing_if = "Option::is_none")]
    advertised: Option<bool>,
}

pub(crate) async fn sessions_commands_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<SessionsTargetParams>,
) -> std::result::Result<ApiSuccess<SessionCommandsResponse>, StackError> {
    let store = state.state.lock().await;
    let session = store
        .get_session(&id)?
        .ok_or_else(|| StackError::SessionNotFound { id: id.clone() })?;
    drop(store);
    if let Some(asserted) = params.target_id.as_deref()
        && asserted != session.target_id
    {
        return Err(StackError::InvalidParam {
            field: "target",
            reason: format!(
                "session `{}` belongs to target `{}`, not `{asserted}`",
                session.id, session.target_id
            ),
        });
    }
    let stored = stored_available_commands(&session.metadata_json);
    let (available_commands, updated_at) = stored
        .map(|stored| {
            (
                stored
                    .commands
                    .into_iter()
                    .map(AvailableCommandResponse::from)
                    .collect(),
                stored.updated_at,
            )
        })
        .unwrap_or_default();
    Ok(ApiSuccess::new(SessionCommandsResponse {
        available_commands,
        updated_at,
    }))
}

pub(crate) async fn sessions_commands_run_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<SessionsTargetParams>,
    Json(payload): Json<SessionCommandRunBody>,
) -> std::result::Result<ApiSuccess<SessionCommandRunResponse>, StackError> {
    let trimmed = payload.command.trim();
    let command = trimmed.strip_prefix('/').unwrap_or(trimmed).trim();
    if command.is_empty() {
        return Err(StackError::InvalidParam {
            field: "command",
            reason: "command name must not be empty".to_owned(),
        });
    }
    // One session fetch serves both the target assertion and the advisory
    // check; Array-mode gating still runs through `session_agent_target`.
    let session = {
        let store = state.state.lock().await;
        store
            .get_session(&id)?
            .ok_or_else(|| StackError::SessionNotFound { id: id.clone() })?
    };
    if let Some(asserted) = params.target_id.as_deref()
        && asserted != session.target_id
    {
        return Err(StackError::InvalidParam {
            field: "target",
            reason: format!(
                "session `{}` belongs to target `{}`, not `{asserted}`",
                session.id, session.target_id
            ),
        });
    }
    let target = state.session_agent_target(Some(&session.target_id)).await?;
    // Advisory check against the last advertised list. Never a hard block:
    // agents accept unadvertised commands and the stored list can be stale,
    // but some agents silently no-op on commands they do not recognize, so
    // the mismatch is worth surfacing to the caller.
    let advertised = stored_available_commands(&session.metadata_json)
        .map(|stored| stored.commands.iter().any(|entry| entry.name == command));
    if advertised == Some(false) {
        tracing::warn!(
            session_id = %id,
            command = %command,
            "session command not in the agent's advertised list; submitting anyway"
        );
    }
    // Over ACP a slash command is invoked as an ordinary `session/prompt`
    // whose text starts with `/name`; there is no dedicated method.
    let args = payload.args.as_deref().map(str::trim).unwrap_or_default();
    let text = if args.is_empty() {
        format!("/{command}")
    } else {
        format!("/{command} {args}")
    };
    let blocks = parse_prompt_blocks(&serde_json::Value::String(text))?;
    ensure_agent_started(&state, &target.target_id).await?;
    let agent_for_prompt = target.live_agent_config.lock().await.clone();
    state
        .model_catalog
        .ensure_prompt_supported(selected_agent_model(&agent_for_prompt), &blocks)
        .await?;
    let prompt_json = serde_json::to_string(&blocks).map_err(|err| {
        StackError::PromptBodyInvalid(format!("failed to canonicalize prompt: {err}"))
    })?;
    let record = target
        .supervisor
        .submit_prompt(&id, blocks, prompt_json, &state.state)
        .await?;
    Ok(ApiSuccess::new(SessionCommandRunResponse {
        prompt_id: record.id,
        session_id: record.session_id,
        status: record.status,
        created_at: record.created_at,
        message_id: record.message_id,
        advertised,
    }))
}
