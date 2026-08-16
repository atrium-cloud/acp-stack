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

// The supervisor's create_session softens exactly the
// `AgentConfigProvision` variant from these lookups into an
// ignored-feature record. If a future change makes them construct any
// other variant, that error would become a session-creation hard failure
// again — these tests exist to make that change loud.
#[test]
fn mode_lookup_only_constructs_agent_config_provision() {
    let missing = session_config_id_for_value(None, AgentSessionConfigCategory::Mode, "plan");
    assert!(matches!(
        missing,
        Err(crate::error::StackError::AgentConfigProvision { .. })
    ));
    let unadvertised =
        session_config_id_for_value(Some(&[]), AgentSessionConfigCategory::Mode, "plan");
    assert!(matches!(
        unadvertised,
        Err(crate::error::StackError::AgentConfigProvision { .. })
    ));
}

#[test]
fn model_lookup_only_constructs_agent_config_provision() {
    let response = agent_client_protocol::schema::v1::NewSessionResponse::new("session");
    assert!(matches!(
        session_model_selection_for_value(&response, "some-model"),
        Err(crate::error::StackError::AgentConfigProvision { .. })
    ));
}

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

fn request_with_options(options: Vec<(&str, PermissionOptionKind)>) -> RequestPermissionRequest {
    RequestPermissionRequest::new(
        SessionId::new("sess_auto"),
        ToolCallUpdate::new(ToolCallId::new("tc_auto"), ToolCallUpdateFields::default()),
        options
            .into_iter()
            .map(|(id, kind)| PermissionOption::new(PermissionOptionId::new(id), id, kind))
            .collect::<Vec<_>>(),
    )
}

fn selected_option_id(outcome: RequestPermissionOutcome) -> Option<String> {
    match outcome {
        RequestPermissionOutcome::Selected(SelectedPermissionOutcome { option_id, .. }) => {
            Some(option_id.0.to_string())
        }
        _ => None,
    }
}

#[test]
fn auto_approve_prefers_allow_once_over_allow_always() {
    let request = request_with_options(vec![
        ("deny", PermissionOptionKind::RejectOnce),
        ("allow-always", PermissionOptionKind::AllowAlways),
        ("allow-once", PermissionOptionKind::AllowOnce),
    ]);
    assert_eq!(
        selected_option_id(auto_approve_acp_permission(&request)),
        Some("allow-once".to_owned())
    );
}

#[test]
fn auto_approve_takes_allow_always_when_it_is_the_only_allow() {
    let request = request_with_options(vec![
        ("deny", PermissionOptionKind::RejectAlways),
        ("allow-always", PermissionOptionKind::AllowAlways),
    ]);
    assert_eq!(
        selected_option_id(auto_approve_acp_permission(&request)),
        Some("allow-always".to_owned())
    );
}

#[test]
fn auto_approve_never_selects_reject_options() {
    let request = request_with_options(vec![
        ("deny-once", PermissionOptionKind::RejectOnce),
        ("deny-always", PermissionOptionKind::RejectAlways),
    ]);
    assert_eq!(
        auto_approve_acp_permission(&request),
        RequestPermissionOutcome::Cancelled
    );
}

#[test]
fn auto_approve_cancels_on_empty_options() {
    let request = request_with_options(Vec::new());
    assert_eq!(
        auto_approve_acp_permission(&request),
        RequestPermissionOutcome::Cancelled
    );
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
    let outcome_task = tokio::spawn(async move {
        resolve_acp_permission(&service_for_task, &sink, request, None).await
    });

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

    let outcome = outcome_task
        .await
        .expect("task joins")
        .expect("permission response");
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
    let outcome_task = tokio::spawn(async move {
        resolve_acp_permission(&service_for_task, &sink, request, None).await
    });

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

    let outcome = outcome_task
        .await
        .expect("task joins")
        .expect("permission response");
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

    enqueue_session_notification(&queue, Arc::clone(&drain), tool_call_notification("first")).await;
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
