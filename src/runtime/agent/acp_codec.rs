//! Free codec helpers for ACP session/config option encoding and inbound
//! request/notification translation.
//!
//! Extracted from `acp_bridge.rs` so the bridge file can focus on the
//! connection lifecycle. These helpers do not need an `AcpBridge` instance;
//! they translate between ACP protocol types and the daemon's own request
//! shapes.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_client_protocol::schema::v1::{
    Meta, NewSessionResponse, PermissionOptionId, ReadTextFileRequest, ReadTextFileResponse,
    RequestPermissionOutcome, RequestPermissionRequest, SelectedPermissionOutcome,
    SessionConfigKind, SessionConfigOption, SessionConfigSelectOptions, SessionNotification,
    SessionUpdate, WriteTextFileRequest, WriteTextFileResponse,
};
use tokio::sync::{Mutex as TokioMutex, mpsc};

use crate::error::{Result, StackError};
use crate::runtime::agent::acp_bridge::{AgentSessionConfigCategory, AgentSessionModelSelection};
use crate::runtime::agent::session_sink::SessionEventSink;
use crate::runtime::mediation::permissions::{
    NewPermission, PermissionOutcome, PermissionService, PermissionSource,
};
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
        let category_matches = option
            .category
            .as_ref()
            .is_some_and(|option_category| category.matches(option_category));
        let id_matches = option.id.0.as_ref() == category.id();
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
        let category_matches = option
            .category
            .as_ref()
            .is_some_and(|option_category| category.matches(option_category));
        let id_matches = option.id.0.as_ref() == category.id();
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

fn session_config_option_contains_value(option: &SessionConfigOption, value: &str) -> bool {
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
/// the ACP `RequestPermissionOutcome`. Falls back to `Cancelled` on every
/// failure so the agent's prompt turn always settles deterministically.
pub(crate) async fn resolve_acp_permission(
    service: &PermissionService,
    sink: &Arc<dyn SessionEventSink>,
    request: RequestPermissionRequest,
) -> RequestPermissionOutcome {
    // Serialize the full request for the durable detail record. The schema
    // type is JSON-friendly; failure here only happens for non-JSON-safe
    // values, which the schema does not contain.
    let detail = match serde_json::to_value(&request) {
        Ok(value) => value,
        Err(err) => {
            tracing::warn!(error = %err, "failed to serialize permission request");
            return RequestPermissionOutcome::Cancelled;
        }
    };
    let agent_session_id = request.session_id.0.to_string();
    let Some(session_id) = sink.local_session_id(&agent_session_id).await else {
        return RequestPermissionOutcome::Cancelled;
    };
    let first_option_id = request
        .options
        .first()
        .map(|opt| opt.option_id.0.to_string());

    let (_record, rx) = match service
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
            return RequestPermissionOutcome::Cancelled;
        }
    };

    match rx.await {
        Ok(PermissionOutcome::Approved { option_id, .. }) => {
            let chosen = option_id.or(first_option_id);
            match chosen {
                Some(id) => RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                    PermissionOptionId::new(id),
                )),
                None => RequestPermissionOutcome::Cancelled,
            }
        }
        _ => RequestPermissionOutcome::Cancelled,
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
mod tests {
    use super::*;
    use crate::config::PermissionTimeoutAction;
    use crate::events::EventHub;
    use crate::runtime::agent::session_changes::SessionChangesHandle;
    use crate::runtime::mediation::permissions::PermissionService;
    use crate::state::StateStore;
    use agent_client_protocol::JsonRpcMessage;
    use agent_client_protocol::schema::v1::{
        AgentNotification, PermissionOption, PermissionOptionId, PermissionOptionKind,
        RequestPermissionRequest, SessionId, SessionUpdate, ToolCallId, ToolCallUpdate,
        ToolCallUpdateFields,
    };
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;
    use tokio::sync::Mutex as TokioMutex;

    #[derive(Default)]
    struct RecordingSink {
        events: Mutex<Vec<(String, String, String)>>,
        changes: SessionChangesHandle,
    }

    impl SessionEventSink for RecordingSink {
        fn capture_session_update<'a>(
            &'a self,
            agent_session_id: &'a str,
            update: &'a SessionUpdate,
        ) -> futures::future::BoxFuture<'a, bool> {
            Box::pin(async move {
                self.changes.apply(agent_session_id, update).await;
                true
            })
        }

        fn append<'a>(
            &'a self,
            session_id: &'a str,
            kind: &'a str,
            payload_json: &'a str,
        ) -> futures::future::BoxFuture<'a, ()> {
            Box::pin(async move {
                self.events.lock().expect("sink lock").push((
                    session_id.to_owned(),
                    kind.to_owned(),
                    payload_json.to_owned(),
                ));
            })
        }
    }

    #[derive(Default)]
    struct BlockingNotificationSink {
        operations: Mutex<Vec<String>>,
        first_capture_started: tokio::sync::Notify,
        release_first_capture: tokio::sync::Notify,
        first_capture_seen: AtomicBool,
    }

    impl SessionEventSink for BlockingNotificationSink {
        fn capture_session_update<'a>(
            &'a self,
            _agent_session_id: &'a str,
            update: &'a SessionUpdate,
        ) -> futures::future::BoxFuture<'a, bool> {
            Box::pin(async move {
                let SessionUpdate::ToolCall(tool_call) = update else {
                    panic!("test notification must contain a tool call");
                };
                self.operations
                    .lock()
                    .expect("operations lock")
                    .push(format!("capture:{}", tool_call.tool_call_id.0));
                if !self.first_capture_seen.swap(true, Ordering::SeqCst) {
                    self.first_capture_started.notify_one();
                    self.release_first_capture.notified().await;
                }
                true
            })
        }

        fn append<'a>(
            &'a self,
            _session_id: &'a str,
            _kind: &'a str,
            payload_json: &'a str,
        ) -> futures::future::BoxFuture<'a, ()> {
            Box::pin(async move {
                let payload: serde_json::Value =
                    serde_json::from_str(payload_json).expect("notification payload JSON");
                let tool_call_id = payload["update"]["toolCallId"]
                    .as_str()
                    .expect("tool call id");
                self.operations
                    .lock()
                    .expect("operations lock")
                    .push(format!("append:{tool_call_id}"));
            })
        }
    }

    fn tool_call_notification(tool_call_id: &str) -> SessionNotification {
        let params = serde_json::json!({
            "sessionId": "sess_queue",
            "update": {
                "sessionUpdate": "tool_call",
                "toolCallId": tool_call_id,
                "title": format!("Edit {tool_call_id}"),
                "kind": "edit",
                "status": "in_progress",
                "content": []
            }
        });
        let notification = AgentNotification::parse_message("session/update", &params)
            .expect("tool call notification should deserialize");
        let AgentNotification::SessionNotification(note) = notification else {
            panic!("tool call should be a session notification");
        };
        note
    }

    fn fake_request(session_id: &str) -> RequestPermissionRequest {
        RequestPermissionRequest::new(
            SessionId::new(session_id),
            ToolCallUpdate::new(ToolCallId::new("tc_1"), ToolCallUpdateFields::default()),
            vec![PermissionOption::new(
                PermissionOptionId::new("allow"),
                "Allow",
                PermissionOptionKind::AllowOnce,
            )],
        )
    }

    async fn fresh_service() -> (tempfile::TempDir, PermissionService) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.sqlite");
        let store = StateStore::open(&path).expect("open");
        store.migrate().expect("migrate");
        let state = Arc::new(TokioMutex::new(store));
        let events = EventHub::new();
        (
            dir,
            PermissionService::new(
                state,
                events,
                Duration::from_secs(60),
                PermissionTimeoutAction::Deny,
            ),
        )
    }

    #[tokio::test]
    async fn approve_passthrough_returns_selected_option() {
        let (_dir, service) = fresh_service().await;
        let request = fake_request("sess_test");
        let service_for_task = service.clone();
        let sink: Arc<dyn SessionEventSink> = Arc::new(RecordingSink::default());
        let outcome_task =
            tokio::spawn(
                async move { resolve_acp_permission(&service_for_task, &sink, request).await },
            );

        // Drain the new permission row + approve it.
        let mut id = None;
        for _ in 0..50 {
            let pending = service.pending(10).await.expect("pending");
            if let Some(first) = pending.first() {
                id = Some(first.id.clone());
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let perm_id = id.expect("permission row must appear");
        service
            .approve(&perm_id, Some("allow".to_owned()), None, "session-key")
            .await
            .expect("approve");

        let outcome = outcome_task.await.expect("task joins");
        match outcome {
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome { option_id, .. }) => {
                assert_eq!(option_id.0.as_ref(), "allow");
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    #[tokio::test]
    async fn deny_passthrough_returns_cancelled() {
        let (_dir, service) = fresh_service().await;
        let request = fake_request("sess_test");
        let service_for_task = service.clone();
        let sink: Arc<dyn SessionEventSink> = Arc::new(RecordingSink::default());
        let outcome_task =
            tokio::spawn(
                async move { resolve_acp_permission(&service_for_task, &sink, request).await },
            );

        let mut id = None;
        for _ in 0..50 {
            let pending = service.pending(10).await.expect("pending");
            if let Some(first) = pending.first() {
                id = Some(first.id.clone());
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let perm_id = id.expect("permission row must appear");
        service
            .deny(&perm_id, None, "session-key")
            .await
            .expect("deny");

        let outcome = outcome_task.await.expect("task joins");
        assert!(matches!(outcome, RequestPermissionOutcome::Cancelled));
    }

    #[test]
    fn session_config_helpers_validate_select_values_by_category() {
        let options: Vec<SessionConfigOption> = serde_json::from_str(
            r#"[
                {
                    "id": "agent-model",
                    "name": "Model",
                    "category": "model",
                    "type": "select",
                    "currentValue": "openai/gpt-5.5",
                    "options": [
                        {"value": "openai/gpt-5.5", "name": "GPT-5.5"},
                        {"value": "anthropic/claude-sonnet-4-5", "name": "Claude Sonnet 4.5"}
                    ]
                },
                {
                    "id": "mode",
                    "name": "Mode",
                    "category": "mode",
                    "type": "select",
                    "currentValue": "smart",
                    "options": [
                        {"value": "smart", "name": "Smart"},
                        {"value": "fast", "name": "Fast"}
                    ]
                }
            ]"#,
        )
        .expect("session config options deserialize");

        let model_id = session_config_id_for_value(
            Some(&options),
            AgentSessionConfigCategory::Model,
            "openai/gpt-5.5",
        )
        .expect("model value should be accepted");
        assert_eq!(model_id, "agent-model");
        assert_eq!(
            session_config_values(Some(&options), AgentSessionConfigCategory::Mode)
                .expect("mode values"),
            ["fast", "smart"]
        );
        let err = session_config_id_for_value(
            Some(&options),
            AgentSessionConfigCategory::Model,
            "openai/not-advertised",
        )
        .expect_err("unknown model should be rejected");
        assert!(err.to_string().contains("openai/not-advertised"));
    }

    #[test]
    fn session_model_helpers_reject_removed_legacy_model_state() {
        // ACP v1 dropped the pre-1.0 `models` session state; an agent that
        // only advertises the legacy shape gets a clear provisioning error
        // instead of silent acceptance.
        let response: NewSessionResponse = serde_json::from_str(
            r#"{
                "sessionId": "sess_legacy",
                "models": {
                    "currentModelId": "opencode-go/deepseek-v4-flash",
                    "availableModels": [
                        {
                            "modelId": "opencode-go/deepseek-v4-flash",
                            "name": "DeepSeek V4 Flash"
                        }
                    ]
                }
            }"#,
        )
        .expect("unknown fields are ignored on deserialize");

        let err = session_model_values(&response).expect_err("legacy models must be rejected");
        assert!(err.to_string().contains("model"));
        let err = session_model_selection_for_value(&response, "opencode-go/deepseek-v4-flash")
            .expect_err("legacy model selection must be rejected");
        assert!(err.to_string().contains("opencode-go/deepseek-v4-flash"));
    }

    #[test]
    fn prompt_message_id_meta_round_trips() {
        let meta = prompt_message_id_meta("msg_test_1");
        assert_eq!(meta_message_id(Some(&meta)), Some("msg_test_1"));
        assert_eq!(meta_message_id(None), None);
        assert_eq!(meta_message_id(Some(&Meta::new())), None);
    }

    #[tokio::test]
    async fn usage_update_notifications_deserialize_and_enqueue() {
        let params = serde_json::json!({
            "sessionId": "sess_usage",
            "update": {
                "sessionUpdate": "usage_update",
                "used": 128,
                "size": 4096,
                "cost": {
                    "amount": 0.25,
                    "currency": "USD"
                }
            }
        });
        let notification = AgentNotification::parse_message("session/update", &params)
            .expect("usage_update notification should deserialize");
        let AgentNotification::SessionNotification(note) = notification else {
            panic!("usage_update should be a session notification");
        };
        let sink = Arc::new(RecordingSink::default());
        let sink_dyn: Arc<dyn SessionEventSink> = sink.clone();
        let drain = Arc::new(NotificationDrain::default());
        let queue = spawn_session_notification_queue(sink_dyn);
        enqueue_session_notification(&queue, Arc::clone(&drain), note).await;
        drain.wait_idle().await;

        let events = sink.events.lock().expect("sink events");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "sess_usage");
        assert_eq!(events[0].1, "session.update");
        assert!(events[0].2.contains(r#""sessionUpdate":"usage_update""#));
    }

    #[tokio::test]
    async fn diff_notification_updates_transient_snapshot_and_preserves_raw_event() {
        let params = serde_json::json!({
            "sessionId": "sess_diff",
            "update": {
                "sessionUpdate": "tool_call",
                "toolCallId": "call_1",
                "title": "Edit secret file",
                "kind": "edit",
                "status": "completed",
                "content": [{
                    "type": "diff",
                    "path": "/workspace/.env",
                    "oldText": "TOKEN=old",
                    "newText": "TOKEN=new",
                    "_meta": {"source": "agent"}
                }]
            }
        });
        let notification = AgentNotification::parse_message("session/update", &params)
            .expect("diff notification should deserialize");
        let AgentNotification::SessionNotification(note) = notification else {
            panic!("diff should be a session notification");
        };
        let sink = Arc::new(RecordingSink::default());
        let sink_dyn: Arc<dyn SessionEventSink> = sink.clone();
        let drain = Arc::new(NotificationDrain::default());
        let queue = spawn_session_notification_queue(sink_dyn);
        enqueue_session_notification(&queue, Arc::clone(&drain), note).await;
        drain.wait_idle().await;

        let snapshot =
            serde_json::to_value(sink.changes.snapshot("sess_diff").await).expect("snapshot JSON");
        assert_eq!(
            snapshot["tool_calls"][0]["content"][0]["path"],
            "/workspace/.env"
        );
        assert_eq!(
            snapshot["tool_calls"][0]["content"][0]["newText"],
            "TOKEN=new"
        );
        let events = sink.events.lock().expect("sink events");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "sess_diff");
        assert!(events[0].2.contains(r#""oldText":"TOKEN=old""#));
        assert!(events[0].2.contains(r#""newText":"TOKEN=new""#));
    }

    #[tokio::test]
    async fn queued_notification_survives_backpressured_producer_cancellation_in_fifo_order() {
        let sink = Arc::new(BlockingNotificationSink::default());
        let sink_dyn: Arc<dyn SessionEventSink> = sink.clone();
        let drain = Arc::new(NotificationDrain::default());
        let queue = spawn_session_notification_queue(sink_dyn);

        enqueue_session_notification(&queue, Arc::clone(&drain), tool_call_notification("first"))
            .await;
        sink.first_capture_started.notified().await;

        let second_queue = queue.clone();
        let second_drain = Arc::clone(&drain);
        let mut second_enqueue = tokio::spawn(async move {
            enqueue_session_notification(
                &second_queue,
                second_drain,
                tool_call_notification("second"),
            )
            .await;
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut second_enqueue)
                .await
                .is_err(),
            "second producer should backpressure after transferring queue ownership"
        );
        second_enqueue.abort();
        let _ = second_enqueue.await;

        sink.release_first_capture.notify_one();
        drain.wait_idle().await;

        assert_eq!(
            *sink.operations.lock().expect("operations lock"),
            [
                "capture:first",
                "append:first",
                "capture:second",
                "append:second"
            ]
        );
    }
}
