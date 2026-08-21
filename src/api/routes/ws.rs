use axum::extract::State;
use serde::{Deserialize, Serialize};

use super::super::core::AppState;
use crate::envelope::ApiSuccess;
use crate::error::StackError;

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct WsConnectionsResponse {
    connections: Vec<super::super::ws_registry::WsConnectionView>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct WsSessionsResponse {
    sessions: Vec<super::super::ws_registry::WsSessionView>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub(crate) struct DisconnectConnectionsRequest {
    connection_ids: Vec<String>,
    /// Recorded as `operator_reason` on each `ws.client_disconnected` event.
    /// Omitted here means the event carries no `operator_reason` field.
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub(crate) struct DisconnectSessionsRequest {
    session_ids: Vec<String>,
    /// Recorded as `operator_reason` on each `ws.client_disconnected` event.
    /// Omitted here means the event carries no `operator_reason` field.
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct DisconnectResponse {
    requested: usize,
}

pub(crate) async fn ws_connections_handler(
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<WsConnectionsResponse>, StackError> {
    Ok(ApiSuccess::new(WsConnectionsResponse {
        connections: state.ws_registry.list_connections(),
    }))
}

pub(crate) async fn ws_sessions_handler(
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<WsSessionsResponse>, StackError> {
    Ok(ApiSuccess::new(WsSessionsResponse {
        sessions: state.ws_registry.list_sessions(),
    }))
}

pub(crate) async fn ws_disconnect_connections_handler(
    State(state): State<AppState>,
    axum::Json(body): axum::Json<DisconnectConnectionsRequest>,
) -> std::result::Result<ApiSuccess<DisconnectResponse>, StackError> {
    Ok(ApiSuccess::new(DisconnectResponse {
        requested: state
            .ws_registry
            .disconnect_connections(&body.connection_ids, body.reason.as_deref()),
    }))
}

pub(crate) async fn ws_disconnect_sessions_handler(
    State(state): State<AppState>,
    axum::Json(body): axum::Json<DisconnectSessionsRequest>,
) -> std::result::Result<ApiSuccess<DisconnectResponse>, StackError> {
    Ok(ApiSuccess::new(DisconnectResponse {
        requested: state
            .ws_registry
            .disconnect_sessions(&body.session_ids, body.reason.as_deref()),
    }))
}
