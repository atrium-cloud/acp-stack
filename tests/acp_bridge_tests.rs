//! Drives `AcpBridge::spawn` against the fake-agent gate baked into the
//! `acps` binary. The fake responds to `initialize` with hardcoded
//! capabilities so we can verify the spawn + handshake path end-to-end
//! without depending on a third-party agent.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use acp_stack::acp_bridge::{AcpBridge, SessionEventSink};
use acp_stack::config::{AgentConfig, AgentInstallConfig};

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
        command: env!("CARGO_BIN_EXE_acps").into(),
        args: vec!["__acps-test-fake-agent".into()],
        cwd: None,
        env: vec![],
        expected_sha256: None,
        restart: "never".into(),
        adapter: None,
        install: Some(AgentInstallConfig {
            install_type: "shell".into(),
            creates: "true".into(),
            shell: Some("true".into()),
            id: None,
            registry_url: None,
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
    )
    .await
    .expect("bridge spawns");
    let caps = bridge.capabilities();
    assert_eq!(caps.protocol_version, 1);
    assert_eq!(caps.agent_name.as_deref(), Some("acps-fake-agent"));
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
async fn spawn_does_not_forward_unlisted_parent_environment() {
    let mut config = fake_agent_config();
    config
        .args
        .extend(["--assert-env-absent".into(), "PATH".into()]);

    let bridge = AcpBridge::spawn(&config, fake_env(), std::env::temp_dir(), null_sink(), None)
        .await
        .expect("bridge spawns");
    let caps = bridge.capabilities();
    assert_eq!(caps.agent_title.as_deref(), Some("env absent"));
    bridge.shutdown().await.expect("shutdown ok");
}

#[tokio::test]
async fn new_session_round_trips_and_prompt_emits_notifications() {
    use agent_client_protocol::schema::{ContentBlock, PromptRequest, TextContent};

    let sink = Arc::new(InMemorySink::default());
    let sink_dyn: Arc<dyn SessionEventSink> = sink.clone();
    let bridge = AcpBridge::spawn(
        &fake_agent_config(),
        fake_env(),
        std::env::temp_dir(),
        sink_dyn,
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
    let stop = bridge.prompt_session(prompt).await.expect("session/prompt");
    assert!(matches!(
        stop,
        agent_client_protocol::schema::StopReason::EndTurn
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
async fn load_session_returns_unsupported_capability_when_agent_disables_flag() {
    let mut config = fake_agent_config();
    config.args.push("--no-cap-load-session".into());
    let bridge = AcpBridge::spawn(&config, fake_env(), std::env::temp_dir(), null_sink(), None)
        .await
        .expect("spawn");
    assert!(!bridge.capabilities().supports_load_session());

    let err = bridge
        .load_session(
            agent_client_protocol::schema::SessionId::new("sess_does_not_exist"),
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
    let bridge = AcpBridge::spawn(&config, fake_env(), std::env::temp_dir(), null_sink(), None)
        .await
        .expect("spawn");

    let err = bridge
        .resume_session(
            agent_client_protocol::schema::SessionId::new("sess_does_not_exist"),
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
    let bridge = AcpBridge::spawn(&config, fake_env(), std::env::temp_dir(), null_sink(), None)
        .await
        .expect("spawn");

    let err = bridge
        .close_session(agent_client_protocol::schema::SessionId::new(
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
    use agent_client_protocol::schema::{ContentBlock, PromptRequest, TextContent};

    let sink = Arc::new(BlockingSink::default());
    let sink_dyn: Arc<dyn SessionEventSink> = sink.clone();
    let bridge = Arc::new(
        AcpBridge::spawn(
            &fake_agent_config(),
            fake_env(),
            std::env::temp_dir(),
            sink_dyn,
            None,
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
    use agent_client_protocol::schema::{ContentBlock, PromptRequest, StopReason, TextContent};
    let bridge = AcpBridge::spawn(
        &fake_agent_config(),
        fake_env(),
        std::env::temp_dir(),
        null_sink(),
        None,
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
    let stop = bridge.prompt_session(prompt).await.expect("prompt");
    assert!(matches!(stop, StopReason::Cancelled));
    bridge.shutdown().await.expect("shutdown ok");
}
