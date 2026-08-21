use super::*;

#[derive(Deserialize, schemars::JsonSchema)]
pub(crate) struct SessionsStatusParams {
    #[serde(default = "default_session_status_threshold")]
    threshold: String,
    #[serde(default = "default_session_status_window")]
    window: String,
    #[serde(default = "default_session_status_limit")]
    limit: u32,
    #[serde(default, alias = "target")]
    target_id: Option<String>,
}

fn default_session_status_threshold() -> String {
    DEFAULT_SESSION_ACTIVITY_THRESHOLD.to_owned()
}

fn default_session_status_window() -> String {
    DEFAULT_SESSION_STATUS_WINDOW.to_owned()
}

fn default_session_status_limit() -> u32 {
    MAX_LOGS_LIMIT
}

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct SessionsStatusResponse {
    generated_at: String,
    threshold: String,
    window: String,
    window_start: String,
    window_end: String,
    session_count: usize,
    active_count: usize,
    truncated: bool,
    sessions: Vec<SessionStatusSessionResponse>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct SessionStatusSessionResponse {
    id: String,
    /// Derived per request from the durable row plus in-flight prompt and
    /// permission state; a wider set than the durable `status`.
    #[schemars(extend("enum" = ["closed", "available", "permission_required", "idle", "working", "prompt_sent", "done", "stopped", "error", "cancelled"]))]
    state: &'static str,
    /// Durable session status as stored.
    #[schemars(extend("enum" = ["active", "available", "closed"]))]
    status: String,
    agent_id: String,
    cwd: String,
    title: Option<String>,
    last_activity_at: String,
    last_activity_from: String,
    recent: bool,
    prompt: Option<SessionStatusPromptResponse>,
    permission: Option<SessionStatusPermissionResponse>,
    prompt_stream_started_at: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct SessionStatusPromptResponse {
    id: String,
    created_at: String,
    updated_at: String,
    #[schemars(extend("enum" = ["pending", "running", "completed", "errored", "cancelled", "stalled"]))]
    status: String,
    stop_reason: Option<String>,
    error_code: Option<String>,
    error_message: Option<String>,
    message_id: Option<String>,
    message_id_acknowledged: bool,
}

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct SessionStatusPermissionResponse {
    id: String,
    created_at: String,
    updated_at: String,
}

impl SessionStatusSessionResponse {
    fn from_record(
        record: SessionStatusRecord,
        cutoff: chrono::DateTime<Utc>,
    ) -> std::result::Result<Self, StackError> {
        let last_activity = chrono::DateTime::parse_from_rfc3339(&record.last_activity_at)
            .map_err(|err| StackError::InvalidParam {
                field: "last_activity_at",
                reason: format!("stored session activity timestamp is invalid: {err}"),
            })?
            .with_timezone(&Utc);
        let state = derived_session_state(&record);
        let prompt = record
            .latest_prompt
            .map(|prompt| SessionStatusPromptResponse {
                id: prompt.id,
                created_at: prompt.created_at,
                updated_at: prompt.updated_at,
                status: prompt.status,
                stop_reason: prompt.stop_reason,
                error_code: prompt.error_code,
                error_message: prompt.error_message,
                message_id: prompt.message_id,
                message_id_acknowledged: prompt.message_id_acknowledged,
            });
        let permission =
            record
                .pending_permission
                .map(|permission| SessionStatusPermissionResponse {
                    id: permission.id,
                    created_at: permission.created_at,
                    updated_at: permission.updated_at,
                });
        Ok(Self {
            id: record.id,
            state,
            status: record.status,
            agent_id: record.agent_id,
            cwd: record.cwd,
            title: record.title,
            last_activity_at: record.last_activity_at,
            last_activity_from: record.last_activity_from,
            recent: last_activity >= cutoff,
            prompt,
            permission,
            prompt_stream_started_at: record.prompt_stream_started_at,
        })
    }
}

fn derived_session_state(record: &SessionStatusRecord) -> &'static str {
    match record.status.as_str() {
        SESSION_STATUS_CLOSED => return "closed",
        SESSION_STATUS_AVAILABLE => return "available",
        _ => {}
    }
    if record.pending_permission.is_some() {
        return "permission_required";
    }
    let Some(prompt) = record.latest_prompt.as_ref() else {
        return "idle";
    };
    match prompt.status.as_str() {
        "pending" | "running" => {
            if record.prompt_stream_started_at.is_some() {
                "working"
            } else {
                "prompt_sent"
            }
        }
        "completed" if prompt.stop_reason.as_deref() == Some("end_turn") => "done",
        "completed" => "stopped",
        "errored" | "stalled" => "error",
        "cancelled" => "cancelled",
        _ => "idle",
    }
}

pub(crate) async fn sessions_status_handler(
    Query(params): Query<SessionsStatusParams>,
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<SessionsStatusResponse>, StackError> {
    let threshold =
        crate::time_util::parse_duration_suffix(&params.threshold).ok_or_else(|| {
            StackError::InvalidParam {
                field: "threshold",
                reason: format!(
                    "not a valid duration; expected values like `{}` or `30m`",
                    DEFAULT_SESSION_ACTIVITY_THRESHOLD
                ),
            }
        })?;
    let window = parse_session_status_window(&params.window)?;
    let generated_at = Utc::now();
    let cutoff = generated_at - threshold;
    let window_start = generated_at
        .checked_sub_signed(window)
        .ok_or(StackError::InvalidParam {
            field: "window",
            reason: "duration window underflowed the timestamp range".to_owned(),
        })?;
    let window_start_text = window_start.to_rfc3339_opts(SecondsFormat::Nanos, true);
    let limit = params.limit.min(MAX_LOGS_LIMIT);
    let query_limit = limit.saturating_add(1);
    let target = state
        .session_agent_target(params.target_id.as_deref())
        .await?;
    let store = state.state.lock().await;
    let mut rows = store.query_session_status_window(
        &window_start_text,
        Some(&target.target_id),
        query_limit,
    )?;
    drop(store);
    let truncated = rows.len() > limit as usize;
    if truncated {
        rows.truncate(limit as usize);
    }
    let active_count = rows
        .iter()
        .filter(|row| row.status == SESSION_STATUS_ACTIVE)
        .count();
    let sessions = rows
        .into_iter()
        .map(|row| SessionStatusSessionResponse::from_record(row, cutoff))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(ApiSuccess::new(SessionsStatusResponse {
        generated_at: generated_at.to_rfc3339_opts(SecondsFormat::Nanos, true),
        threshold: params.threshold,
        window: params.window,
        window_start: window_start_text,
        window_end: generated_at.to_rfc3339_opts(SecondsFormat::Nanos, true),
        session_count: sessions.len(),
        active_count,
        truncated,
        sessions,
    }))
}

fn parse_session_status_window(raw: &str) -> Result<chrono::Duration> {
    let duration =
        crate::time_util::parse_duration_suffix(raw).ok_or_else(|| StackError::InvalidParam {
            field: "window",
            reason: format!("not a valid duration; expected values between 1m and 999h, got {raw}"),
        })?;
    let seconds = duration.num_seconds();
    if !(MIN_SESSION_STATUS_WINDOW_SECS..=MAX_SESSION_STATUS_WINDOW_SECS).contains(&seconds) {
        return Err(StackError::InvalidParam {
            field: "window",
            reason: format!("duration must be between 1m and 999h inclusive, got {raw}"),
        });
    }
    Ok(duration)
}
