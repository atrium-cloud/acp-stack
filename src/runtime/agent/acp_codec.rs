//! Free codec helpers for ACP session/config option encoding and inbound
//! request/notification translation.
//!
//! Extracted from `acp_bridge.rs` so the bridge file can focus on the
//! connection lifecycle. These helpers do not need an `AcpBridge` instance;
//! they translate between ACP protocol types and the daemon's own request
//! shapes.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_client_protocol::RequestCancellation;
use agent_client_protocol::schema::v1::{
    Meta, NewSessionResponse, PermissionOptionId, PermissionOptionKind, ReadTextFileRequest,
    ReadTextFileResponse, RequestPermissionOutcome, RequestPermissionRequest,
    SelectedPermissionOutcome, SessionConfigKind, SessionConfigOption, SessionConfigSelectOptions,
    SessionNotification, SessionUpdate, WriteTextFileRequest, WriteTextFileResponse,
};
use tokio::sync::{Mutex as TokioMutex, mpsc};

use crate::error::{Result, StackError};
use crate::runtime::agent::acp_bridge::{AgentSessionConfigCategory, AgentSessionModelSelection};
use crate::runtime::agent::session_sink::SessionEventSink;
use crate::runtime::mediation::permissions::{
    NewPermission, PermissionOutcome, PermissionService, PermissionSource,
};

/// Stable audit reason shared by the durable permission decision and its
/// published cancellation event when ACP `$/cancel_request` wins the race.
const ACP_REQUEST_CANCELLED_REASON: &str = "acp-request-cancelled";
use crate::state::StateStore;

use super::acp_bridge::{NotificationDrain, NotificationGuard};

/// `_meta` namespace for acp-stack's local protocol extensions (also read by
/// `AgentCapabilitiesDto::supports_fork_message_id`).
const ACP_STACK_META_KEY: &str = "acpStack";
const MESSAGE_ID_META_KEY: &str = "messageId";

/// At most one notification may wait behind the worker. A notification owns
/// both parsed ACP content and its raw JSON payload, so a deeper queue could
/// multiply memory use for large file diffs. The producer transfers ownership
/// before waiting, which keeps shutdown cancellation lossless.
const SESSION_NOTIFICATION_BACKLOG: usize = 1;

pub(super) struct QueuedSessionNotification {
    agent_session_id: String,
    update: SessionUpdate,
    payload: String,
    _guard: NotificationGuard,
}

pub(super) type SessionNotificationSender = mpsc::UnboundedSender<QueuedSessionNotification>;

pub(super) fn spawn_session_notification_queue(
    sink: Arc<dyn SessionEventSink>,
) -> SessionNotificationSender {
    let (sender, mut receiver) = mpsc::unbounded_channel::<QueuedSessionNotification>();
    tokio::spawn(async move {
        while let Some(notification) = receiver.recv().await {
            if sink
                .capture_session_update(&notification.agent_session_id, &notification.update)
                .await
            {
                sink.append(
                    &notification.agent_session_id,
                    "session.update",
                    &notification.payload,
                )
                .await;
            }
        }
    });
    sender
}

/// Wire shape of the local prompt message-id extension since ACP v1 dropped
/// the unstable top-level `messageId` fields: the client stamps
/// `_meta.acpStack.messageId` on `session/prompt`, and an agent that recorded
/// it echoes the same shape on the `session/prompt` response.
pub fn prompt_message_id_meta(message_id: &str) -> Meta {
    let mut stack = serde_json::Map::new();
    stack.insert(
        MESSAGE_ID_META_KEY.to_owned(),
        serde_json::Value::String(message_id.to_owned()),
    );
    let mut meta = Meta::new();
    meta.insert(
        ACP_STACK_META_KEY.to_owned(),
        serde_json::Value::Object(stack),
    );
    meta
}

pub fn meta_message_id(meta: Option<&Meta>) -> Option<&str> {
    meta?
        .get(ACP_STACK_META_KEY)?
        .get(MESSAGE_ID_META_KEY)?
        .as_str()
}

pub fn session_config_id_for_value(
    config_options: Option<&[SessionConfigOption]>,
    category: AgentSessionConfigCategory,
    value: &str,
) -> Result<String> {
    let Some(config_options) = config_options else {
        return Err(StackError::AgentConfigProvision {
            path: PathBuf::from("ACP session config options"),
            reason: format!(
                "agent did not advertise a `{}` session config option",
                category.id()
            ),
        });
    };
    for option in config_options {
        // With boolean client capability advertised, an agent may ship a
        // boolean option under a typed-lane category or id (e.g. a boolean
        // "thinking" toggle). The typed lanes only speak select values, so a
        // non-select match must not shadow a later select with the same
        // category.
        if !matches!(option.kind, SessionConfigKind::Select(_)) {
            continue;
        }
        let category_matches = option
            .category
            .as_ref()
            .is_some_and(|option_category| category.matches(option_category));
        let id_matches = category.matches_id(option.id.0.as_ref());
        if (category_matches || id_matches) && session_config_option_contains_value(option, value) {
            return Ok(option.id.0.to_string());
        }
    }
    Err(StackError::AgentConfigProvision {
        path: PathBuf::from("ACP session config options"),
        reason: format!(
            "agent did not advertise `{value}` as an available `{}`",
            category.id()
        ),
    })
}

pub fn session_config_values(
    config_options: Option<&[SessionConfigOption]>,
    category: AgentSessionConfigCategory,
) -> Result<Vec<String>> {
    let Some(config_options) = config_options else {
        return Err(StackError::AgentConfigProvision {
            path: PathBuf::from("ACP session config options"),
            reason: format!(
                "agent did not advertise a `{}` session config option",
                category.id()
            ),
        });
    };
    for option in config_options {
        // Same select-only guard as `session_config_id_for_value`: the typed
        // lanes' value lists are meaningful only for select options.
        if !matches!(option.kind, SessionConfigKind::Select(_)) {
            continue;
        }
        let category_matches = option
            .category
            .as_ref()
            .is_some_and(|option_category| category.matches(option_category));
        let id_matches = category.matches_id(option.id.0.as_ref());
        if category_matches || id_matches {
            let mut values = session_config_option_values(option);
            values.sort();
            values.dedup();
            return Ok(values);
        }
    }
    Err(StackError::AgentConfigProvision {
        path: PathBuf::from("ACP session config options"),
        reason: format!(
            "agent did not advertise a `{}` session config option",
            category.id()
        ),
    })
}

pub fn session_model_selection_for_value(
    response: &NewSessionResponse,
    value: &str,
) -> Result<AgentSessionModelSelection> {
    if let Some(config_options) = response.config_options.as_deref()
        && let Ok(config_id) = session_config_id_for_value(
            Some(config_options),
            AgentSessionConfigCategory::Model,
            value,
        )
    {
        return Ok(AgentSessionModelSelection::ConfigOption { config_id });
    }
    Err(StackError::AgentConfigProvision {
        path: PathBuf::from("ACP session config options"),
        reason: format!("agent did not advertise `{value}` as an available `model`"),
    })
}

pub fn session_model_values(response: &NewSessionResponse) -> Result<Vec<String>> {
    session_config_values(
        response.config_options.as_deref(),
        AgentSessionConfigCategory::Model,
    )
}

pub(crate) fn session_config_option_contains_value(
    option: &SessionConfigOption,
    value: &str,
) -> bool {
    session_config_option_values(option)
        .iter()
        .any(|candidate| candidate == value)
}

fn session_config_option_values(option: &SessionConfigOption) -> Vec<String> {
    match &option.kind {
        SessionConfigKind::Select(select) => match &select.options {
            SessionConfigSelectOptions::Ungrouped(options) => options
                .iter()
                .map(|option| option.value.0.to_string())
                .collect(),
            SessionConfigSelectOptions::Grouped(groups) => groups
                .iter()
                .flat_map(|group| group.options.iter())
                .map(|option| option.value.0.to_string())
                .collect(),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// Forward a `session/request_permission` request through the durable
/// PermissionService, await the decision, and translate the result back to
/// the ACP `RequestPermissionOutcome`. ACP request cancellation atomically
/// settles the durable permission before returning JSON-RPC `-32800`.
pub(crate) async fn resolve_acp_permission(
    service: &PermissionService,
    sink: &Arc<dyn SessionEventSink>,
    request: RequestPermissionRequest,
    cancellation: Option<RequestCancellation>,
) -> std::result::Result<RequestPermissionOutcome, agent_client_protocol::Error> {
    // Serialize the full request for the durable detail record. The schema
    // type is JSON-friendly; failure here only happens for non-JSON-safe
    // values, which the schema does not contain.
    let detail = match serde_json::to_value(&request) {
        Ok(value) => value,
        Err(err) => {
            tracing::warn!(error = %err, "failed to serialize permission request");
            return Err(agent_client_protocol::Error::internal_error());
        }
    };
    let agent_session_id = request.session_id.0.to_string();
    let Some(session_id) = sink.local_session_id(&agent_session_id).await else {
        return Ok(RequestPermissionOutcome::Cancelled);
    };
    let first_option_id = request
        .options
        .first()
        .map(|opt| opt.option_id.0.to_string());

    let (record, mut rx) = match service
        .request(NewPermission {
            source: PermissionSource::Acp,
            requester: Some(format!("session:{session_id}")),
            subject_id: Some(session_id),
            detail,
        })
        .await
    {
        Ok(pair) => pair,
        Err(err) => {
            tracing::warn!(error = %err, "permission service rejected ACP passthrough");
            return Err(agent_client_protocol::Error::internal_error());
        }
    };

    let outcome = tokio::select! {
        outcome = &mut rx => outcome,
        () = wait_for_request_cancellation(cancellation) => {
            match service
                .cancel_if_pending(&record.id, ACP_REQUEST_CANCELLED_REASON)
                .await
            {
                Ok(true) => return Err(agent_client_protocol::Error::request_cancelled()),
                Ok(false) => rx.await,
                Err(error) => {
                    tracing::warn!(error = %error, permission_id = %record.id, "failed to persist ACP permission cancellation");
                    return Err(agent_client_protocol::Error::internal_error());
                }
            }
        }
    };

    match outcome {
        Ok(PermissionOutcome::Approved { option_id, .. }) => {
            let chosen = option_id.or(first_option_id);
            Ok(match chosen {
                Some(id) => RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                    PermissionOptionId::new(id),
                )),
                None => RequestPermissionOutcome::Cancelled,
            })
        }
        Ok(_) => Ok(RequestPermissionOutcome::Cancelled),
        Err(error) => {
            tracing::warn!(error = %error, permission_id = %record.id, "ACP permission waiter closed before a decision");
            Err(agent_client_protocol::Error::internal_error())
        }
    }
}

async fn wait_for_request_cancellation(cancellation: Option<RequestCancellation>) {
    match cancellation {
        Some(cancellation) => cancellation.cancelled().await,
        None => std::future::pending().await,
    }
}

/// Approve an agent permission request without an operator: pick the first
/// `AllowOnce` option, else the first `AllowAlways`. One-shot grants come
/// first so a single testflight prompt never leaves a durable allow behind
/// in harness-side state. Reject-kind options are never selected; a request
/// offering no allow option is answered `Cancelled`.
pub(crate) fn auto_approve_acp_permission(
    request: &RequestPermissionRequest,
) -> RequestPermissionOutcome {
    let allow = request
        .options
        .iter()
        .find(|option| option.kind == PermissionOptionKind::AllowOnce)
        .or_else(|| {
            request
                .options
                .iter()
                .find(|option| option.kind == PermissionOptionKind::AllowAlways)
        });
    match allow {
        Some(option) => {
            tracing::info!(
                option = %option.name,
                "auto-approved agent permission request"
            );
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                option.option_id.clone(),
            ))
        }
        None => RequestPermissionOutcome::Cancelled,
    }
}

/// Byte cap on `fs/read_text_file`. ACP has no size field on the request, so
/// the client bounds what it will load into memory for one call.
const ACP_FS_READ_MAX_BYTES: u64 = 10 * 1024 * 1024;

/// `fs/read_text_file`: workspace-contained disk read with optional 1-based
/// `line` offset and `limit` line count. Headless, there are no editor
/// buffers — the disk is the truth.
pub(crate) async fn handle_read_text_file(
    workspace_root: &Path,
    sink: &Arc<dyn SessionEventSink>,
    request: ReadTextFileRequest,
) -> std::result::Result<ReadTextFileResponse, AcpFsError> {
    let agent_session_id = request.session_id.0.to_string();
    if sink.local_session_id(&agent_session_id).await.is_none() {
        return Err(unknown_session_error(&agent_session_id));
    }
    let path = crate::workspace::resolve_workspace_abs_path(
        workspace_root,
        &request.path,
        crate::workspace::PathIntent::ReadExisting,
    )
    .map_err(acp_fs_error)?;
    let read = crate::workspace::read_file(&path, ACP_FS_READ_MAX_BYTES).map_err(acp_fs_error)?;
    let content = String::from_utf8(read.content).map_err(|_| {
        AcpFsError::invalid_params().data(serde_json::json!({
            "reason": "file is not valid UTF-8 text",
        }))
    })?;
    Ok(ReadTextFileResponse::new(slice_lines(
        &content,
        request.line,
        request.limit,
    )))
}

/// `fs/write_text_file`: workspace-contained atomic write-through plus a
/// durable `fs.write` audit event when state is attached.
pub(crate) async fn handle_write_text_file(
    workspace_root: &Path,
    state: Option<&Arc<TokioMutex<StateStore>>>,
    sink: &Arc<dyn SessionEventSink>,
    request: WriteTextFileRequest,
) -> std::result::Result<WriteTextFileResponse, AcpFsError> {
    let agent_session_id = request.session_id.0.to_string();
    let Some(local_session_id) = sink.local_session_id(&agent_session_id).await else {
        return Err(unknown_session_error(&agent_session_id));
    };
    let path = crate::workspace::resolve_workspace_abs_path(
        workspace_root,
        &request.path,
        crate::workspace::PathIntent::WriteOrCreate,
    )
    .map_err(acp_fs_error)?;
    let metadata = crate::workspace::write_file_atomic(&path, request.content.as_bytes())
        .map_err(acp_fs_error)?;
    if let Some(state) = state {
        let payload = serde_json::json!({
            "session_id": local_session_id,
            "path": path.to_string_lossy(),
            "bytes": metadata.size,
        });
        let store = state.lock().await;
        if let Err(error) = store.append_event_with_source(
            "info",
            "fs.write",
            crate::state::EVENT_SOURCE_ACP,
            "",
            &payload.to_string(),
        ) {
            tracing::warn!(error = %error, "failed to record fs.write audit event");
        }
    }
    Ok(WriteTextFileResponse::new())
}

type AcpFsError = agent_client_protocol::Error;

fn unknown_session_error(agent_session_id: &str) -> AcpFsError {
    AcpFsError::invalid_params().data(serde_json::json!({
        "reason": format!("unknown session `{agent_session_id}`"),
    }))
}

/// Map workspace errors onto the ACP error space: missing file is
/// resource-not-found, containment/validation failures are invalid-params
/// with the reason in `data`, everything else is internal.
fn acp_fs_error(error: StackError) -> AcpFsError {
    match &error {
        StackError::WorkspaceNotFound { .. } => AcpFsError::resource_not_found(None),
        StackError::WorkspacePathInvalid { .. }
        | StackError::WorkspaceSymlinkEscape { .. }
        | StackError::WorkspaceParentNotFound { .. }
        | StackError::WorkspaceTooLarge { .. } => {
            AcpFsError::invalid_params().data(serde_json::json!({
                "reason": error.to_string(),
            }))
        }
        _ => AcpFsError::into_internal_error(error),
    }
}

/// Apply ACP's optional 1-based `line` offset and `limit` line count.
fn slice_lines(content: &str, line: Option<u32>, limit: Option<u32>) -> String {
    if line.is_none() && limit.is_none() {
        return content.to_owned();
    }
    let start = line.map_or(0, |line| line.saturating_sub(1) as usize);
    let selected: Vec<&str> = match limit {
        Some(limit) => content.lines().skip(start).take(limit as usize).collect(),
        None => content.lines().skip(start).collect(),
    };
    selected.join("\n")
}

pub(super) async fn enqueue_session_notification(
    sender: &SessionNotificationSender,
    drain: Arc<NotificationDrain>,
    note: SessionNotification,
) {
    // Serialize the verbatim notification payload so downstream queriers can
    // reconstruct the full ACP update without re-deriving from the typed enum.
    let payload = match serde_json::to_string(&note) {
        Ok(payload) => payload,
        Err(err) => {
            tracing::warn!(error = %err, "failed to serialize session/update; dropping");
            return;
        }
    };
    let agent_session_id = note.session_id.0.to_string();
    let notification = QueuedSessionNotification {
        agent_session_id,
        update: note.update,
        payload,
        _guard: drain.enter(),
    };
    if sender.send(notification).is_err() {
        tracing::warn!("session/update worker stopped; dropping notification");
        return;
    }
    // Ownership has transferred to the drain-owned worker before this await.
    // The ACP callback is sequential, so at most one additional notification
    // can queue while the worker is blocked without risking shutdown loss.
    drain.wait_at_most(SESSION_NOTIFICATION_BACKLOG).await;
}

#[cfg(test)]
mod tests;
