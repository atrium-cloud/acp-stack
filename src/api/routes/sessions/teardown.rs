use super::*;

#[derive(Serialize)]
pub(crate) struct SessionsCancelResponse {
    session_id: String,
}

pub(crate) async fn sessions_cancel_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<SessionsTargetParams>,
) -> std::result::Result<ApiSuccess<SessionsCancelResponse>, StackError> {
    let target = target_for_session_wind_down(&state, &id, params.target_id.as_deref()).await?;
    target.supervisor.cancel_session(&id, &state.state).await?;
    cancel_pending_acp_permissions_for_session(&state, &id, "session-canceled").await;
    Ok(ApiSuccess::new(SessionsCancelResponse { session_id: id }))
}

pub(crate) async fn sessions_close_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<SessionsTargetParams>,
) -> std::result::Result<ApiSuccess<SessionResponse>, StackError> {
    let target = target_for_session_wind_down(&state, &id, params.target_id.as_deref()).await?;
    let record = target.supervisor.close_session(&id, &state.state).await?;
    cancel_pending_acp_permissions_for_session(&state, &id, "session-closed").await;
    Ok(ApiSuccess::new(SessionResponse::from(record)))
}

#[derive(Serialize)]
pub(crate) struct SessionsDeleteResponse {
    session_id: String,
    deleted: bool,
}

pub(crate) async fn sessions_delete_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<SessionsTargetParams>,
) -> std::result::Result<ApiSuccess<SessionsDeleteResponse>, StackError> {
    // Unknown ids are a silent success per ACP session/delete, so the stored
    // target comes from a single lookup here instead of the shared resolvers
    // (which error on missing sessions and would turn a concurrent delete
    // racing this handler into a 404).
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
    cancel_pending_acp_permissions_for_session(&state, &id, "session-deleted").await;
    Ok(ApiSuccess::new(SessionsDeleteResponse {
        session_id: id,
        deleted,
    }))
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
