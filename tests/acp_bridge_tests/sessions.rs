use std::sync::Arc;

use acp_stack::runtime::agent::acp_bridge::{AcpBridge, AcpPermissionPolicy, SessionEventSink};

use crate::support::{InMemorySink, fake_agent_config, fake_env, null_sink};

#[tokio::test]
async fn new_session_round_trips_and_prompt_emits_notifications() {
    use agent_client_protocol::schema::v1::{ContentBlock, PromptRequest, TextContent};

    let sink = Arc::new(InMemorySink::default());
    let sink_dyn: Arc<dyn SessionEventSink> = sink.clone();
    let bridge = AcpBridge::spawn(
        &fake_agent_config(),
        fake_env(),
        std::env::temp_dir(),
        sink_dyn,
        AcpPermissionPolicy::Cancel,
        &Default::default(),
        None,
        None,
    )
    .await
    .expect("spawn");
    assert!(bridge.capabilities().supports_load_session());

    let new_session = bridge
        .new_session(std::env::temp_dir(), vec![])
        .await
        .expect("session/new");
    let session_id = new_session.session_id.clone();

    let prompt = PromptRequest::new(
        session_id.clone(),
        vec![ContentBlock::Text(TextContent::new("hello"))],
    );
    let stop = bridge
        .prompt_session(prompt)
        .await
        .expect("session/prompt")
        .stop_reason;
    assert!(matches!(
        stop,
        agent_client_protocol::schema::v1::StopReason::EndTurn
    ));

    // Notifications go through a tokio::spawn inside the sink, so let the
    // runtime drain microtasks before reading.
    tokio::task::yield_now().await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let events = sink.events.lock().expect("sink").len();
    assert!(
        events >= 2,
        "expected at least 2 session/update events, saw {events}"
    );
    let recorded_session = sink.events.lock().unwrap()[0].session_id.clone();
    assert_eq!(recorded_session, session_id.0.to_string());
    let kind = sink.events.lock().unwrap()[0].kind.clone();
    assert_eq!(kind, "session.update");
    let payload = sink.events.lock().unwrap()[0].payload.clone();
    assert!(payload.contains("chunk-1"));

    bridge.shutdown().await.expect("shutdown ok");
}

#[tokio::test]
async fn prompt_rejects_unadvertised_image_content() {
    use agent_client_protocol::schema::v1::{ContentBlock, ImageContent, PromptRequest, SessionId};

    let bridge = AcpBridge::spawn(
        &fake_agent_config(),
        fake_env(),
        std::env::temp_dir(),
        null_sink(),
        AcpPermissionPolicy::Cancel,
        &Default::default(),
        None,
        None,
    )
    .await
    .expect("spawn");
    let error = bridge
        .prompt_session(PromptRequest::new(
            SessionId::new("sess_prompt_capability"),
            vec![ContentBlock::Image(ImageContent::new(
                "aW1hZ2U=",
                "image/png",
            ))],
        ))
        .await
        .expect_err("image content requires an advertised capability");
    assert!(matches!(
        error,
        acp_stack::error::StackError::AgentUnsupportedCapability {
            name: "promptCapabilities.image"
        }
    ));
    bridge.shutdown().await.expect("shutdown ok");
}

#[tokio::test]
async fn cancelled_permission_does_not_block_dispatch_and_is_persisted() {
    use std::time::Duration;

    use acp_stack::config::PermissionTimeoutAction;
    use acp_stack::events::EventHub;
    use acp_stack::runtime::mediation::permissions::PermissionService;
    use acp_stack::state::StateStore;
    use agent_client_protocol::schema::v1::{ContentBlock, PromptRequest, SessionId, TextContent};
    use tokio::sync::Mutex as TokioMutex;

    let state_dir = tempfile::tempdir().expect("state tempdir");
    let store = StateStore::open(state_dir.path().join("state.sqlite")).expect("open state");
    store.migrate().expect("migrate state");
    let state = Arc::new(TokioMutex::new(store));
    let events = EventHub::new();
    let mut event_rx = events.subscribe();
    let permissions = PermissionService::new(
        state,
        events,
        Duration::from_secs(60),
        PermissionTimeoutAction::Deny,
    );
    let mut config = fake_agent_config();
    config.args.push("--request-permission-then-cancel".into());
    let bridge = AcpBridge::spawn(
        &config,
        fake_env(),
        std::env::temp_dir(),
        null_sink(),
        AcpPermissionPolicy::Service(permissions),
        &Default::default(),
        None,
        None,
    )
    .await
    .expect("spawn");

    tokio::time::timeout(
        Duration::from_secs(5),
        bridge.prompt_session(PromptRequest::new(
            SessionId::new("sess_permission_cancel"),
            vec![ContentBlock::Text(TextContent::new("cancel permission"))],
        )),
    )
    .await
    .expect("permission did not block prompt dispatch")
    .expect("prompt response");

    let canceled = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = event_rx.recv().await.expect("permission event");
            if event.topic == "permissions" && event.payload["kind"] == "permission.canceled" {
                break event;
            }
        }
    })
    .await
    .expect("durable permission cancellation event");
    assert_eq!(canceled.payload["data"]["reason"], "acp-request-cancelled");
    bridge.shutdown().await.expect("shutdown ok");
}

#[tokio::test]
async fn new_session_returns_custom_model_config_option_id() {
    let mut config = fake_agent_config();
    config.id = "placebo".into();
    config.args.extend([
        "--model-config-option".into(),
        "deepseek/deepseek-v4-flash".into(),
        "--model-config-option-id".into(),
        "agent-model".into(),
    ]);
    let bridge = AcpBridge::spawn(
        &config,
        fake_env(),
        std::env::temp_dir(),
        null_sink(),
        AcpPermissionPolicy::Cancel,
        &Default::default(),
        None,
        None,
    )
    .await
    .expect("spawn");

    let new_session = bridge
        .new_session(std::env::temp_dir(), vec![])
        .await
        .expect("session/new");
    let options = new_session.config_options.as_ref().expect("config options");
    assert_eq!(options[0].id.0.as_ref(), "agent-model");
    bridge.shutdown().await.expect("shutdown ok");
}

// A spec-strict agent only returns config options when the client advertised
// `session.configOptions` at initialize, so this round-trip proves the bridge
// actually advertises the capability on the wire.
#[tokio::test]
async fn new_session_advertises_config_options_to_strict_agent() {
    let mut config = fake_agent_config();
    config.id = "placebo".into();
    config.args.extend([
        "--model-config-option".into(),
        "deepseek/deepseek-v4-flash".into(),
        "--require-client-config-options".into(),
    ]);
    let bridge = AcpBridge::spawn(
        &config,
        fake_env(),
        std::env::temp_dir(),
        null_sink(),
        AcpPermissionPolicy::Cancel,
        &Default::default(),
        None,
        None,
    )
    .await
    .expect("spawn");

    let new_session = bridge
        .new_session(std::env::temp_dir(), vec![])
        .await
        .expect("session/new");
    let options = new_session
        .config_options
        .as_ref()
        .expect("strict agent returned config options, so the capability was advertised");
    assert_eq!(options[0].id.0.as_ref(), "model");
    bridge.shutdown().await.expect("shutdown ok");
}
