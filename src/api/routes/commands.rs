use axum::Json;
use axum::extract::{Path, Query, State};
use serde::{Deserialize, Serialize};

use super::super::core::AppState;
use super::logs::{LogsLimitParams, MAX_LOGS_LIMIT, default_logs_limit, parse_order};
use crate::envelope::ApiSuccess;
use crate::error::StackError;
use crate::runtime::mediation::commands::SubmitRequest;

#[derive(Debug, Deserialize)]
pub(crate) struct CommandSubmitRequest {
    command: String,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    env: Option<std::collections::HashMap<String, String>>,
    #[serde(default, rename = "timeout")]
    timeout_override: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct CommandResponse {
    id: String,
    created_at: String,
    updated_at: String,
    status: String,
    command: String,
    exit_status: Option<i64>,
    started_at: Option<String>,
    finished_at: Option<String>,
    cwd: Option<String>,
    duration_ms: Option<i64>,
    truncated: bool,
    last_output_event_id: Option<String>,
    last_output_at: Option<String>,
    last_output_seq: Option<i64>,
    output_bytes: i64,
    last_progress_at: Option<String>,
}

impl From<crate::state::CommandRecord> for CommandResponse {
    fn from(record: crate::state::CommandRecord) -> Self {
        Self {
            id: record.id,
            created_at: record.created_at,
            updated_at: record.updated_at,
            status: record.status,
            command: record.command,
            exit_status: record.exit_status,
            started_at: record.started_at,
            finished_at: record.finished_at,
            cwd: record.cwd,
            duration_ms: record.duration_ms,
            truncated: record.truncated,
            last_output_event_id: record.last_output_event_id,
            last_output_at: record.last_output_at,
            last_output_seq: record.last_output_seq,
            output_bytes: record.output_bytes,
            last_progress_at: record.last_progress_at,
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct CommandsListResponse {
    items: Vec<CommandResponse>,
}

pub(crate) async fn commands_submit_handler(
    State(state): State<AppState>,
    Json(body): Json<CommandSubmitRequest>,
) -> std::result::Result<ApiSuccess<CommandResponse>, StackError> {
    let request = SubmitRequest {
        command: body.command,
        cwd: body.cwd,
        env: body.env,
        timeout_override: body.timeout_override,
    };
    let record = state.commands.submit(request).await?;
    Ok(ApiSuccess::new(record.into()))
}

pub(crate) async fn commands_get_handler(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<CommandResponse>, StackError> {
    let record = state.commands.get(&id).await?;
    Ok(ApiSuccess::new(record.into()))
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub(crate) struct CommandOutputParams {
    #[serde(default = "default_logs_limit")]
    limit: u32,
    after: Option<String>,
    order: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct CommandOutputResponse {
    chunks: Vec<CommandOutputFrame>,
    next_cursor: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct CommandOutputFrame {
    event_id: String,
    created_at: String,
    command_id: String,
    stream: String,
    seq: i64,
    data: String,
}

impl CommandOutputFrame {
    fn from_event(event: crate::state::Event) -> std::result::Result<Self, StackError> {
        let payload: serde_json::Value = serde_json::from_str(&event.payload_json)
            .map_err(|_| StackError::InvalidEventPayload)?;
        let command_id = payload
            .get("command_id")
            .and_then(|value| value.as_str())
            .ok_or(StackError::InvalidEventPayload)?;
        let stream = payload
            .get("stream")
            .and_then(|value| value.as_str())
            .ok_or(StackError::InvalidEventPayload)?;
        let seq = payload
            .get("seq")
            .and_then(|value| value.as_i64())
            .ok_or(StackError::InvalidEventPayload)?;
        let data = payload
            .get("data")
            .and_then(|value| value.as_str())
            .ok_or(StackError::InvalidEventPayload)?;
        Ok(Self {
            event_id: event.id,
            created_at: event.created_at,
            command_id: command_id.to_owned(),
            stream: stream.to_owned(),
            seq,
            data: data.to_owned(),
        })
    }
}

pub(crate) async fn commands_output_handler(
    Path(id): Path<String>,
    Query(params): Query<CommandOutputParams>,
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<CommandOutputResponse>, StackError> {
    let limit = params.limit.min(MAX_LOGS_LIMIT);
    let order = parse_order(params.order.as_deref())?;
    let store = state.state.lock().await;
    if store.get_command(&id)?.is_none() {
        return Err(StackError::CommandNotFound { id });
    }
    let events = store.query_command_output_events(&id, limit, params.after.as_deref(), order)?;
    drop(store);
    let next_cursor = if events.len() == limit as usize {
        events.last().map(|event| event.id.clone())
    } else {
        None
    };
    let chunks = events
        .into_iter()
        .map(CommandOutputFrame::from_event)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(ApiSuccess::new(CommandOutputResponse {
        chunks,
        next_cursor,
    }))
}

pub(crate) async fn commands_list_handler(
    Query(params): Query<LogsLimitParams>,
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<CommandsListResponse>, StackError> {
    let limit = params.limit.min(MAX_LOGS_LIMIT);
    let records = state.commands.list(limit).await?;
    Ok(ApiSuccess::new(CommandsListResponse {
        items: records.into_iter().map(CommandResponse::from).collect(),
    }))
}

pub(crate) async fn commands_cancel_handler(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<CommandResponse>, StackError> {
    let record = state.commands.cancel(&id).await?;
    Ok(ApiSuccess::new(record.into()))
}
