use acp_stack::runtime::agent::acp_bridge::{AcpBridge, AcpPermissionPolicy};

use crate::support::{fake_agent_config, fake_env, null_sink};

#[tokio::test]
async fn spawn_completes_initialize_and_captures_capabilities() {
    let bridge = AcpBridge::spawn(
        &std::env::temp_dir(),
        &fake_agent_config(),
        fake_env(),
        std::env::temp_dir(),
        null_sink(),
        AcpPermissionPolicy::Cancel,
        &Default::default(),
        "/bin/sh",
        None,
        None,
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
async fn spawn_sends_client_identity() {
    let mut config = fake_agent_config();
    config.args.push("--require-client-info".into());
    let bridge = AcpBridge::spawn(
        &std::env::temp_dir(),
        &config,
        fake_env(),
        std::env::temp_dir(),
        null_sink(),
        AcpPermissionPolicy::Cancel,
        &Default::default(),
        "/bin/sh",
        None,
        None,
        None,
    )
    .await
    .expect("placebo accepted clientInfo");
    bridge.shutdown().await.expect("shutdown ok");
}

async fn spawn_with_args(args: &[&str]) -> acp_stack::error::Result<AcpBridge> {
    let mut config = fake_agent_config();
    config.args.extend(args.iter().map(|arg| (*arg).to_owned()));
    AcpBridge::spawn(
        &std::env::temp_dir(),
        &config,
        fake_env(),
        std::env::temp_dir(),
        null_sink(),
        AcpPermissionPolicy::Cancel,
        &Default::default(),
        "/bin/sh",
        None,
        None,
        None,
    )
    .await
}

#[tokio::test]
async fn initialize_failure_carries_the_redacted_stderr_tail() {
    acp_stack::redaction::register_secret_values(["StderrCanary-5Tq81"]);
    let error = match spawn_with_args(&[
        "--stderr-lines",
        "2",
        "--stderr-echo",
        "boot failed with key StderrCanary-5Tq81",
        "--initialize-error",
    ])
    .await
    {
        Ok(bridge) => {
            bridge.shutdown().await.expect("shutdown ok");
            panic!("initialize must fail");
        }
        Err(error) => error,
    };
    assert_eq!(
        error.to_string(),
        "agent failed to initialize: fake initialize failure; agent stderr:\nplacebo stderr line 0\nplacebo stderr line 1\nboot failed with key [redacted]"
    );
}

// The per-line logging and rate limit are covered by the `agent_stderr` unit tests; a
// thread-local subscriber here races other tests for the shared callsite interest.
#[tokio::test]
async fn shutdown_keeps_the_last_stderr_lines_past_the_rate_limit() {
    let bridge = spawn_with_args(&["--stderr-lines", "205"])
        .await
        .expect("spawn");
    bridge.shutdown().await.expect("shutdown ok");

    assert!(
        bridge
            .stderr_tail()
            .ends_with("placebo stderr line 203\nplacebo stderr line 204")
    );
}

#[tokio::test]
async fn spawn_rejects_an_incompatible_protocol_version() {
    let mut config = fake_agent_config();
    config.args.push("--initialize-protocol-v0".into());
    let error = match AcpBridge::spawn(
        &std::env::temp_dir(),
        &config,
        fake_env(),
        std::env::temp_dir(),
        null_sink(),
        AcpPermissionPolicy::Cancel,
        &Default::default(),
        "/bin/sh",
        None,
        None,
        None,
    )
    .await
    {
        Ok(bridge) => {
            bridge.shutdown().await.expect("shutdown ok");
            panic!("protocol v0 must be rejected");
        }
        Err(error) => error,
    };
    assert!(matches!(
        error,
        acp_stack::error::StackError::AgentInitializeFailed { .. }
    ));
    assert!(error.to_string().contains("agent returned 0"), "{error}");
}

#[tokio::test]
async fn unadvertised_http_mcp_transport_is_skipped_not_fatal() {
    use agent_client_protocol::schema::v1::{McpServer, McpServerHttp};

    let bridge = AcpBridge::spawn(
        &std::env::temp_dir(),
        &fake_agent_config(),
        fake_env(),
        std::env::temp_dir(),
        null_sink(),
        AcpPermissionPolicy::Cancel,
        &Default::default(),
        "/bin/sh",
        None,
        None,
        None,
    )
    .await
    .expect("spawn");
    let partitioned = bridge
        .capabilities()
        .partition_mcp_servers(vec![McpServer::Http(McpServerHttp::new(
            "test-http",
            "https://example.invalid/mcp",
        ))])
        .expect("an unadvertised transport is skipped, not an error");
    assert!(partitioned.accepted.is_empty());
    assert_eq!(partitioned.skipped.len(), 1);
    assert_eq!(partitioned.skipped[0].name, "test-http");
    assert_eq!(partitioned.skipped[0].capability, "mcpCapabilities.http");

    bridge
        .new_session(std::env::temp_dir(), partitioned.accepted)
        .await
        .expect("session create survives the skipped server");
    bridge.shutdown().await.expect("shutdown ok");
}

#[tokio::test]
async fn shutdown_terminates_the_child() {
    let bridge = AcpBridge::spawn(
        &std::env::temp_dir(),
        &fake_agent_config(),
        fake_env(),
        std::env::temp_dir(),
        null_sink(),
        AcpPermissionPolicy::Cancel,
        &Default::default(),
        "/bin/sh",
        None,
        None,
        None,
    )
    .await
    .expect("spawn ok");
    let pid = bridge.pid().expect("pid available");
    bridge.shutdown().await.expect("shutdown ok");

    #[cfg(unix)]
    {
        // SAFETY: signal 0 is the standard "does this pid exist" probe; it delivers no signal.
        unsafe {
            let alive = libc::kill(pid as i32, 0);
            if alive == 0 {
                // The pid may have been reused; recheck after a beat before calling it a leak.
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
        &std::env::temp_dir(),
        &fake_agent_config(),
        fake_env(),
        std::env::temp_dir(),
        null_sink(),
        AcpPermissionPolicy::Cancel,
        &Default::default(),
        "/bin/sh",
        None,
        None,
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
    // The bridge forwards the caller-supplied home verbatim, so a temp path
    // keeps the child away from the developer's real HOME.
    let home_dir = tempfile::tempdir().expect("tempdir");
    let home = home_dir.path().to_string_lossy().into_owned();
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
        home_dir.path(),
        &config,
        env,
        std::env::temp_dir(),
        null_sink(),
        AcpPermissionPolicy::Cancel,
        &Default::default(),
        "/bin/sh",
        None,
        None,
        None,
    )
    .await
    .expect("bridge spawns");
    let caps = bridge.capabilities();
    assert_eq!(caps.agent_title.as_deref(), Some("env assertions passed"));
    assert_ne!(home, "secret-home");
    bridge.shutdown().await.expect("shutdown ok");
}
