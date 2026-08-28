use acp_stack::runtime::agent::acp_bridge::{AcpBridge, AcpPermissionPolicy};

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
        None,
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
        &std::env::temp_dir(),
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
        &std::env::temp_dir(),
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
        None,
        None,
    )
    .await
    .expect("spawn");
    assert!(!bridge.capabilities().supports_delete_session());

    let err = bridge
        .delete_session(agent_client_protocol::schema::v1::SessionId::new(
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
        None,
        None,
    )
    .await
    .expect("spawn");
    assert!(bridge.capabilities().supports_delete_session());

    bridge
        .delete_session(agent_client_protocol::schema::v1::SessionId::new(
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
        None,
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
        &std::env::temp_dir(),
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
        &std::env::temp_dir(),
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
        &std::env::temp_dir(),
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
