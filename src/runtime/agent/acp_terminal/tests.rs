use super::*;

/// Stands in for `[workspace].default_shell` in contexts whose subject is not
/// the shell choice itself.
const TEST_SHELL: &str = "/bin/sh";

#[test]
fn keep_newest_retains_tail_at_char_boundary() {
    assert_eq!(keep_newest("hello", 10), ("hello", false));
    assert_eq!(keep_newest("hello", 5), ("hello", false));
    assert_eq!(keep_newest("hello", 3), ("llo", true));
    // A cutoff landing mid-char rounds UP past it, so the result stays within
    // the limit and on a boundary.
    let (kept, truncated) = keep_newest("héllo", 4);
    assert!(truncated);
    assert_eq!(kept, "llo");
    assert!(kept.len() as u64 <= 4);
    let (kept, truncated) = keep_newest("🚀🚀🚀", 6);
    assert!(truncated);
    assert_eq!(kept, "🚀");
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
    assert!(buffer.data.contains("chunk-099"));
    assert!(!buffer.data.contains("chunk-000"));
}

#[test]
fn terminal_environment_excludes_provider_keys() {
    let agent_env = vec![EnvVariable::new("MY_FLAG", "1")];
    let env = terminal_environment(Path::new("/home/acp-terminal-test"), &agent_env);
    assert_eq!(env.get("MY_FLAG").map(String::as_str), Some("1"));
    // Composition starts from an empty map, never the agent process env,
    // which carries provider API keys.
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

    // The owner drains the pipes before publishing exit, so output must be
    // visible the moment wait_for_exit resolves. Polling here would mask a
    // drain regression.
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
        home: root.path().to_path_buf(),
        sandbox: crate::config::SandboxConfig::default(),
        shell: TEST_SHELL.to_owned(),
        network_provider: None,
        command_log: None,
        sink: Arc::new(CwdStubSink {
            cwd: session_dir.to_string_lossy().into_owned(),
        }),
    };
    // No cwd on the request: the handler falls back to the session's recorded
    // cwd, not the workspace root.
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
    // Block only the pending -> running transition, so the INSERT and the
    // failure finalization still succeed.
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
        home: std::env::temp_dir(),
        sandbox: crate::config::SandboxConfig::default(),
        shell: TEST_SHELL.to_owned(),
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
        home: std::env::temp_dir(),
        sandbox: crate::config::SandboxConfig::default(),
        shell: TEST_SHELL.to_owned(),
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

    // Kill-intent exits finalize as `cancelled` with no exit status, matching
    // the gateway's operator-cancel mapping.
    let guard = state.lock().await;
    let commands = guard
        .query_commands(crate::state::CommandFilter {
            limit: 10,
            ..Default::default()
        })
        .expect("query commands");
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].status, "cancelled");
    assert_eq!(commands[0].exit_status, None);
    let events = guard
        .query_events(crate::state::LogFilter {
            limit: 10,
            kind: Some("command.cancelled"),
            ..Default::default()
        })
        .expect("query events");
    assert_eq!(events.len(), 1, "expected one command.cancelled event");
}

#[tokio::test]
async fn create_terminal_without_args_runs_the_command_through_a_shell() {
    use agent_client_protocol::schema::v1::SessionId;

    let root = tempfile::tempdir().expect("workspace root");
    let context = TerminalHandlerContext {
        registry: Arc::new(TerminalRegistry::default()),
        workspace_root: root.path().to_path_buf(),
        home: root.path().to_path_buf(),
        sandbox: crate::config::SandboxConfig::default(),
        shell: TEST_SHELL.to_owned(),
        network_provider: None,
        command_log: None,
        sink: Arc::new(NoopStubSink),
    };
    // Whole shell line in `command` with no argv, exactly what goose sends.
    let request =
        CreateTerminalRequest::new(SessionId::new("sess_agent"), "printf hello && printf world")
            .cwd(Some(root.path().to_path_buf()));
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
    assert_eq!(handle.buffer.lock().await.data, "helloworld");
}

#[tokio::test]
async fn create_terminal_with_args_execs_the_program_verbatim() {
    use agent_client_protocol::schema::v1::SessionId;

    let root = tempfile::tempdir().expect("workspace root");
    let context = TerminalHandlerContext {
        registry: Arc::new(TerminalRegistry::default()),
        workspace_root: root.path().to_path_buf(),
        home: root.path().to_path_buf(),
        sandbox: crate::config::SandboxConfig::default(),
        shell: TEST_SHELL.to_owned(),
        network_provider: None,
        command_log: None,
        sink: Arc::new(NoopStubSink),
    };
    // A shell would treat `&&` as an operator; exact exec passes it through as
    // one literal argument.
    let request = CreateTerminalRequest::new(SessionId::new("sess_agent"), "/bin/echo")
        .args(vec!["hello && echo world".to_owned()])
        .cwd(Some(root.path().to_path_buf()));
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
    assert_eq!(
        handle.buffer.lock().await.data.trim_end(),
        "hello && echo world"
    );
}

#[tokio::test]
async fn create_terminal_runs_the_configured_workspace_shell() {
    use agent_client_protocol::schema::v1::SessionId;
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().expect("workspace root");
    // A stand-in interpreter that reports the argv it was handed, so the
    // assertion pins the configured path rather than any system shell.
    let shell_path = root.path().join("marker-shell");
    std::fs::write(
        &shell_path,
        "#!/bin/sh\nprintf 'marker-shell ran: %s' \"$2\"\n",
    )
    .expect("write marker shell");
    std::fs::set_permissions(&shell_path, std::fs::Permissions::from_mode(0o755))
        .expect("mark marker shell executable");

    let context = TerminalHandlerContext {
        registry: Arc::new(TerminalRegistry::default()),
        workspace_root: root.path().to_path_buf(),
        home: root.path().to_path_buf(),
        sandbox: crate::config::SandboxConfig::default(),
        shell: shell_path.to_string_lossy().into_owned(),
        network_provider: None,
        command_log: None,
        sink: Arc::new(NoopStubSink),
    };
    let request = CreateTerminalRequest::new(SessionId::new("sess_agent"), "printf ignored")
        .cwd(Some(root.path().to_path_buf()));
    let response = handle_create_terminal(&context, request)
        .await
        .expect("terminal created");

    let handle = context
        .registry
        .get("sess_agent", &response.terminal_id.0)
        .await
        .expect("handle");
    assert_eq!(handle.wait_for_exit().await.exit_code, Some(0));
    assert_eq!(
        handle.buffer.lock().await.data,
        "marker-shell ran: printf ignored"
    );
}

#[test]
fn terminal_invocation_selects_exec_form_from_argv_presence() {
    let (program, args) = terminal_invocation("/bin/bash", "printf hi", &[]);
    assert_eq!(program, PathBuf::from("/bin/bash"));
    assert_eq!(args, vec!["-c".to_owned(), "printf hi".to_owned()]);

    // A configured shell never displaces an agent-supplied argv.
    let (program, args) = terminal_invocation("/bin/bash", "/bin/echo", &["hi".to_owned()]);
    assert_eq!(program, PathBuf::from("/bin/echo"));
    assert_eq!(args, vec!["hi".to_owned()]);
}

#[tokio::test]
async fn create_terminal_rejects_a_blank_command() {
    use agent_client_protocol::schema::v1::SessionId;

    let state_dir = tempfile::tempdir().expect("tempdir");
    let store = StateStore::open(state_dir.path().join("state.sqlite")).expect("state open");
    store.migrate().expect("migrate");
    let state = Arc::new(TokioMutex::new(store));

    let root = tempfile::tempdir().expect("workspace root");
    let context = TerminalHandlerContext {
        registry: Arc::new(TerminalRegistry::default()),
        workspace_root: root.path().to_path_buf(),
        home: root.path().to_path_buf(),
        sandbox: crate::config::SandboxConfig::default(),
        shell: TEST_SHELL.to_owned(),
        network_provider: None,
        command_log: Some(TerminalCommandLog {
            state: state.clone(),
            event_hub: EventHub::new(),
        }),
        sink: Arc::new(NoopStubSink),
    };
    // Whitespace only: without the guard this would reach the shell as `-c ""`
    // and report a successful no-op run.
    let request = CreateTerminalRequest::new(SessionId::new("sess_agent"), "   ")
        .cwd(Some(root.path().to_path_buf()));
    let error = handle_create_terminal(&context, request)
        .await
        .expect_err("blank command must be refused");
    assert_eq!(error.code, AcpError::invalid_params().code);

    // Refused before any row or child exists, so nothing is left behind.
    let commands = state
        .lock()
        .await
        .query_commands(crate::state::CommandFilter {
            limit: 10,
            ..Default::default()
        })
        .expect("query commands");
    assert!(commands.is_empty(), "blank command must not log a row");
}

#[tokio::test]
async fn shell_wrapped_terminal_logs_the_agent_requested_command() {
    use agent_client_protocol::schema::v1::SessionId;

    let state_dir = tempfile::tempdir().expect("tempdir");
    let store = StateStore::open(state_dir.path().join("state.sqlite")).expect("state open");
    store.migrate().expect("migrate");
    let state = Arc::new(TokioMutex::new(store));

    let root = tempfile::tempdir().expect("workspace root");
    let context = TerminalHandlerContext {
        registry: Arc::new(TerminalRegistry::default()),
        workspace_root: root.path().to_path_buf(),
        home: root.path().to_path_buf(),
        sandbox: crate::config::SandboxConfig::default(),
        shell: TEST_SHELL.to_owned(),
        network_provider: None,
        command_log: Some(TerminalCommandLog {
            state: state.clone(),
            event_hub: EventHub::new(),
        }),
        sink: Arc::new(NoopStubSink),
    };
    let request = CreateTerminalRequest::new(SessionId::new("sess_agent"), "printf audited")
        .cwd(Some(root.path().to_path_buf()));
    let response = handle_create_terminal(&context, request)
        .await
        .expect("terminal created");
    context
        .registry
        .get("sess_agent", &response.terminal_id.0)
        .await
        .expect("handle")
        .wait_for_exit()
        .await;

    // The audit row is the agent's intent, not the interpreter the runtime
    // chose to run it under.
    let commands = state
        .lock()
        .await
        .query_commands(crate::state::CommandFilter {
            limit: 10,
            ..Default::default()
        })
        .expect("query commands");
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].command, "printf audited");
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
    // register() reaps the child before returning None, so a live process
    // here is the shutdown orphan this path exists to prevent.
    let alive = unsafe { libc::kill(pid, 0) } == 0;
    assert!(!alive, "child survived closed-registry registration");
}
