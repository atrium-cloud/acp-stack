use super::*;

/// Decision reasons recorded on permission requests settled by a session
/// teardown. The cancel path has its own, recorded by the supervisor that
/// settles those requests.
const SESSION_CLOSED_PERMISSION_REASON: &str = "session-closed";
const SESSION_DELETED_PERMISSION_REASON: &str = "session-deleted";

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct SessionsCancelResponse {
    session_id: String,
}

pub(crate) async fn sessions_cancel_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<SessionsTargetParams>,
) -> std::result::Result<ApiSuccess<SessionsCancelResponse>, StackError> {
    let target = target_for_session_wind_down(&state, &id, params.target_id.as_deref()).await?;
    // The cancel itself settles this session's pending permission requests: an
    // agent parked on one cannot end its turn until it is answered, and the
    // supervisor waits for that turn to settle before returning.
    target
        .supervisor
        .cancel_session(&id, &state.state, &state.permissions)
        .await?;
    Ok(ApiSuccess::new(SessionsCancelResponse { session_id: id }))
}

pub(crate) async fn sessions_close_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<SessionsTargetParams>,
) -> std::result::Result<ApiSuccess<SessionResponse>, StackError> {
    let target = target_for_session_wind_down(&state, &id, params.target_id.as_deref()).await?;
    let record = target.supervisor.close_session(&id, &state.state).await?;
    cancel_pending_acp_permissions_for_session(&state, &id, SESSION_CLOSED_PERMISSION_REASON).await;
    Ok(ApiSuccess::new(SessionResponse::from(record)))
}

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct SessionsDeleteResponse {
    session_id: String,
    deleted: bool,
}

pub(crate) async fn sessions_delete_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<SessionsTargetParams>,
) -> std::result::Result<ApiSuccess<SessionsDeleteResponse>, StackError> {
    // Unknown ids are a silent success per ACP session/delete, so this uses a single lookup rather
    // than the shared resolvers, which would turn a concurrent delete racing this handler into a 404.
    let stored_target_id = {
        let store = state.state.lock().await;
        store.get_session(&id)?.map(|record| record.target_id)
    };
    let Some(stored_target_id) = stored_target_id else {
        return Ok(ApiSuccess::new(SessionsDeleteResponse {
            session_id: id,
            deleted: false,
        }));
    };
    if let Some(asserted) = params.target_id.as_deref()
        && asserted != stored_target_id
    {
        return Err(StackError::InvalidParam {
            field: "target",
            reason: format!(
                "session `{id}` belongs to target `{stored_target_id}`, not `{asserted}`"
            ),
        });
    }
    let target = state.existing_session_target(&stored_target_id).await?;
    let deleted = target
        .supervisor
        .delete_session(&id, &state.state)
        .await?
        .is_some();
    cancel_pending_acp_permissions_for_session(&state, &id, SESSION_DELETED_PERMISSION_REASON)
        .await;
    Ok(ApiSuccess::new(SessionsDeleteResponse {
        session_id: id,
        deleted,
    }))
}

/// When a session closes or is deleted, any in-flight ACP-source permission
/// rows for that session must be settled — otherwise the operator UI shows
/// stale "pending" rows that won't resolve until the per-request timer fires
/// (default 5 minutes). The ACP-side prompt-turn is already dead; the durable
/// row should reflect that immediately. Cancel settles its own inside the
/// supervisor, where the answer is what lets the agent end its turn.
async fn cancel_pending_acp_permissions_for_session(
    state: &AppState,
    session_id: &str,
    reason: &str,
) {
    if let Err(err) = state
        .permissions
        .cancel_pending_for_session(session_id, reason)
        .await
    {
        tracing::warn!(
            error = %err,
            session_id,
            "failed to cancel pending ACP permissions on session teardown",
        );
    }
}
