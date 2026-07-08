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
        None,
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
async fn terminate_probe_terminates_the_child() {
    let bridge = AcpBridge::spawn(
        &fake_agent_config(),
        fake_env(),
        std::env::temp_dir(),
        null_sink(),
        None,
        &Default::default(),
        None,
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
        None,
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
        None,
        &Default::default(),
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

#[tokio::test]
async fn list_sessions_returns_agent_sessions() {
    let bridge = AcpBridge::spawn(
        &fake_agent_config(),
        fake_env(),
        std::env::temp_dir(),
        null_sink(),
        None,
        &Default::default(),
        None,
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
        None,
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
        None,
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
        None,
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
        None,
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
        None,
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
        None,
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
        None,
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
        None,
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
        None,
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
        None,
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
    use agent_client_protocol::schema::v1::{ContentBlock, PromptRequest, StopReason, TextContent};
    let bridge = AcpBridge::spawn(
        &fake_agent_config(),
        fake_env(),
        std::env::temp_dir(),
        null_sink(),
        None,
        &Default::default(),
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
    let stop = bridge
        .prompt_session(prompt)
        .await
        .expect("prompt")
        .stop_reason;
    assert!(matches!(stop, StopReason::Cancelled));
    bridge.shutdown().await.expect("shutdown ok");
}

// --- terminal/* round-trips -------------------------------------------------
//
// The placebo drives the client's terminal handlers during a prompt and
// reports the round-trip as a `terminal-report:{json}` message chunk, which
// these tests read back out of the sink.

const RESOURCE_NOT_FOUND_CODE: i64 = -32002;
const INVALID_PARAMS_CODE: i64 = -32602;

/// Run a prompt against a placebo configured with `terminal_flags` and return
/// (report, bridge, sink). The bridge is still running so callers can assert
/// shutdown behavior; most tests just shut it down.
async fn run_terminal_probe(
    terminal_flags: &[&str],
    command_log: Option<acp_stack::runtime::agent::acp_bridge::TerminalCommandLog>,
) -> (serde_json::Value, AcpBridge, Arc<InMemorySink>) {
    use agent_client_protocol::schema::v1::{ContentBlock, PromptRequest, TextContent};
    let mut config = fake_agent_config();
    config
        .args
        .extend(terminal_flags.iter().map(|s| s.to_string()));
    let sink = Arc::new(InMemorySink::default());
    let sink_dyn: Arc<dyn SessionEventSink> = sink.clone();
    let bridge = AcpBridge::spawn(
        &config,
        fake_env(),
        std::env::temp_dir(),
        sink_dyn,
        None,
        &Default::default(),
        command_log,
    )
    .await
    .expect("spawn");
    let session = bridge
        .new_session(std::env::temp_dir(), vec![])
        .await
        .expect("session/new");
    let prompt = PromptRequest::new(
        session.session_id.clone(),
        vec![ContentBlock::Text(TextContent::new("run terminal probe"))],
    );
    bridge.prompt_session(prompt).await.expect("session/prompt");

    // Notification persistence goes through a spawned task inside the sink;
    // poll briefly for the report chunk instead of assuming ordering.
    let mut report = None;
    for _ in 0..100 {
        {
            let events = sink.events.lock().expect("sink lock");
            for event in events.iter() {
                if let Some(index) = event.payload.find("terminal-report:") {
                    let tail = &event.payload[index + "terminal-report:".len()..];
                    // The report JSON is embedded inside a JSON string field;
                    // decode by re-parsing the payload and extracting the text.
                    let payload: serde_json::Value =
                        serde_json::from_str(&event.payload).expect("payload parses");
                    let text = find_terminal_report_text(&payload)
                        .unwrap_or_else(|| panic!("report text missing in {tail}"));
                    let json = text
                        .strip_prefix("terminal-report:")
                        .expect("report prefix");
                    report = Some(serde_json::from_str(json).expect("report parses"));
                    break;
                }
            }
        }
        if report.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let report = report.expect("placebo emitted a terminal-report chunk");
    (report, bridge, sink)
}

/// Recursively find the string value carrying the terminal report.
fn find_terminal_report_text(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(text) if text.starts_with("terminal-report:") => {
            Some(text.clone())
        }
        serde_json::Value::Object(map) => map.values().find_map(find_terminal_report_text),
        serde_json::Value::Array(items) => items.iter().find_map(find_terminal_report_text),
        _ => None,
    }
}

fn open_test_state() -> (
    tempfile::TempDir,
    Arc<tokio::sync::Mutex<acp_stack::state::StateStore>>,
) {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let store =
        acp_stack::state::StateStore::open(tempdir.path().join("state.sqlite")).expect("open");
    store.migrate().expect("migrate");
    (tempdir, Arc::new(tokio::sync::Mutex::new(store)))
}

#[tokio::test]
async fn terminal_create_returns_output_and_records_acp_command() {
    let (_tempdir, state) = open_test_state();
    let event_hub = acp_stack::events::EventHub::new();
    // Subscribe before the probe runs: the hub is a broadcast channel, so
    // only events published after subscription are observable.
    let mut live_events = event_hub.subscribe();
    let (report, bridge, _sink) = run_terminal_probe(
        &[
            "--terminal-command",
            "printf",
            "--terminal-arg",
            "hi-terminal",
        ],
        Some(acp_stack::runtime::agent::acp_bridge::TerminalCommandLog {
            state: state.clone(),
            event_hub: event_hub.clone(),
        }),
    )
    .await;
    assert_eq!(report["exit_code"], 0);
    assert_eq!(report["signal"], serde_json::Value::Null);
    assert_eq!(report["output"], "hi-terminal");
    assert_eq!(report["truncated"], false);
    assert_eq!(report["post_release_error_code"], RESOURCE_NOT_FOUND_CODE);
    bridge.shutdown().await.expect("shutdown ok");

    let commands = state
        .lock()
        .await
        .query_commands(acp_stack::state::CommandFilter {
            limit: 10,
            ..Default::default()
        })
        .expect("query commands");
    let row = commands
        .iter()
        .find(|row| row.origin == "acp")
        .expect("acp-origin command row recorded");
    assert_eq!(row.command, "printf hi-terminal");
    assert_eq!(row.status, "exited");
    assert!(row.session_id.is_some());

    // Terminal output and lifecycle transitions must fan out on the
    // per-command live topic exactly like gateway commands do.
    let topic = format!("commands.{}", row.id);
    let mut saw_output_chunk = false;
    let mut saw_exited = false;
    while let Ok(event) = live_events.try_recv() {
        if event.topic != topic {
            continue;
        }
        let kind = event.payload["kind"].as_str().unwrap_or_default();
        if kind.ends_with(".stdout")
            && event.payload["data"]["data"]
                .as_str()
                .is_some_and(|data| data.contains("hi-terminal"))
        {
            saw_output_chunk = true;
        }
        if kind == "command.exited" {
            saw_exited = true;
        }
    }
    assert!(saw_output_chunk, "missing live stdout chunk on {topic}");
    assert!(saw_exited, "missing live command.exited on {topic}");
}

#[tokio::test]
async fn terminal_create_confines_cwd_outside_workspace() {
    let (report, bridge, _sink) = run_terminal_probe(
        &[
            "--terminal-command",
            "printf",
            "--terminal-arg",
            "never-runs",
            "--terminal-cwd",
            "/",
        ],
        None,
    )
    .await;
    assert_eq!(report["create_error_code"], INVALID_PARAMS_CODE);
    bridge.shutdown().await.expect("shutdown ok");
}

#[tokio::test]
async fn terminal_output_truncates_to_newest_bytes_at_byte_limit() {
    let (report, bridge, _sink) = run_terminal_probe(
        &[
            "--terminal-command",
            "sh",
            "--terminal-arg=-c",
            "--terminal-arg=printf aaaaabbbbb",
            "--terminal-byte-limit",
            "5",
        ],
        None,
    )
    .await;
    assert_eq!(report["exit_code"], 0);
    // Spec direction: truncation drops the OLDEST bytes and keeps the newest.
    assert_eq!(report["output"], "bbbbb");
    assert_eq!(report["truncated"], true);
    bridge.shutdown().await.expect("shutdown ok");
}

#[tokio::test]
async fn terminal_wait_for_exit_returns_exit_code() {
    let (report, bridge, _sink) = run_terminal_probe(
        &[
            "--terminal-command",
            "sh",
            "--terminal-arg=-c",
            "--terminal-arg=exit 7",
        ],
        None,
    )
    .await;
    assert_eq!(report["exit_code"], 7);
    assert_eq!(report["signal"], serde_json::Value::Null);
    bridge.shutdown().await.expect("shutdown ok");
}

#[tokio::test]
async fn terminal_kill_terminates_but_output_remains_readable() {
    let (report, bridge, _sink) = run_terminal_probe(
        &[
            "--terminal-command",
            "sh",
            "--terminal-arg=-c",
            "--terminal-arg=printf started; sleep 30",
            "--terminal-kill",
        ],
        None,
    )
    .await;
    assert_eq!(report["exit_code"], serde_json::Value::Null);
    let signal = report["signal"].as_str().expect("signal name");
    assert!(
        signal == "SIGTERM" || signal == "SIGKILL",
        "unexpected signal {signal}"
    );
    // Output was read AFTER the kill and is still the buffered content.
    assert_eq!(report["output"], "started");
    bridge.shutdown().await.expect("shutdown ok");
}

// The strict placebo only touches terminal/* when the client advertised
// `terminal: true`, so a non-skipped full lifecycle proves the capability is
// on the wire: create -> output (with byte limit) -> kill -> wait -> output ->
// release -> post-release error.
#[tokio::test]
async fn terminal_full_lifecycle_under_advertised_capability() {
    let (report, bridge, _sink) = run_terminal_probe(
        &[
            "--require-terminal",
            "--terminal-command",
            "sh",
            "--terminal-arg=-c",
            "--terminal-arg=printf started; sleep 30",
            "--terminal-byte-limit",
            "1024",
            "--terminal-kill",
            "--terminal-release-unknown",
        ],
        None,
    )
    .await;
    assert_eq!(
        report.get("skipped"),
        None,
        "strict agent skipped the terminal probe; capability not advertised"
    );
    assert_eq!(
        report["release_unknown_error_code"],
        RESOURCE_NOT_FOUND_CODE
    );
    assert_eq!(report["exit_code"], serde_json::Value::Null);
    assert!(report["signal"].is_string());
    assert_eq!(report["output"], "started");
    assert_eq!(report["post_release_error_code"], RESOURCE_NOT_FOUND_CODE);
    bridge.shutdown().await.expect("shutdown ok");
}

#[tokio::test]
async fn terminal_release_of_unknown_id_errors() {
    let (report, bridge, _sink) = run_terminal_probe(&["--terminal-release-unknown"], None).await;
    assert_eq!(
        report["release_unknown_error_code"],
        RESOURCE_NOT_FOUND_CODE
    );
    bridge.shutdown().await.expect("shutdown ok");
}

// --- fs/* round-trips --------------------------------------------------------
//
// The probe workspace root is std::env::temp_dir() (the bridge spawn cwd), so
// per-test tempdirs created under it are inside the workspace.

#[tokio::test]
async fn fs_write_persists_to_disk_and_records_audit_event() {
    let dir = tempfile::tempdir().expect("tempdir");
    let target = dir.path().join("fs-probe.txt");
    let (_state_dir, state) = open_test_state();
    let (report, bridge, _sink) = run_terminal_probe(
        &[
            "--fs-write-path",
            &target.to_string_lossy(),
            "--fs-write-content",
            "hello fs",
            "--fs-read-path",
            &target.to_string_lossy(),
        ],
        Some(acp_stack::runtime::agent::acp_bridge::TerminalCommandLog {
            state: state.clone(),
            event_hub: acp_stack::events::EventHub::new(),
        }),
    )
    .await;
    assert_eq!(report["fs_write_ok"], true);
    assert_eq!(report["fs_read_content"], "hello fs");
    let on_disk = std::fs::read_to_string(&target).expect("file exists");
    assert_eq!(on_disk, "hello fs");
    bridge.shutdown().await.expect("shutdown ok");

    let events = state
        .lock()
        .await
        .query_events(acp_stack::state::LogFilter {
            limit: 50,
            kind: Some("fs.write"),
            source: Some("acp"),
            ..Default::default()
        })
        .expect("query events");
    assert_eq!(events.len(), 1, "expected one acp fs.write audit event");
    assert!(events[0].payload_json.contains("fs-probe.txt"));
}

#[tokio::test]
async fn fs_read_honors_line_and_limit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let target = dir.path().join("lines.txt");
    std::fs::write(&target, "one\ntwo\nthree\nfour\n").expect("seed file");
    let (report, bridge, _sink) = run_terminal_probe(
        &[
            "--fs-read-path",
            &target.to_string_lossy(),
            "--fs-read-line",
            "2",
            "--fs-read-limit",
            "2",
        ],
        None,
    )
    .await;
    assert_eq!(report["fs_read_content"], "two\nthree");
    bridge.shutdown().await.expect("shutdown ok");
}

#[tokio::test]
async fn fs_rejects_out_of_workspace_write_and_missing_file_read() {
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("does-not-exist.txt");
    let (report, bridge, _sink) = run_terminal_probe(
        &[
            "--fs-write-path",
            "/etc/acp-stack-escape-attempt",
            "--fs-read-path",
            &missing.to_string_lossy(),
        ],
        None,
    )
    .await;
    assert_eq!(report["fs_write_error_code"], INVALID_PARAMS_CODE);
    assert_eq!(report["fs_read_error_code"], RESOURCE_NOT_FOUND_CODE);
    bridge.shutdown().await.expect("shutdown ok");
}

#[cfg(unix)]
#[tokio::test]
async fn fs_write_rejects_symlink_target() {
    let dir = tempfile::tempdir().expect("tempdir");
    let outside = tempfile::NamedTempFile::new().expect("outside file");
    let link = dir.path().join("sneaky-link");
    std::os::unix::fs::symlink(outside.path(), &link).expect("symlink");
    let (report, bridge, _sink) =
        run_terminal_probe(&["--fs-write-path", &link.to_string_lossy()], None).await;
    assert_eq!(report["fs_write_error_code"], INVALID_PARAMS_CODE);
    bridge.shutdown().await.expect("shutdown ok");
}

// The strict placebo only touches fs/* when the client advertised both
// fs.readTextFile and fs.writeTextFile, so a non-skipped write-then-read
// round-trip proves the capability is on the wire.
#[tokio::test]
async fn fs_round_trip_under_advertised_capability() {
    let dir = tempfile::tempdir().expect("tempdir");
    let target = dir.path().join("strict.txt");
    let (report, bridge, _sink) = run_terminal_probe(
        &[
            "--require-fs",
            "--fs-write-path",
            &target.to_string_lossy(),
            "--fs-write-content",
            "strict fs round trip",
            "--fs-read-path",
            &target.to_string_lossy(),
        ],
        None,
    )
    .await;
    assert_eq!(
        report.get("fs_skipped"),
        None,
        "strict agent skipped the fs probe; capability not advertised"
    );
    assert_eq!(report["fs_write_ok"], true);
    assert_eq!(report["fs_read_content"], "strict fs round trip");
    bridge.shutdown().await.expect("shutdown ok");
}

#[tokio::test]
async fn shutdown_kills_live_terminals() {
    let pid_dir = tempfile::tempdir().expect("tempdir");
    let pid_file = pid_dir.path().join("terminal.pid");
    let script = format!("echo $$ > {}; sleep 30", pid_file.to_string_lossy());
    let (report, bridge, _sink) = run_terminal_probe(
        &[
            "--terminal-command",
            "sh",
            "--terminal-arg=-c",
            &format!("--terminal-arg={script}"),
            "--terminal-orphan",
        ],
        None,
    )
    .await;
    assert_eq!(report["orphaned"], true);

    // Wait until the child has written its pid, proving it is alive.
    let mut pid: Option<i32> = None;
    for _ in 0..100 {
        if let Ok(text) = std::fs::read_to_string(&pid_file)
            && let Ok(parsed) = text.trim().parse::<i32>()
        {
            pid = Some(parsed);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let pid = pid.expect("terminal child wrote its pid");

    bridge.shutdown().await.expect("shutdown ok");

    // The terminal child is in its own process group, so only the registry
    // drain can have killed it. kill(pid, 0) refuses for a dead pid.
    #[cfg(unix)]
    {
        let mut alive = true;
        for _ in 0..50 {
            // SAFETY: signal 0 is the standard existence probe.
            if unsafe { libc::kill(pid, 0) } != 0 {
                alive = false;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(!alive, "terminal child {pid} survived bridge shutdown");
    }
}
