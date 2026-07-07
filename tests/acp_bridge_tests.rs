#![cfg(feature = "test-fixtures")]

//! Drives `AcpBridge::spawn` against the standalone placebo ACP fixture so the
//! spawn + handshake path is exercised end-to-end without depending on a
//! third-party agent.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use acp_stack::config::{AgentConfig, AgentInstallConfig};
use acp_stack::runtime::agent::acp_bridge::{AcpBridge, SessionEventSink};

#[derive(Default)]
struct CapturedEvent {
    session_id: String,
    kind: String,
    payload: String,
}

#[derive(Default)]
struct InMemorySink {
    events: Mutex<Vec<CapturedEvent>>,
}

impl SessionEventSink for InMemorySink {
    fn append<'a>(
        &'a self,
        session_id: &'a str,
        kind: &'a str,
        payload_json: &'a str,
    ) -> futures::future::BoxFuture<'a, ()> {
        Box::pin(async move {
            self.events.lock().expect("sink lock").push(CapturedEvent {
                session_id: session_id.to_owned(),
                kind: kind.to_owned(),
                payload: payload_json.to_owned(),
            });
        })
    }
}

fn null_sink() -> Arc<dyn SessionEventSink> {
    Arc::new(InMemorySink::default())
}

fn fake_agent_config() -> AgentConfig {
    AgentConfig {
        id: "fake".into(),
        name: "fake".into(),
        command: env!("CARGO_BIN_EXE_placebo-agent").into(),
        args: vec!["acp".into()],
        cwd: None,
        env: vec![],
        expected_sha256: None,
        restart: "never".into(),
        mode: None,
        model: None,
        harness_version: None,
        adapter: None,
        provider: None,
        subagent: None,
        auto_update: None,
        install: Some(AgentInstallConfig {
            install_type: "shell".into(),
            creates: "true".into(),
            shell: Some("true".into()),
        }),
    }
}

fn fake_env() -> HashMap<String, String> {
    HashMap::new()
}

#[tokio::test]
async fn spawn_completes_initialize_and_captures_capabilities() {
    let bridge = AcpBridge::spawn(
        &fake_agent_config(),
        fake_env(),
        std::env::temp_dir(),
        null_sink(),
        None,
        &Default::default(),
    )
    .await
    .expect("bridge spawns");
    let caps = bridge.capabilities();
    assert_eq!(caps.protocol_version, 1);
    assert_eq!(caps.agent_name.as_deref(), Some("placebo-agent"));
    bridge.shutdown().await.expect("shutdown ok");
}

#[tokio::test]
async fn shutdown_terminates_the_child() {
    let bridge = AcpBridge::spawn(
        &fake_agent_config(),
        fake_env(),
        std::env::temp_dir(),
        null_sink(),
        None,
        &Default::default(),
    )
    .await
    .expect("spawn ok");
    let pid = bridge.pid().expect("pid available");
    bridge.shutdown().await.expect("shutdown ok");

    // After shutdown, the child should be gone. We can't query directly via
    // a portable API, but the OS will refuse `kill(pid, 0)` for a dead pid.
    #[cfg(unix)]
    {
        // SAFETY: kill with signal 0 is the standard "does this pid exist"
        // probe; never delivers a signal.
        unsafe {
            let alive = libc::kill(pid as i32, 0);
            if alive == 0 {
                // The PID may have been reused; give the kernel a beat then
                // recheck. If still alive, we've leaked.
                std::thread::sleep(std::time::Duration::from_millis(50));
                let still_alive = libc::kill(pid as i32, 0);
                assert_ne!(
                    still_alive, 0,
                    "fake agent pid {pid} appears to still be running after shutdown"
                );
            }
        }
    }
}

#[tokio::test]
async fn terminate_probe_terminates_the_child() {
    let bridge = AcpBridge::spawn(
        &fake_agent_config(),
        fake_env(),
        std::env::temp_dir(),
        null_sink(),
        None,
        &Default::default(),
    )
    .await
    .expect("spawn ok");
    let pid = bridge.pid().expect("pid available");
    bridge.terminate_probe().await.expect("terminate ok");

    #[cfg(unix)]
    unsafe {
        let alive = libc::kill(pid as i32, 0);
        if alive == 0 {
            std::thread::sleep(std::time::Duration::from_millis(50));
            let still_alive = libc::kill(pid as i32, 0);
            assert_ne!(
                still_alive, 0,
                "fake agent pid {pid} appears to still be running after probe terminate"
            );
        }
    }
}

#[tokio::test]
async fn spawn_forwards_only_reserved_runtime_context_and_explicit_env() {
    let home = std::env::var("HOME").expect("HOME must be set for bridge runtime context test");
    let mut config = fake_agent_config();
    config.args.extend([
        "--assert-env-present".into(),
        "HOME".into(),
        "--assert-env-absent".into(),
        "LANG".into(),
        "--assert-env-present".into(),
        "ACP_STACK_EXPLICIT_ENV".into(),
        "--assert-env-not-equals".into(),
        "HOME".into(),
        "secret-home".into(),
    ]);
    let mut env = fake_env();
    env.insert("HOME".into(), "secret-home".into());
    env.insert("ACP_STACK_EXPLICIT_ENV".into(), "present".into());

    let bridge = AcpBridge::spawn(
        &config,
        env,
        std::env::temp_dir(),
        null_sink(),
        None,
        &Default::default(),
    )
    .await
    .expect("bridge spawns");
    let caps = bridge.capabilities();
    assert_eq!(caps.agent_title.as_deref(), Some("env assertions passed"));
    assert_ne!(home, "secret-home");
    bridge.shutdown().await.expect("shutdown ok");
}

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
        None,
        &Default::default(),
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
        None,
        &Default::default(),
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

#[tokio::test]
async fn list_sessions_returns_agent_sessions() {
    let bridge = AcpBridge::spawn(
        &fake_agent_config(),
        fake_env(),
        std::env::temp_dir(),
        null_sink(),
        None,
        &Default::default(),
    )
    .await
    .expect("spawn");
    assert!(bridge.capabilities().supports_list_sessions());

    let sessions = bridge.list_sessions().await.expect("session/list");
    assert!(
        sessions
            .iter()
            .any(|session| session.session_id.0.to_string() == "sess_listed_0"),
        "sessions = {sessions:?}"
    );
    bridge.shutdown().await.expect("shutdown ok");
}

#[tokio::test]
async fn list_sessions_follows_pagination() {
    let mut config = fake_agent_config();
    config.args.push("--session-list-paginated".into());
    let bridge = AcpBridge::spawn(
        &config,
        fake_env(),
        std::env::temp_dir(),
        null_sink(),
        None,
        &Default::default(),
    )
    .await
    .expect("spawn");

    let sessions = bridge.list_sessions().await.expect("session/list");
    let ids = sessions
        .iter()
        .map(|session| session.session_id.0.to_string())
        .collect::<Vec<_>>();
    assert_eq!(ids, vec!["sess_listed_page_1", "sess_listed_page_2"]);
    bridge.shutdown().await.expect("shutdown ok");
}

#[tokio::test]
async fn list_sessions_returns_unsupported_capability_when_agent_disables_flag() {
    let mut config = fake_agent_config();
    config.args.push("--no-cap-list-session".into());
    let bridge = AcpBridge::spawn(
        &config,
        fake_env(),
        std::env::temp_dir(),
        null_sink(),
        None,
        &Default::default(),
    )
    .await
    .expect("spawn");
    assert!(!bridge.capabilities().supports_list_sessions());

    let err = bridge
        .list_sessions()
        .await
        .expect_err("must report unsupported capability");
    assert!(matches!(
        err,
        acp_stack::error::StackError::AgentUnsupportedCapability {
            name: "session/list"
        }
    ));
    bridge.shutdown().await.expect("shutdown ok");
}

#[tokio::test]
async fn list_sessions_rejects_repeated_cursor() {
    let mut config = fake_agent_config();
    config.args.push("--session-list-repeated-cursor".into());
    let bridge = AcpBridge::spawn(
        &config,
        fake_env(),
        std::env::temp_dir(),
        null_sink(),
        None,
        &Default::default(),
    )
    .await
    .expect("spawn");

    let err = bridge
        .list_sessions()
        .await
        .expect_err("must reject repeated cursor");
    assert!(matches!(
        err,
        acp_stack::error::StackError::AgentRequestFailed {
            method: "session/list",
            ..
        }
    ));
    assert!(err.to_string().contains("repeated pagination cursor"));
    bridge.shutdown().await.expect("shutdown ok");
}

#[tokio::test]
async fn load_session_returns_unsupported_capability_when_agent_disables_flag() {
    let mut config = fake_agent_config();
    config.args.push("--no-cap-load-session".into());
    let bridge = AcpBridge::spawn(
        &config,
        fake_env(),
        std::env::temp_dir(),
        null_sink(),
        None,
        &Default::default(),
    )
    .await
    .expect("spawn");
    assert!(!bridge.capabilities().supports_load_session());

    let err = bridge
        .load_session(
            agent_client_protocol::schema::v1::SessionId::new("sess_does_not_exist"),
            std::env::temp_dir(),
            vec![],
        )
        .await
        .expect_err("must report unsupported capability");
    assert!(matches!(
        err,
        acp_stack::error::StackError::AgentUnsupportedCapability {
            name: "session/load"
        }
    ));

    bridge.shutdown().await.expect("shutdown ok");
}

#[tokio::test]
async fn resume_session_returns_unsupported_capability_when_agent_disables_flag() {
    let mut config = fake_agent_config();
    config.args.push("--no-cap-resume-session".into());
    let bridge = AcpBridge::spawn(
        &config,
        fake_env(),
        std::env::temp_dir(),
        null_sink(),
        None,
        &Default::default(),
    )
    .await
    .expect("spawn");

    let err = bridge
        .resume_session(
            agent_client_protocol::schema::v1::SessionId::new("sess_does_not_exist"),
            std::env::temp_dir(),
            vec![],
        )
        .await
        .expect_err("must report unsupported capability");
    assert!(matches!(
        err,
        acp_stack::error::StackError::AgentUnsupportedCapability {
            name: "session/resume"
        }
    ));

    bridge.shutdown().await.expect("shutdown ok");
}

#[tokio::test]
async fn close_session_returns_unsupported_capability_when_agent_disables_flag() {
    let mut config = fake_agent_config();
    config.args.push("--no-cap-close-session".into());
    let bridge = AcpBridge::spawn(
        &config,
        fake_env(),
        std::env::temp_dir(),
        null_sink(),
        None,
        &Default::default(),
    )
    .await
    .expect("spawn");

    let err = bridge
        .close_session(agent_client_protocol::schema::v1::SessionId::new(
            "sess_does_not_exist",
        ))
        .await
        .expect_err("must report unsupported capability");
    assert!(matches!(
        err,
        acp_stack::error::StackError::AgentUnsupportedCapability {
            name: "session/close"
        }
    ));

    bridge.shutdown().await.expect("shutdown ok");
}

#[tokio::test]
async fn fork_session_returns_child_session() {
    let bridge = AcpBridge::spawn(
        &fake_agent_config(),
        fake_env(),
        std::env::temp_dir(),
        null_sink(),
        None,
        &Default::default(),
    )
    .await
    .expect("spawn");
    assert!(bridge.capabilities().supports_fork_session());

    let fork = bridge
        .fork_session(
            agent_client_protocol::schema::v1::SessionId::new("sess_parent"),
            std::env::temp_dir(),
            vec![],
            None,
        )
        .await
        .expect("session/fork");
    assert_eq!(fork.session_id.0.as_ref(), "sess_fake_0");
    bridge.shutdown().await.expect("shutdown ok");
}

#[tokio::test]
async fn fork_session_returns_unsupported_capability_when_agent_disables_flag() {
    let mut config = fake_agent_config();
    config.args.push("--no-cap-fork-session".into());
    let bridge = AcpBridge::spawn(
        &config,
        fake_env(),
        std::env::temp_dir(),
        null_sink(),
        None,
        &Default::default(),
    )
    .await
    .expect("spawn");

    let err = bridge
        .fork_session(
            agent_client_protocol::schema::v1::SessionId::new("sess_parent"),
            std::env::temp_dir(),
            vec![],
            None,
        )
        .await
        .expect_err("must report unsupported capability");
    assert!(matches!(
        err,
        acp_stack::error::StackError::AgentUnsupportedCapability {
            name: "session/fork"
        }
    ));
    bridge.shutdown().await.expect("shutdown ok");
}

#[tokio::test]
async fn fork_session_sends_message_id_when_capability_is_present() {
    let mut config = fake_agent_config();
    config.args.extend([
        "--expect-fork-message-id".into(),
        "00000000-0000-4000-8000-000000000001".into(),
    ]);
    let bridge = AcpBridge::spawn(
        &config,
        fake_env(),
        std::env::temp_dir(),
        null_sink(),
        None,
        &Default::default(),
    )
    .await
    .expect("spawn");
    assert!(bridge.capabilities().supports_fork_message_id());

    let fork = bridge
        .fork_session(
            agent_client_protocol::schema::v1::SessionId::new("sess_parent"),
            std::env::temp_dir(),
            vec![],
            Some("00000000-0000-4000-8000-000000000001".to_owned()),
        )
        .await
        .expect("session/fork with message id");
    assert_eq!(fork.session_id.0.as_ref(), "sess_fake_0");
    bridge.shutdown().await.expect("shutdown ok");
}

#[tokio::test]
async fn fork_session_rejects_message_id_when_capability_is_missing() {
    let mut config = fake_agent_config();
    config.args.push("--no-cap-fork-message-id".into());
    let bridge = AcpBridge::spawn(
        &config,
        fake_env(),
        std::env::temp_dir(),
        null_sink(),
        None,
        &Default::default(),
    )
    .await
    .expect("spawn");
    assert!(bridge.capabilities().supports_fork_session());
    assert!(!bridge.capabilities().supports_fork_message_id());

    let err = bridge
        .fork_session(
            agent_client_protocol::schema::v1::SessionId::new("sess_parent"),
            std::env::temp_dir(),
            vec![],
            Some("00000000-0000-4000-8000-000000000001".to_owned()),
        )
        .await
        .expect_err("message-id fork requires explicit support");
    assert!(matches!(
        err,
        acp_stack::error::StackError::AgentUnsupportedCapability {
            name: "session/fork.messageId"
        }
    ));
    bridge.shutdown().await.expect("shutdown ok");
}

#[derive(Default)]
struct BlockingSink {
    append_started: tokio::sync::Notify,
    allow_append_finish: tokio::sync::Notify,
    flush_started: tokio::sync::Notify,
    append_done: AtomicBool,
}

impl SessionEventSink for BlockingSink {
    fn append<'a>(
        &'a self,
        _session_id: &'a str,
        _kind: &'a str,
        _payload_json: &'a str,
    ) -> futures::future::BoxFuture<'a, ()> {
        Box::pin(async move {
            self.append_started.notify_waiters();
            self.allow_append_finish.notified().await;
            self.append_done.store(true, Ordering::SeqCst);
        })
    }

    fn flush<'a>(&'a self) -> futures::future::BoxFuture<'a, ()> {
        Box::pin(async move {
            assert!(
                self.append_done.load(Ordering::SeqCst),
                "shutdown flushed the session event sink before the connection task drained"
            );
            self.flush_started.notify_waiters();
        })
    }
}

#[tokio::test]
async fn shutdown_waits_for_connection_task_before_flushing_sink() {
    use agent_client_protocol::schema::v1::{ContentBlock, PromptRequest, TextContent};

    let sink = Arc::new(BlockingSink::default());
    let sink_dyn: Arc<dyn SessionEventSink> = sink.clone();
    let bridge = Arc::new(
        AcpBridge::spawn(
            &fake_agent_config(),
            fake_env(),
            std::env::temp_dir(),
            sink_dyn,
            None,
            &Default::default(),
        )
        .await
        .expect("spawn"),
    );
    let response = bridge
        .new_session(std::env::temp_dir(), vec![])
        .await
        .expect("new session");
    let prompt = PromptRequest::new(
        response.session_id,
        vec![ContentBlock::Text(TextContent::new("block until shutdown"))],
    );
    let prompt_bridge = Arc::clone(&bridge);
    let prompt_task = tokio::spawn(async move { prompt_bridge.prompt_session(prompt).await });

    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        sink.append_started.notified(),
    )
    .await
    .expect("append started");

    let shutdown_bridge = Arc::clone(&bridge);
    let shutdown_task = tokio::spawn(async move { shutdown_bridge.shutdown().await });
    let early_flush = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        sink.flush_started.notified(),
    )
    .await;
    assert!(
        early_flush.is_err(),
        "sink flushed while a notification handler was still appending"
    );

    sink.allow_append_finish.notify_waiters();
    shutdown_task
        .await
        .expect("shutdown task joins")
        .expect("shutdown ok");
    let _ = prompt_task.await.expect("prompt task joins");
}

#[tokio::test]
async fn cancel_session_settles_prompt_with_cancelled_stop_reason() {
    use agent_client_protocol::schema::v1::{ContentBlock, PromptRequest, StopReason, TextContent};
    let bridge = AcpBridge::spawn(
        &fake_agent_config(),
        fake_env(),
        std::env::temp_dir(),
        null_sink(),
        None,
        &Default::default(),
    )
    .await
    .expect("spawn");
    let response = bridge
        .new_session(std::env::temp_dir(), vec![])
        .await
        .expect("new");
    let session_id = response.session_id;

    // The fake agent only checks the cancel flag after it emits its
    // notifications, so firing the notification first and then sending the
    // prompt deterministically returns `cancelled`.
    bridge
        .cancel_session(session_id.clone())
        .await
        .expect("cancel");
    let prompt = PromptRequest::new(
        session_id,
        vec![ContentBlock::Text(TextContent::new("ignored"))],
    );
    let stop = bridge
        .prompt_session(prompt)
        .await
        .expect("prompt")
        .stop_reason;
    assert!(matches!(stop, StopReason::Cancelled));
    bridge.shutdown().await.expect("shutdown ok");
}
