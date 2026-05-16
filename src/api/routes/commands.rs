use axum::Json;
use axum::extract::{Path, Query, State};
use serde::{Deserialize, Serialize};

use super::super::core::AppState;
use super::logs::{LogsLimitParams, MAX_LOGS_LIMIT};
use crate::commands::SubmitRequest;
use crate::envelope::ApiSuccess;
use crate::error::StackError;

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
