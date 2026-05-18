use axum::extract::State;
use serde::{Deserialize, Serialize};

use super::super::core::AppState;
use crate::envelope::ApiSuccess;
use crate::error::StackError;

#[derive(Serialize)]
pub(crate) struct WsConnectionsResponse {
    connections: Vec<super::super::ws_registry::WsConnectionView>,
}

#[derive(Serialize)]
pub(crate) struct WsSessionsResponse {
    sessions: Vec<super::super::ws_registry::WsSessionView>,
}

#[derive(Deserialize)]
pub(crate) struct DisconnectConnectionsRequest {
    connection_ids: Vec<String>,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct DisconnectSessionsRequest {
    session_ids: Vec<String>,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Serialize)]
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
    let _reason = body.reason.as_deref().unwrap_or("operator-request");
    Ok(ApiSuccess::new(DisconnectResponse {
        requested: state
            .ws_registry
            .disconnect_connections(&body.connection_ids),
    }))
}

pub(crate) async fn ws_disconnect_sessions_handler(
    State(state): State<AppState>,
    axum::Json(body): axum::Json<DisconnectSessionsRequest>,
) -> std::result::Result<ApiSuccess<DisconnectResponse>, StackError> {
    let _reason = body.reason.as_deref().unwrap_or("operator-request");
    Ok(ApiSuccess::new(DisconnectResponse {
        requested: state.ws_registry.disconnect_sessions(&body.session_ids),
    }))
}
