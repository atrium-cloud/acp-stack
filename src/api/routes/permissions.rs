use axum::Json;
use axum::extract::{Path, Query, State};
use serde::{Deserialize, Serialize};

use super::super::core::AppState;
use super::logs::{LogsLimitParams, MAX_LOGS_LIMIT};
use crate::envelope::ApiSuccess;
use crate::error::StackError;

#[derive(Serialize)]
pub(crate) struct PermissionsListResponse {
    permissions: Vec<crate::runtime::mediation::permissions::PermissionRequestView>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub(crate) struct PermissionApproveBody {
    option_id: Option<String>,
    reason: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub(crate) struct PermissionDenyBody {
    reason: Option<String>,
}

pub(crate) async fn permissions_pending_handler(
    Query(params): Query<LogsLimitParams>,
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<PermissionsListResponse>, StackError> {
    let limit = params.limit.min(MAX_LOGS_LIMIT);
    let permissions = state.permissions.pending(limit).await?;
    Ok(ApiSuccess::new(PermissionsListResponse { permissions }))
}

pub(crate) async fn permissions_get_handler(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> std::result::Result<
    ApiSuccess<crate::runtime::mediation::permissions::PermissionRequestView>,
    StackError,
> {
    let view = state.permissions.get(&id).await?;
    Ok(ApiSuccess::new(view))
}

pub(crate) async fn permissions_approve_handler(
    Path(id): Path<String>,
    State(state): State<AppState>,
    body: Option<Json<PermissionApproveBody>>,
) -> std::result::Result<
    ApiSuccess<crate::runtime::mediation::permissions::PermissionDecisionView>,
    StackError,
> {
    let Json(body) = body.unwrap_or_default();
    // The deciding principal is the bearer-token tier. These routes are
    // session-tier (per docs/specs/security.md:20); the principal is always
    // "session-key" and that's what's recorded in `permission_decisions`. If
    // the tier policy ever splits approve vs deny across keys, surface the
    // resolved KeyKind from the request extension here.
    let decision = state
        .permissions
        .approve(&id, body.option_id, body.reason, "session-key")
        .await?;
    Ok(ApiSuccess::new(decision))
}

pub(crate) async fn permissions_deny_handler(
    Path(id): Path<String>,
    State(state): State<AppState>,
    body: Option<Json<PermissionDenyBody>>,
) -> std::result::Result<
    ApiSuccess<crate::runtime::mediation::permissions::PermissionDecisionView>,
    StackError,
> {
    let Json(body) = body.unwrap_or_default();
    // Hardcoded "session-key" mirrors `permissions_approve_handler`; see the
    // rationale comment there.
    let decision = state
        .permissions
        .deny(&id, body.reason, "session-key")
        .await?;
    Ok(ApiSuccess::new(decision))
}
