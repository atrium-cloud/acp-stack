use std::sync::atomic::Ordering;

use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use http::StatusCode;
use serde::Serialize;

use super::super::core::AppState;
use super::logs::default_logs_limit;
use crate::config::AgentAdapterConfig;
use crate::envelope::ApiSuccess;
use crate::error::StackError;
use crate::runtime::health::HealthReport;

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
        process_state: snapshot.state.as_wire_str().to_owned(),
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

#[derive(Serialize)]
pub(crate) struct HealthLiveResponse {
    ok: bool,
    server: ServerInfo,
}

/// `GET /v1/health/live` — always 200 once the daemon is accepting requests.
/// Per `docs/specs/api/api.md`, this answers "is the process alive and the
/// router up?" without touching SQLite, the supervisor, or the workspace.
/// Readers that want subsystem detail should call `/v1/health/ready`.
pub(crate) async fn health_live_handler() -> ApiSuccess<HealthLiveResponse> {
    ApiSuccess::new(HealthLiveResponse {
        ok: true,
        server: ServerInfo {
            version: env!("CARGO_PKG_VERSION"),
        },
    })
}

/// `GET /v1/health/ready` — collects a fresh `HealthReport` and returns 200
/// when every subsystem is ok, otherwise 503 with the same body shape so
/// callers can pull the `failing` list and per-subsystem detail from a single
/// schema regardless of status code.
///
/// The envelope's top-level `ok` mirrors `report.ok` (not always `true`) so
/// the 503 case follows the envelope convention from `docs/specs/api/api.md`
/// where successful responses use `ok: true` and failure responses use
/// `ok: false`. Clients that key off envelope.ok see the same yes/no signal
/// as the HTTP status code.
pub(crate) async fn health_ready_handler(State(state): State<AppState>) -> Response {
    let report = HealthReport::collect(&state).await;
    let status = if report.ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    let body = serde_json::json!({
        "ok": report.ok,
        "data": report,
    });
    (status, Json(body)).into_response()
}
