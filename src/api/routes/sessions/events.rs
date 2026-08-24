use super::*;

#[derive(Deserialize, Default, schemars::JsonSchema)]
pub(crate) struct SessionsEventsParams {
    /// Values above 1000 are silently clamped to 1000, not rejected.
    #[serde(default = "default_logs_limit")]
    limit: u32,
    #[serde(default)]
    after: Option<String>,
    #[serde(default, alias = "target")]
    target_id: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
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
    if let Some(asserted) = params.target_id.as_deref() {
        let record = store
            .get_session(&id)?
            .ok_or_else(|| StackError::SessionNotFound { id: id.clone() })?;
        if asserted != record.target_id {
            return Err(StackError::InvalidParam {
                field: "target",
                reason: format!(
                    "session `{}` belongs to target `{}`, not `{asserted}`",
                    record.id, record.target_id
                ),
            });
        }
    }
    let events = store.query_session_events(&id, params.after.as_deref(), limit)?;
    drop(store);
    Ok(ApiSuccess::new(SessionsEventsResponse {
        events: events.into_iter().map(LogEventJson::from).collect(),
    }))
}

pub(crate) async fn sessions_changes_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<SessionsTargetParams>,
) -> std::result::Result<ApiSuccess<SessionChangesSnapshot>, StackError> {
    resolved_stored_target_id(&state, &id, params.target_id.as_deref()).await?;
    let snapshot = state.session_changes.snapshot(&id).await;
    Ok(ApiSuccess::new(snapshot))
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

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct SessionSnapshotResponse {
    session: SessionResponse,
    in_flight_prompts: Vec<PromptStatusResponse>,
    last_event_id: Option<String>,
    recent_events: Vec<LogEventJson>,
    /// Slash commands the agent last advertised for this session via ACP
    /// `available_commands_update` (latest-wins). Empty when nothing has been
    /// advertised; may be stale until the agent re-advertises.
    available_commands: Vec<AvailableCommandResponse>,
}

pub(crate) async fn sessions_snapshot_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<SessionsTargetParams>,
) -> std::result::Result<ApiSuccess<SessionSnapshotResponse>, StackError> {
    let store = state.state.lock().await;
    let session = store
        .get_session(&id)?
        .ok_or_else(|| StackError::SessionNotFound { id: id.clone() })?;
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
    let in_flight = store.in_flight_prompts_for_session(&id)?;
    let recent = store.latest_session_events(&id, SNAPSHOT_RECENT_EVENTS_LIMIT)?;
    drop(store);
    let available_commands = stored_available_commands(&session.metadata_json)
        .map(|stored| {
            stored
                .commands
                .into_iter()
                .map(AvailableCommandResponse::from)
                .collect()
        })
        .unwrap_or_default();
    // `latest_session_events` returns newest-first, so the next cursor is at the head of the slice.
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
        available_commands,
    }))
}
