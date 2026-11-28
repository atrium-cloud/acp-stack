use acp_stack::runtime::agent::acp_bridge::{AcpBridge, AcpPermissionPolicy, ForkPoint};

use crate::support::{fake_agent_config, fake_env, null_sink};

#[tokio::test]
async fn list_sessions_returns_agent_sessions() {
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
    .expect("spawn");
    assert!(!bridge.capabilities().supports_load_session());

    let err = bridge
        .load_session(
            agent_client_protocol::v1::schema::SessionId::new("sess_does_not_exist"),
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
    .expect("spawn");

    let err = bridge
        .resume_session(
            agent_client_protocol::v1::schema::SessionId::new("sess_does_not_exist"),
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
    .expect("spawn");

    let err = bridge
        .close_session(agent_client_protocol::v1::schema::SessionId::new(
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
async fn delete_session_returns_unsupported_capability_when_agent_disables_flag() {
    let mut config = fake_agent_config();
    config.args.push("--no-cap-delete-session".into());
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
    .expect("spawn");
    assert!(!bridge.capabilities().supports_delete_session());

    let err = bridge
        .delete_session(agent_client_protocol::v1::schema::SessionId::new(
            "sess_does_not_exist",
        ))
        .await
        .expect_err("must report unsupported capability");
    assert!(matches!(
        err,
        acp_stack::error::StackError::AgentUnsupportedCapability {
            name: "session/delete"
        }
    ));

    bridge.shutdown().await.expect("shutdown ok");
}

#[tokio::test]
async fn delete_session_round_trips_when_the_agent_advertises_the_capability() {
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
    assert!(bridge.capabilities().supports_delete_session());

    bridge
        .delete_session(agent_client_protocol::v1::schema::SessionId::new(
            "sess_fake_1",
        ))
        .await
        .expect("delete session");

    bridge.shutdown().await.expect("shutdown ok");
}

#[tokio::test]
async fn fork_session_returns_child_session() {
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
    assert!(bridge.capabilities().supports_fork_session());

    let (fork, _) = bridge
        .fork_session(
            agent_client_protocol::v1::schema::SessionId::new("sess_parent"),
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
    .expect("spawn");

    let err = bridge
        .fork_session(
            agent_client_protocol::v1::schema::SessionId::new("sess_parent"),
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
async fn fork_session_sends_a_cursor_as_the_standard_cursor_id() {
    let record = tempfile::tempdir().expect("record dir");
    let record_path = record.path().join("forks.jsonl");
    let mut config = fake_agent_config();
    config
        .args
        .extend(["--fork-record".into(), record_path.display().to_string()]);
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
    .expect("spawn");

    bridge
        .fork_session(
            agent_client_protocol::v1::schema::SessionId::new("sess_parent"),
            std::env::temp_dir(),
            vec![],
            Some(ForkPoint::Cursor("cursor_prompt_2".to_owned())),
        )
        .await
        .expect("session/fork at a cursor");
    let recorded: serde_json::Value = serde_json::from_str(
        std::fs::read_to_string(&record_path)
            .expect("fork record")
            .trim(),
    )
    .expect("fork record json");
    assert_eq!(
        recorded,
        serde_json::json!({ "cursorId": "cursor_prompt_2", "airMessageId": null })
    );
    bridge.shutdown().await.expect("shutdown ok");
}

#[tokio::test]
async fn fork_session_sends_an_air_fork_point_on_the_catalog_declaration_alone() {
    // The vendor adapters advertise nothing about the AIR key, so the fork point
    // has to travel on the catalog declaration alone.
    let mut config = fake_agent_config();
    config.args.extend([
        "--expect-air-fork-message-id".into(),
        "msg_previous_turn".into(),
    ]);
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
    .expect("spawn");

    let (fork, _) = bridge
        .fork_session(
            agent_client_protocol::v1::schema::SessionId::new("sess_parent"),
            std::env::temp_dir(),
            vec![],
            Some(ForkPoint::AirMessageId("msg_previous_turn".to_owned())),
        )
        .await
        .expect("session/fork with an AIR fork point");
    assert_eq!(fork.session_id.0.as_ref(), "sess_fake_0");
    bridge.shutdown().await.expect("shutdown ok");
}
