use acp_stack::runtime::agent::acp_bridge::{AcpBridge, AcpPermissionPolicy};

use crate::support::{fake_agent_config, fake_env, null_sink};

#[tokio::test]
async fn spawn_completes_initialize_and_captures_capabilities() {
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
    .expect("placebo accepted clientInfo");
    bridge.shutdown().await.expect("shutdown ok");
}

#[tokio::test]
async fn spawn_rejects_an_incompatible_protocol_version() {
    let mut config = fake_agent_config();
    config.args.push("--initialize-protocol-v0".into());
    let error = match AcpBridge::spawn(
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
        AcpPermissionPolicy::Cancel,
        &Default::default(),
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
