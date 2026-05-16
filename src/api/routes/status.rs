use std::sync::atomic::Ordering;

use axum::extract::State;
use serde::Serialize;

use super::super::core::AppState;
use super::logs::default_logs_limit;
use crate::config::AgentAdapterConfig;
use crate::envelope::ApiSuccess;
use crate::error::StackError;

#[derive(Serialize)]
pub(crate) struct StatusResponse {
    schema_version: i64,
    latest_event: Option<String>,
    server: ServerInfo,
}

#[derive(Serialize)]
struct ServerInfo {
    version: &'static str,
}

pub(crate) async fn status_handler(
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<StatusResponse>, StackError> {
    let store = state.state.lock().await;
    let schema_version = store.schema_version()?;
    let latest_event = store.latest_event_timestamp()?;
    drop(store);
    Ok(ApiSuccess::new(StatusResponse {
        schema_version,
        latest_event,
        server: ServerInfo {
            version: env!("CARGO_PKG_VERSION"),
        },
    }))
}

#[derive(Serialize)]
pub(crate) struct StatusAgentResponse {
    configured: bool,
    agent: AgentStatusJson,
    process_state: String,
    pid: Option<u32>,
    lifecycle_events: Vec<AgentLifecycleJson>,
}

#[derive(Serialize)]
struct AgentStatusJson {
    id: String,
    name: String,
    command: String,
    args: Vec<String>,
    cwd: Option<String>,
    restart: String,
    adapter: Option<AgentAdapterConfig>,
}

#[derive(Serialize)]
struct AgentLifecycleJson {
    id: String,
    created_at: String,
    event_kind: String,
    message: String,
    payload_json: String,
}

pub(crate) async fn status_agent_handler(
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<StatusAgentResponse>, StackError> {
    let store = state.state.lock().await;
    let lifecycle_events = store.query_agent_lifecycle(default_logs_limit())?;
    drop(store);
    let snapshot = state.agent_supervisor.snapshot().await;
    let agent = &state.config.agent;
    Ok(ApiSuccess::new(StatusAgentResponse {
        configured: true,
        agent: AgentStatusJson {
            id: agent.id.clone(),
            name: agent.name.clone(),
            command: agent.command.clone(),
            args: agent.args.clone(),
            cwd: agent.cwd.clone(),
            restart: agent.restart.clone(),
            adapter: agent.adapter.clone(),
        },
        process_state: format!("{:?}", snapshot.state).to_lowercase(),
        pid: snapshot.pid,
        lifecycle_events: lifecycle_events
            .into_iter()
            .map(|event| AgentLifecycleJson {
                id: event.id,
                created_at: event.created_at,
                event_kind: event.event_kind,
                message: event.message,
                payload_json: event.payload_json,
            })
            .collect(),
    }))
}

#[derive(Serialize)]
pub(crate) struct StatusConnectionsResponse {
    active_requests: u64,
}

pub(crate) async fn status_connections_handler(
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<StatusConnectionsResponse>, StackError> {
    Ok(ApiSuccess::new(StatusConnectionsResponse {
        active_requests: state.active_requests.load(Ordering::Relaxed),
    }))
}
