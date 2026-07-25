use super::*;

#[test]
fn keep_newest_retains_tail_at_char_boundary() {
    // Under limit: whole string, untruncated.
    assert_eq!(keep_newest("hello", 10), ("hello", false));
    assert_eq!(keep_newest("hello", 5), ("hello", false));
    // Over limit: newest bytes retained.
    assert_eq!(keep_newest("hello", 3), ("llo", true));
    // Multibyte: cutoff lands mid-'é' (2 bytes) and rounds UP past it,
    // so the result stays within the limit and on a boundary.
    let (kept, truncated) = keep_newest("héllo", 4);
    assert!(truncated);
    assert_eq!(kept, "llo");
    assert!(kept.len() as u64 <= 4);
    // 4-byte emoji: limit 6 can hold one emoji (4 bytes) but not one and
    // a half; cutoff rounds up to the next full glyph.
    let (kept, truncated) = keep_newest("🚀🚀🚀", 6);
    assert!(truncated);
    assert_eq!(kept, "🚀");
    // Limit 0 drops everything.
    let (kept, truncated) = keep_newest("hello", 0);
    assert_eq!(kept, "");
    assert!(truncated);
}

#[test]
fn effective_output_byte_limit_clamps_agent_requests() {
    assert_eq!(
        effective_output_byte_limit(None),
        DEFAULT_TERMINAL_OUTPUT_BYTE_LIMIT
    );
    assert_eq!(effective_output_byte_limit(Some(512)), 512);
    assert_eq!(
        effective_output_byte_limit(Some(u64::MAX)),
        MAX_TERMINAL_OUTPUT_BYTE_LIMIT
    );
}

#[test]
fn buffer_append_trims_to_cap_during_accumulation() {
    let mut buffer = TerminalBuffer::default();
    for index in 0..100 {
        buffer.append_capped(&format!("chunk-{index:03} "), 64);
        assert!(
            buffer.data.len() as u64 <= 64,
            "buffer exceeded cap at chunk {index}: {} bytes",
            buffer.data.len()
        );
    }
    assert!(buffer.truncated);
    // The newest chunk survived; the oldest did not.
    assert!(buffer.data.contains("chunk-099"));
    assert!(!buffer.data.contains("chunk-000"));
}

#[test]
fn terminal_environment_excludes_provider_keys() {
    let agent_env = vec![EnvVariable::new("MY_FLAG", "1")];
    let env = terminal_environment(&agent_env);
    assert_eq!(env.get("MY_FLAG").map(String::as_str), Some("1"));
    // Only the managed baseline plus agent vars — nothing else can be
    // present because composition starts from an empty map, never from
    // the agent process env (which carries provider API keys).
    let allowed = ["PATH", "HOME", "MY_FLAG"];
    for key in env.keys() {
        assert!(allowed.contains(&key.as_str()), "unexpected env var {key}");
    }
}

#[tokio::test]
async fn registered_terminal_captures_output_and_exit_code() {
    let cwd = std::env::temp_dir();
    let resolved = crate::runtime::mediation::commands::policy::resolve_cwd_under_workspace(
        &cwd,
        &cwd.to_string_lossy(),
    )
    .expect("resolve cwd");
    let child = crate::runtime::mediation::commands::exec::spawn_child(
        std::path::Path::new("/bin/sh"),
        &[
            "-c".to_owned(),
            "printf hi-from-terminal; exit 7".to_owned(),
        ],
        &resolved,
        None,
        &crate::config::SandboxConfig::default(),
        None,
    )
    .expect("spawn");

    let registry = Arc::new(TerminalRegistry::default());
    let terminal_id = registry
        .register("sess_test", child, DEFAULT_TERMINAL_OUTPUT_BYTE_LIMIT, None)
        .await
        .expect("register on open registry");
    let handle = registry
        .get("sess_test", &terminal_id)
        .await
        .expect("handle");

    let status = handle.wait_for_exit().await;
    assert_eq!(status.exit_code, Some(7));
    assert_eq!(status.signal, None);

    // The owner drains the pipes before publishing exit, so the full
    // output must be visible the moment wait_for_exit resolves — no
    // polling allowed here; that would mask a drain regression.
    let output = handle.buffer.lock().await.data.clone();
    assert_eq!(output, "hi-from-terminal");

    assert!(registry.remove("sess_test", &terminal_id).await.is_some());
    assert!(registry.get("sess_test", &terminal_id).await.is_none());
}

#[tokio::test]
async fn kill_terminates_long_running_child_and_publishes_signal() {
    let cwd = std::env::temp_dir();
    let resolved = crate::runtime::mediation::commands::policy::resolve_cwd_under_workspace(
        &cwd,
        &cwd.to_string_lossy(),
    )
    .expect("resolve cwd");
    let child = crate::runtime::mediation::commands::exec::spawn_child(
        std::path::Path::new("/bin/sh"),
        &["-c".to_owned(), "sleep 30".to_owned()],
        &resolved,
        None,
        &crate::config::SandboxConfig::default(),
        None,
    )
    .expect("spawn");

    let registry = Arc::new(TerminalRegistry::default());
    let terminal_id = registry
        .register("sess_test", child, DEFAULT_TERMINAL_OUTPUT_BYTE_LIMIT, None)
        .await
        .expect("register on open registry");
    let handle = registry
        .get("sess_test", &terminal_id)
        .await
        .expect("handle");

    assert!(handle.exit_status().is_none());
    handle.request_kill(Duration::from_millis(200)).await;
    let status = handle.wait_for_exit().await;
    assert_eq!(status.exit_code, None);
    assert_eq!(status.signal.as_deref(), Some("SIGTERM"));
}

#[tokio::test]
async fn create_terminal_defaults_cwd_to_session_cwd() {
    use agent_client_protocol::schema::v1::SessionId;

    struct CwdStubSink {
        cwd: String,
    }
    impl SessionEventSink for CwdStubSink {
        fn session_cwd<'a>(
            &'a self,
            _agent_session_id: &'a str,
        ) -> futures::future::BoxFuture<'a, Option<String>> {
            let cwd = self.cwd.clone();
            Box::pin(async move { Some(cwd) })
        }
        fn append<'a>(
            &'a self,
            _session_id: &'a str,
            _kind: &'a str,
            _payload_json: &'a str,
        ) -> futures::future::BoxFuture<'a, ()> {
            Box::pin(async {})
        }
    }

    let root = tempfile::tempdir().expect("workspace root");
    let session_dir = root.path().join("session-sub");
    std::fs::create_dir(&session_dir).expect("session subdir");

    let context = TerminalHandlerContext {
        registry: Arc::new(TerminalRegistry::default()),
        workspace_root: root.path().to_path_buf(),
        sandbox: crate::config::SandboxConfig::default(),
        network_provider: None,
        command_log: None,
        sink: Arc::new(CwdStubSink {
            cwd: session_dir.to_string_lossy().into_owned(),
        }),
    };
    // No cwd on the request: the handler must fall back to the session's
    // recorded cwd, not the workspace root.
    let request = CreateTerminalRequest::new(SessionId::new("sess_agent"), "/bin/pwd");
    let response = handle_create_terminal(&context, request)
        .await
        .expect("terminal created");

    let handle = context
        .registry
        .get("sess_agent", &response.terminal_id.0)
        .await
        .expect("handle");
    let status = handle.wait_for_exit().await;
    assert_eq!(status.exit_code, Some(0));
    let output = handle.buffer.lock().await.data.clone();
    let expected = std::fs::canonicalize(&session_dir).expect("canonical session dir");
    assert_eq!(output.trim_end(), expected.to_string_lossy().as_ref());
}

struct NoopStubSink;
impl SessionEventSink for NoopStubSink {
    fn append<'a>(
        &'a self,
        _session_id: &'a str,
        _kind: &'a str,
        _payload_json: &'a str,
    ) -> futures::future::BoxFuture<'a, ()> {
        Box::pin(async {})
    }
}

#[tokio::test]
async fn create_terminal_start_failure_finalizes_command_row() {
    use agent_client_protocol::schema::v1::SessionId;

    let state_dir = tempfile::tempdir().expect("tempdir");
    let db_path = state_dir.path().join("state.sqlite");
    let store = StateStore::open(db_path.clone()).expect("state open");
    store.migrate().expect("migrate");
    // Force start_command to fail while append_command (INSERT) and the
    // failure finalization (UPDATE to `failed`) still succeed: block only
    // the pending -> running transition.
    {
        let conn = rusqlite::Connection::open(&db_path).expect("second conn");
        conn.execute_batch(
            "CREATE TRIGGER block_running BEFORE UPDATE ON commands \
             WHEN NEW.status = 'running' \
             BEGIN SELECT RAISE(ABORT, 'forced start failure'); END;",
        )
        .expect("trigger installed");
    }
    let state = Arc::new(TokioMutex::new(store));

    let context = TerminalHandlerContext {
        registry: Arc::new(TerminalRegistry::default()),
        workspace_root: std::env::temp_dir(),
        sandbox: crate::config::SandboxConfig::default(),
        network_provider: None,
        command_log: Some(TerminalCommandLog {
            state: state.clone(),
            event_hub: EventHub::new(),
        }),
        sink: Arc::new(NoopStubSink),
    };
    let request = CreateTerminalRequest::new(SessionId::new("sess_agent"), "/bin/echo");
    handle_create_terminal(&context, request)
        .await
        .expect_err("start failure must surface as an error");

    // The row must not be left `pending`: the handler finalizes it as
    // `failed` before returning.
    let commands = state
        .lock()
        .await
        .query_commands(crate::state::CommandFilter {
            limit: 10,
            ..Default::default()
        })
        .expect("query commands");
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].status, "failed");
}

#[tokio::test]
async fn kill_finalizes_command_row_as_canceled() {
    use agent_client_protocol::schema::v1::SessionId;

    let state_dir = tempfile::tempdir().expect("tempdir");
    let store = StateStore::open(state_dir.path().join("state.sqlite")).expect("state open");
    store.migrate().expect("migrate");
    let state = Arc::new(TokioMutex::new(store));

    let context = TerminalHandlerContext {
        registry: Arc::new(TerminalRegistry::default()),
        workspace_root: std::env::temp_dir(),
        sandbox: crate::config::SandboxConfig::default(),
        network_provider: None,
        command_log: Some(TerminalCommandLog {
            state: state.clone(),
            event_hub: EventHub::new(),
        }),
        sink: Arc::new(NoopStubSink),
    };
    let request = CreateTerminalRequest::new(SessionId::new("sess_agent"), "/bin/sh")
        .args(vec!["-c".to_owned(), "sleep 30".to_owned()]);
    let response = handle_create_terminal(&context, request)
        .await
        .expect("terminal created");
    let handle = context
        .registry
        .get("sess_agent", &response.terminal_id.0)
        .await
        .expect("handle");

    handle.request_kill(Duration::from_millis(200)).await;
    let status = handle.wait_for_exit().await;
    assert_eq!(status.signal.as_deref(), Some("SIGTERM"));

    // Kill-intent exits finalize as `canceled` with no exit status,
    // matching the gateway's operator-cancel mapping; the ACP-side
    // TerminalExitStatus above still carries the signal.
    let guard = state.lock().await;
    let commands = guard
        .query_commands(crate::state::CommandFilter {
            limit: 10,
            ..Default::default()
        })
        .expect("query commands");
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].status, "canceled");
    assert_eq!(commands[0].exit_status, None);
    let events = guard
        .query_events(crate::state::LogFilter {
            limit: 10,
            kind: Some("command.canceled"),
            ..Default::default()
        })
        .expect("query events");
    assert_eq!(events.len(), 1, "expected one command.canceled event");
}

#[tokio::test]
async fn closed_registry_rejects_registration_and_kills_child() {
    let cwd = std::env::temp_dir();
    let resolved = crate::runtime::mediation::commands::policy::resolve_cwd_under_workspace(
        &cwd,
        &cwd.to_string_lossy(),
    )
    .expect("resolve cwd");
    let child = crate::runtime::mediation::commands::exec::spawn_child(
        std::path::Path::new("/bin/sh"),
        &["-c".to_owned(), "sleep 30".to_owned()],
        &resolved,
        None,
        &crate::config::SandboxConfig::default(),
        None,
    )
    .expect("spawn");
    let pid = child.id().expect("child pid") as i32;

    let registry = Arc::new(TerminalRegistry::default());
    registry.drain_all().await;

    let registered = registry
        .register("sess_test", child, DEFAULT_TERMINAL_OUTPUT_BYTE_LIMIT, None)
        .await;
    assert!(
        registered.is_none(),
        "closed registry must refuse registration"
    );
    // register() reaps the child before returning None, so the pid must
    // already be gone (ESRCH) — a live process here is the shutdown
    // orphan this path exists to prevent.
    let alive = unsafe { libc::kill(pid, 0) } == 0;
    assert!(!alive, "child survived closed-registry registration");
}
