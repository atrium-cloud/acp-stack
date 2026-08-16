use acp_stack::state::{
    NewPermissionRequest, NewPromptRecord, NewSessionRecord, PromptStatus, SESSION_STATUS_CLOSED,
};

use crate::common::state::{fresh_state, insert_state_test_session};

#[test]
fn restart_blockers_include_pending_and_running_prompts() {
    let (_dir, store) = fresh_state("restart_blockers_prompts.sqlite");
    insert_state_test_session(&store, "sess_pending");
    insert_state_test_session(&store, "sess_running");
    store
        .insert_prompt(NewPromptRecord {
            id: "prm_pending".to_owned(),
            session_id: "sess_pending".to_owned(),
            prompt_json: "[]".to_owned(),
        })
        .expect("pending prompt");
    store
        .insert_prompt(NewPromptRecord {
            id: "prm_pending_second".to_owned(),
            session_id: "sess_pending".to_owned(),
            prompt_json: "[]".to_owned(),
        })
        .expect("second pending prompt");
    store
        .insert_prompt(NewPromptRecord {
            id: "prm_running".to_owned(),
            session_id: "sess_running".to_owned(),
            prompt_json: "[]".to_owned(),
        })
        .expect("running prompt");
    store
        .update_prompt_status(
            "prm_running",
            PromptStatus::Running,
            None,
            None,
            None,
            None,
            None,
        )
        .expect("mark running");

    let blockers = store
        .query_restart_blockers(Some("fake"))
        .expect("restart blockers");
    assert_eq!(blockers.len(), 3);
    assert!(blockers.iter().any(|row| {
        row.session_id == "sess_pending"
            && row.state == "prompt_sent"
            && row.prompt_id.as_deref() == Some("prm_pending")
    }));
    assert!(blockers.iter().any(|row| {
        row.session_id == "sess_pending"
            && row.state == "prompt_sent"
            && row.prompt_id.as_deref() == Some("prm_pending_second")
    }));
    assert!(blockers.iter().any(|row| {
        row.session_id == "sess_running"
            && row.state == "working"
            && row.prompt_id.as_deref() == Some("prm_running")
    }));
}

#[test]
fn restart_blockers_include_pending_acp_permissions() {
    let (_dir, store) = fresh_state("restart_blockers_permissions.sqlite");
    insert_state_test_session(&store, "sess_permission_blocker");
    let permission = store
        .append_permission_request(NewPermissionRequest {
            source: "acp",
            requester: Some("agent"),
            subject_id: Some("sess_permission_blocker"),
            detail_json: "{}",
            expires_at: None,
        })
        .expect("permission inserted");

    let blockers = store
        .query_restart_blockers(None)
        .expect("restart blockers");
    assert_eq!(blockers.len(), 1);
    assert_eq!(blockers[0].state, "permission_required");
    assert_eq!(
        blockers[0].permission_id.as_deref(),
        Some(permission.id.as_str())
    );
}

#[test]
fn restart_blockers_report_prompt_and_permission_for_same_session() {
    let (_dir, store) = fresh_state("restart_blockers_joint.sqlite");
    insert_state_test_session(&store, "sess_joint_blocker");
    store
        .insert_prompt(NewPromptRecord {
            id: "prm_joint".to_owned(),
            session_id: "sess_joint_blocker".to_owned(),
            prompt_json: "[]".to_owned(),
        })
        .expect("prompt inserted");
    let permission = store
        .append_permission_request(NewPermissionRequest {
            source: "acp",
            requester: Some("agent"),
            subject_id: Some("sess_joint_blocker"),
            detail_json: "{}",
            expires_at: None,
        })
        .expect("permission inserted");

    let blockers = store
        .query_restart_blockers(None)
        .expect("restart blockers");
    assert_eq!(blockers.len(), 2);
    assert!(blockers.iter().any(|row| {
        row.session_id == "sess_joint_blocker"
            && row.state == "prompt_sent"
            && row.prompt_id.as_deref() == Some("prm_joint")
    }));
    assert!(blockers.iter().any(|row| {
        row.session_id == "sess_joint_blocker"
            && row.state == "permission_required"
            && row.permission_id.as_deref() == Some(permission.id.as_str())
    }));
}

#[test]
fn pending_acp_permission_ids_for_target_returns_all_matching_rows() {
    let (_dir, store) = fresh_state("restart_permission_ids_target.sqlite");
    store
        .insert_session_for_target(
            "alpha",
            "sess_alpha_permissions".to_owned(),
            NewSessionRecord {
                id: "sess_alpha_permissions".to_owned(),
                agent_id: "alpha-agent".to_owned(),
                cwd: "/tmp/alpha".to_owned(),
                title: None,
                metadata_json: "{}".to_owned(),
            },
        )
        .expect("alpha session");
    store
        .insert_session_for_target(
            "alpha",
            "sess_alpha_closed_permissions".to_owned(),
            NewSessionRecord {
                id: "sess_alpha_closed_permissions".to_owned(),
                agent_id: "alpha-agent".to_owned(),
                cwd: "/tmp/alpha-closed".to_owned(),
                title: None,
                metadata_json: "{}".to_owned(),
            },
        )
        .expect("closed alpha session");
    store
        .update_session_status("sess_alpha_closed_permissions", SESSION_STATUS_CLOSED)
        .expect("closed alpha session status");
    store
        .insert_session_for_target(
            "beta",
            "sess_beta_permissions".to_owned(),
            NewSessionRecord {
                id: "sess_beta_permissions".to_owned(),
                agent_id: "beta-agent".to_owned(),
                cwd: "/tmp/beta".to_owned(),
                title: None,
                metadata_json: "{}".to_owned(),
            },
        )
        .expect("beta session");
    let first = store
        .append_permission_request(NewPermissionRequest {
            source: "acp",
            requester: Some("agent"),
            subject_id: Some("sess_alpha_permissions"),
            detail_json: "{}",
            expires_at: None,
        })
        .expect("first alpha permission");
    let second = store
        .append_permission_request(NewPermissionRequest {
            source: "acp",
            requester: Some("agent"),
            subject_id: Some("sess_alpha_permissions"),
            detail_json: "{}",
            expires_at: None,
        })
        .expect("second alpha permission");
    let closed = store
        .append_permission_request(NewPermissionRequest {
            source: "acp",
            requester: Some("agent"),
            subject_id: Some("sess_alpha_closed_permissions"),
            detail_json: "{}",
            expires_at: None,
        })
        .expect("closed alpha permission");
    store
        .append_permission_request(NewPermissionRequest {
            source: "command",
            requester: Some("agent"),
            subject_id: Some("sess_alpha_permissions"),
            detail_json: "{}",
            expires_at: None,
        })
        .expect("command permission");
    store
        .append_permission_request(NewPermissionRequest {
            source: "acp",
            requester: Some("agent"),
            subject_id: Some("sess_beta_permissions"),
            detail_json: "{}",
            expires_at: None,
        })
        .expect("beta permission");

    let mut ids = store
        .query_pending_acp_permission_ids_for_target("alpha")
        .expect("pending ACP permission ids");
    ids.sort();
    let mut expected = vec![first.id, second.id, closed.id];
    expected.sort();
    assert_eq!(ids, expected);
}

#[test]
fn restart_blockers_ignore_active_sessions_without_prompt() {
    let (_dir, store) = fresh_state("restart_blockers_idle.sqlite");
    insert_state_test_session(&store, "sess_idle");

    let blockers = store
        .query_restart_blockers(None)
        .expect("restart blockers");
    assert!(blockers.is_empty());
}

#[test]
fn restart_blockers_ignore_terminal_latest_prompts() {
    let (_dir, store) = fresh_state("restart_blockers_terminal.sqlite");
    insert_state_test_session(&store, "sess_terminal");
    store
        .insert_prompt(NewPromptRecord {
            id: "prm_terminal".to_owned(),
            session_id: "sess_terminal".to_owned(),
            prompt_json: "[]".to_owned(),
        })
        .expect("prompt inserted");
    store
        .update_prompt_status(
            "prm_terminal",
            PromptStatus::Completed,
            Some("end_turn"),
            None,
            None,
            None,
            None,
        )
        .expect("complete prompt");

    let blockers = store
        .query_restart_blockers(None)
        .expect("restart blockers");
    assert!(blockers.is_empty());
}

#[test]
fn restart_blockers_filter_by_target() {
    let (_dir, store) = fresh_state("restart_blockers_target.sqlite");
    store
        .insert_session_for_target(
            "alpha",
            "sess_alpha".to_owned(),
            NewSessionRecord {
                id: "sess_alpha".to_owned(),
                agent_id: "alpha-agent".to_owned(),
                cwd: "/tmp/alpha".to_owned(),
                title: None,
                metadata_json: "{}".to_owned(),
            },
        )
        .expect("alpha session");
    store
        .insert_session_for_target(
            "beta",
            "sess_beta".to_owned(),
            NewSessionRecord {
                id: "sess_beta".to_owned(),
                agent_id: "beta-agent".to_owned(),
                cwd: "/tmp/beta".to_owned(),
                title: None,
                metadata_json: "{}".to_owned(),
            },
        )
        .expect("beta session");
    for (session_id, prompt_id) in [("sess_alpha", "prm_alpha"), ("sess_beta", "prm_beta")] {
        store
            .insert_prompt(NewPromptRecord {
                id: prompt_id.to_owned(),
                session_id: session_id.to_owned(),
                prompt_json: "[]".to_owned(),
            })
            .expect("prompt inserted");
    }

    let blockers = store
        .query_restart_blockers(Some("beta"))
        .expect("restart blockers");
    assert_eq!(blockers.len(), 1);
    assert_eq!(blockers[0].session_id, "sess_beta");
}
