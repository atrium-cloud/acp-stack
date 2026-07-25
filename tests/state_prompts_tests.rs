use acp_stack::state::{
    FailureClass, NewPermissionRequest, NewPromptRecord, NewSessionRecord, PermissionStatus,
    PromptStatus, SESSION_STATUS_CLOSED, StateStore,
};
use rusqlite::Connection;
use rusqlite::params;
use std::str::FromStr;

mod common;
use common::state::{
    STALE_REASON, STALE_THRESHOLD_SECS, fresh_state, insert_state_test_session,
    seed_running_prompt_at,
};

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

#[test]
fn permission_request_lifecycle_pending_to_approved() {
    let (_dir, store) = fresh_state("perms.sqlite");
    let record = store
        .append_permission_request(NewPermissionRequest {
            source: "command",
            requester: Some("test-suite"),
            subject_id: Some("cmd_x"),
            detail_json: "{\"reason\":\"unit-test\"}",
            expires_at: None,
        })
        .expect("append");
    assert_eq!(record.status, "pending");

    let previous = store
        .transition_permission_status(&record.id, PermissionStatus::Approved)
        .expect("transition");
    assert_eq!(previous, PermissionStatus::Pending);

    let view = store
        .get_permission_request(&record.id)
        .expect("get")
        .expect("row");
    assert_eq!(view.status, "approved");
}

#[test]
fn permission_transition_terminal_to_other_is_rejected() {
    let (_dir, store) = fresh_state("perms.sqlite");
    let record = store
        .append_permission_request(NewPermissionRequest {
            source: "command",
            requester: None,
            subject_id: None,
            detail_json: "{}",
            expires_at: None,
        })
        .expect("append");
    store
        .transition_permission_status(&record.id, PermissionStatus::Denied)
        .expect("first transition");

    let error = store
        .transition_permission_status(&record.id, PermissionStatus::Approved)
        .expect_err("must reject terminal->approved");
    assert!(error.to_string().contains("cannot transition"), "{error}");
}

#[test]
fn permission_reconcile_orphans_categorizes_by_source() {
    let (_dir, store) = fresh_state("perms.sqlite");
    let acp_pending = store
        .append_permission_request(NewPermissionRequest {
            source: "acp",
            requester: Some("sess_a"),
            subject_id: Some("sess_a"),
            detail_json: "{}",
            expires_at: None,
        })
        .expect("acp row");
    let cmd_pending = store
        .append_permission_request(NewPermissionRequest {
            source: "command",
            requester: Some("cmd_a"),
            subject_id: Some("cmd_a"),
            detail_json: "{}",
            expires_at: None,
        })
        .expect("cmd row");

    let (canceled, expired) = store.reconcile_orphaned_permissions().expect("reconcile");
    assert_eq!(canceled, 1);
    assert_eq!(expired, 1);

    let after_acp = store
        .get_permission_request(&acp_pending.id)
        .expect("get")
        .expect("row");
    assert_eq!(after_acp.status, "canceled");

    let after_cmd = store
        .get_permission_request(&cmd_pending.id)
        .expect("get")
        .expect("row");
    assert_eq!(after_cmd.status, "expired");

    // Audit-trail invariant: every terminal request row must have a matching
    // permission_decisions row. Reconcile must insert these to honor the
    // same contract `decide_permission` upholds during normal operation.
    let counts = store.counts().expect("counts");
    assert_eq!(counts.permission_decisions, 2, "expected 2 decision rows");
}

#[test]
fn permission_decisions_persist_with_principal() {
    let (_dir, store) = fresh_state("perms.sqlite");
    let request = store
        .append_permission_request(NewPermissionRequest {
            source: "command",
            requester: None,
            subject_id: None,
            detail_json: "{}",
            expires_at: None,
        })
        .expect("append");
    let decision = store
        .record_permission_decision(
            &request.id,
            PermissionStatus::Approved,
            Some("session-key"),
            Some("operator"),
        )
        .expect("decision");
    assert_eq!(decision.request_id, request.id);
    assert_eq!(decision.decision, "approved");
    assert_eq!(decision.deciding_principal.as_deref(), Some("session-key"));
}

#[test]
fn prompt_status_helpers_round_trip_stalled() {
    assert_eq!(PromptStatus::Stalled.as_str(), "stalled");
    assert_eq!(
        PromptStatus::from_str("stalled").expect("stalled should parse"),
        PromptStatus::Stalled,
    );
    assert!(PromptStatus::Stalled.terminal());
    assert!(PromptStatus::Completed.terminal());
    assert!(PromptStatus::Errored.terminal());
    assert!(PromptStatus::Cancelled.terminal());
    assert!(!PromptStatus::Pending.terminal());
    assert!(!PromptStatus::Running.terminal());
    assert!(PromptStatus::from_str("not_a_status").is_err());
}

#[test]
fn failure_class_round_trips_taxonomy_strings() {
    let pairs = [
        (FailureClass::AgentRequest, "agent_request"),
        (FailureClass::Inference5xx, "inference_5xx"),
        (FailureClass::Inference4xx, "inference_4xx"),
        (FailureClass::Vm, "vm"),
        (FailureClass::Sqlite, "sqlite"),
        (FailureClass::Daemon, "daemon"),
        (FailureClass::AgentProcess, "agent_process"),
        (FailureClass::Stalled, "stalled"),
    ];
    for (variant, expected) in pairs {
        assert_eq!(variant.as_str(), expected);
        assert_eq!(
            FailureClass::from_str(expected).expect("taxonomy should parse"),
            variant,
        );
    }
    assert!(FailureClass::from_str("unknown").is_err());
}

#[test]
fn prompt_update_persists_stalled_with_failure_class_and_detail() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    store
        .insert_session(NewSessionRecord {
            id: "sess_stalled".to_owned(),
            agent_id: "fake".to_owned(),
            cwd: "/tmp/stalled".to_owned(),
            title: None,
            metadata_json: "{}".to_owned(),
        })
        .expect("session inserted");
    store
        .insert_prompt(NewPromptRecord {
            id: "prm_stalled".to_owned(),
            session_id: "sess_stalled".to_owned(),
            prompt_json: "[]".to_owned(),
        })
        .expect("prompt inserted");

    let detail = r#"{"reason":"threshold_exceeded"}"#;
    store
        .update_prompt_status(
            "prm_stalled",
            PromptStatus::Stalled,
            None,
            None,
            None,
            Some(FailureClass::Stalled.as_str()),
            Some(detail),
        )
        .expect("prompt status updated to stalled");

    let prompt = store
        .get_prompt("prm_stalled")
        .expect("prompt lookup")
        .expect("prompt exists");
    assert_eq!(prompt.status, PromptStatus::Stalled.as_str());
    assert_eq!(prompt.failure_class.as_deref(), Some("stalled"));
    assert_eq!(prompt.failure_detail_json.as_deref(), Some(detail));
}

#[test]
fn prompt_update_persists_inference_5xx_failure_taxonomy() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    store
        .insert_session(NewSessionRecord {
            id: "sess_5xx".to_owned(),
            agent_id: "fake".to_owned(),
            cwd: "/tmp/5xx".to_owned(),
            title: None,
            metadata_json: "{}".to_owned(),
        })
        .expect("session inserted");
    store
        .insert_prompt(NewPromptRecord {
            id: "prm_5xx".to_owned(),
            session_id: "sess_5xx".to_owned(),
            prompt_json: "[]".to_owned(),
        })
        .expect("prompt inserted");

    let detail = r#"{"upstream_status":502,"provider":"acme"}"#;
    store
        .update_prompt_status(
            "prm_5xx",
            PromptStatus::Errored,
            None,
            Some("inference.upstream"),
            Some("upstream returned 502"),
            Some(FailureClass::Inference5xx.as_str()),
            Some(detail),
        )
        .expect("prompt status updated to errored");

    let prompt = store
        .get_prompt("prm_5xx")
        .expect("prompt lookup")
        .expect("prompt exists");
    assert_eq!(prompt.status, PromptStatus::Errored.as_str());
    assert_eq!(prompt.failure_class.as_deref(), Some("inference_5xx"));
    assert_eq!(prompt.failure_detail_json.as_deref(), Some(detail));
    assert_eq!(prompt.error_code.as_deref(), Some("inference.upstream"));
}

#[test]
fn prompt_update_preserves_taxonomy_when_called_with_none() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    store
        .insert_session(NewSessionRecord {
            id: "sess_preserve".to_owned(),
            agent_id: "fake".to_owned(),
            cwd: "/tmp/preserve".to_owned(),
            title: None,
            metadata_json: "{}".to_owned(),
        })
        .expect("session inserted");
    store
        .insert_prompt(NewPromptRecord {
            id: "prm_preserve".to_owned(),
            session_id: "sess_preserve".to_owned(),
            prompt_json: "[]".to_owned(),
        })
        .expect("prompt inserted");

    // First write (non-terminal) sets the taxonomy; second write transitions
    // to a terminal status with None on both taxonomy params and must NOT
    // clobber the existing failure_class / failure_detail_json. The terminal
    // write is the only one that lands on already-set rows in production —
    // the supervisor sets a running-state taxonomy and then settles once.
    store
        .update_prompt_status(
            "prm_preserve",
            PromptStatus::Running,
            None,
            Some("vm.boom"),
            Some("vm crashed"),
            Some(FailureClass::Vm.as_str()),
            Some(r#"{"node":"vm-1"}"#),
        )
        .expect("first update");
    store
        .update_prompt_status(
            "prm_preserve",
            PromptStatus::Errored,
            None,
            Some("vm.boom"),
            Some("vm crashed (settle pass)"),
            None,
            None,
        )
        .expect("second update");

    let prompt = store
        .get_prompt("prm_preserve")
        .expect("prompt lookup")
        .expect("prompt exists");
    assert_eq!(prompt.status, PromptStatus::Errored.as_str());
    assert_eq!(prompt.failure_class.as_deref(), Some("vm"));
    assert_eq!(
        prompt.failure_detail_json.as_deref(),
        Some(r#"{"node":"vm-1"}"#)
    );
    assert_eq!(
        prompt.error_message.as_deref(),
        Some("vm crashed (settle pass)")
    );
}

#[test]
fn prompt_update_clears_taxonomy_with_empty_string_sentinel() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    store
        .insert_session(NewSessionRecord {
            id: "sess_clear".to_owned(),
            agent_id: "fake".to_owned(),
            cwd: "/tmp/clear".to_owned(),
            title: None,
            metadata_json: "{}".to_owned(),
        })
        .expect("session inserted");
    store
        .insert_prompt(NewPromptRecord {
            id: "prm_clear".to_owned(),
            session_id: "sess_clear".to_owned(),
            prompt_json: "[]".to_owned(),
        })
        .expect("prompt inserted");

    // First write (non-terminal) sets the taxonomy; second write transitions
    // to terminal with Some("") for both taxonomy params and must clear them.
    store
        .update_prompt_status(
            "prm_clear",
            PromptStatus::Running,
            None,
            None,
            None,
            Some(FailureClass::Daemon.as_str()),
            Some(r#"{"k":"v"}"#),
        )
        .expect("first update sets taxonomy");
    store
        .update_prompt_status(
            "prm_clear",
            PromptStatus::Errored,
            None,
            None,
            None,
            Some(""),
            Some(""),
        )
        .expect("second update clears taxonomy");

    let prompt = store
        .get_prompt("prm_clear")
        .expect("prompt lookup")
        .expect("prompt exists");
    assert_eq!(prompt.status, PromptStatus::Errored.as_str());
    assert!(prompt.failure_class.is_none());
    assert!(prompt.failure_detail_json.is_none());
}

#[test]
fn mark_stalled_prompts_flips_only_aged_rows() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    // Old row: well past the threshold. Fresh row: minted right before
    // the sweep; its `updated_at` will be roughly "now" so the comparison
    // against `now - 60s` keeps it as running.
    let aged = "2020-01-01T00:00:00.000000000Z";
    seed_running_prompt_at(&store, "sess_aged", "prm_aged", aged);

    store
        .insert_session(NewSessionRecord {
            id: "sess_fresh".to_owned(),
            agent_id: "fake".to_owned(),
            cwd: "/tmp".to_owned(),
            title: None,
            metadata_json: "{}".to_owned(),
        })
        .expect("session inserted");
    store
        .insert_prompt(NewPromptRecord {
            id: "prm_fresh".to_owned(),
            session_id: "sess_fresh".to_owned(),
            prompt_json: "[]".to_owned(),
        })
        .expect("prompt inserted");
    store
        .update_prompt_status(
            "prm_fresh",
            PromptStatus::Running,
            None,
            None,
            None,
            None,
            None,
        )
        .expect("prompt flipped to running");

    let pairs = store
        .mark_stalled_prompts(
            std::time::Duration::from_secs(STALE_THRESHOLD_SECS),
            STALE_REASON,
        )
        .expect("mark_stalled_prompts should run");

    assert_eq!(pairs.len(), 1);
    assert_eq!(pairs[0].0, "prm_aged");
    assert_eq!(pairs[0].1, "sess_aged");

    let aged_row = store
        .get_prompt("prm_aged")
        .expect("prompt lookup")
        .expect("prompt exists");
    assert_eq!(aged_row.status, "stalled");
    assert_eq!(aged_row.failure_class.as_deref(), Some("stalled"));
    assert_eq!(aged_row.error_code.as_deref(), Some("prompt.stalled"));
    assert_eq!(aged_row.error_message.as_deref(), Some(STALE_REASON));

    let fresh_row = store
        .get_prompt("prm_fresh")
        .expect("prompt lookup")
        .expect("prompt exists");
    assert_eq!(fresh_row.status, "running");
    assert!(fresh_row.failure_class.is_none());
}

#[test]
fn mark_stalled_prompts_is_idempotent() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    let aged = "2020-01-01T00:00:00.000000000Z";
    seed_running_prompt_at(&store, "sess_aged", "prm_aged", aged);

    let first = store
        .mark_stalled_prompts(
            std::time::Duration::from_secs(STALE_THRESHOLD_SECS),
            STALE_REASON,
        )
        .expect("mark_stalled_prompts should run");
    assert_eq!(first.len(), 1);

    let second = store
        .mark_stalled_prompts(
            std::time::Duration::from_secs(STALE_THRESHOLD_SECS),
            STALE_REASON,
        )
        .expect("second mark_stalled_prompts should run");
    assert!(
        second.is_empty(),
        "stalled rows must not be re-flipped on subsequent sweeps, got {second:?}"
    );
}

#[test]
fn mark_stalled_prompts_leaves_terminal_rows_alone() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    // Seed three terminal rows aged past the threshold. The sweep must
    // not touch any of them — once a prompt is settled, the durable
    // status (`completed`, `errored`, `cancelled`) is the source of
    // truth.
    let aged = "2020-01-01T00:00:00.000000000Z";
    for (session_id, prompt_id, terminal) in [
        ("sess_done", "prm_done", PromptStatus::Completed),
        ("sess_err", "prm_err", PromptStatus::Errored),
        ("sess_cancel", "prm_cancel", PromptStatus::Cancelled),
    ] {
        store
            .insert_session(NewSessionRecord {
                id: session_id.to_owned(),
                agent_id: "fake".to_owned(),
                cwd: "/tmp".to_owned(),
                title: None,
                metadata_json: "{}".to_owned(),
            })
            .expect("session inserted");
        store
            .insert_prompt(NewPromptRecord {
                id: prompt_id.to_owned(),
                session_id: session_id.to_owned(),
                prompt_json: "[]".to_owned(),
            })
            .expect("prompt inserted");
        store
            .update_prompt_status(prompt_id, terminal, None, None, None, None, None)
            .expect("prompt flipped to terminal");
        let connection =
            Connection::open(store.path()).expect("open sqlite directly for updated_at override");
        connection
            .execute(
                "UPDATE prompts SET updated_at = ?1 WHERE id = ?2",
                params![aged, prompt_id],
            )
            .expect("force-set updated_at");
    }

    let pairs = store
        .mark_stalled_prompts(
            std::time::Duration::from_secs(STALE_THRESHOLD_SECS),
            STALE_REASON,
        )
        .expect("mark_stalled_prompts should run");
    assert!(
        pairs.is_empty(),
        "terminal rows must not be flipped to stalled, got {pairs:?}"
    );

    for prompt_id in ["prm_done", "prm_err", "prm_cancel"] {
        let row = store
            .get_prompt(prompt_id)
            .expect("prompt lookup")
            .expect("prompt exists");
        assert_ne!(row.status, "stalled", "{prompt_id} must not flip");
    }
}

#[test]
fn update_prompt_status_is_noop_on_terminal_rows() {
    // Regression test for the sweeper/supervisor race: once a prompt is in any
    // terminal status (`completed | errored | cancelled | stalled`), a later
    // `update_prompt_status` call from the supervisor settle path must NOT
    // overwrite it. The WHERE guard inside `update_prompt_status` enforces
    // this; without it a slow ACP `prompt_session` future returning after the
    // sweeper had already flipped the row to `stalled` would race-erase the
    // stalled marker with `completed`/`errored`/`cancelled`.
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    store
        .insert_session(NewSessionRecord {
            id: "sess_race".to_owned(),
            agent_id: "fake".to_owned(),
            cwd: "/tmp".to_owned(),
            title: None,
            metadata_json: "{}".to_owned(),
        })
        .expect("session inserted");

    let cases = [
        (
            "prm_stalled_then_completed",
            PromptStatus::Stalled,
            PromptStatus::Completed,
        ),
        (
            "prm_stalled_then_errored",
            PromptStatus::Stalled,
            PromptStatus::Errored,
        ),
        (
            "prm_stalled_then_cancelled",
            PromptStatus::Stalled,
            PromptStatus::Cancelled,
        ),
        (
            "prm_completed_then_errored",
            PromptStatus::Completed,
            PromptStatus::Errored,
        ),
    ];

    for (prompt_id, first, second) in cases {
        store
            .insert_prompt(NewPromptRecord {
                id: prompt_id.to_owned(),
                session_id: "sess_race".to_owned(),
                prompt_json: "[]".to_owned(),
            })
            .expect("prompt inserted");
        let first_applied = store
            .update_prompt_status(
                prompt_id,
                first,
                None,
                Some("first.code"),
                Some("first message"),
                Some(FailureClass::Stalled.as_str()),
                None,
            )
            .expect("first terminal write");
        assert!(first_applied, "first terminal write should apply");
        // Second write is the supervisor late-settle. It should not return an
        // error (the row exists), but it must be a no-op on the data.
        let second_applied = store
            .update_prompt_status(
                prompt_id,
                second,
                Some("end_turn"),
                Some("second.code"),
                Some("second message"),
                Some(FailureClass::AgentRequest.as_str()),
                Some(r#"{"clobber":true}"#),
            )
            .expect("second write succeeds without error");
        assert!(
            !second_applied,
            "already-terminal prompt update should report no-op"
        );
        let row = store
            .get_prompt(prompt_id)
            .expect("prompt lookup")
            .expect("prompt exists");
        assert_eq!(
            row.status,
            first.as_str(),
            "{prompt_id} must keep its first terminal status"
        );
        assert_eq!(row.error_code.as_deref(), Some("first.code"));
        assert_eq!(row.error_message.as_deref(), Some("first message"));
        assert_eq!(
            row.failure_class.as_deref(),
            Some(FailureClass::Stalled.as_str())
        );
    }

    // PromptNotFound is still surfaced when the row truly does not exist —
    // the no-op handling must not mask the missing-row case.
    let missing = store.update_prompt_status(
        "prm_does_not_exist",
        PromptStatus::Completed,
        None,
        None,
        None,
        None,
        None,
    );
    match missing {
        Err(acp_stack::error::StackError::PromptNotFound { id }) => {
            assert_eq!(id, "prm_does_not_exist");
        }
        other => panic!("expected PromptNotFound, got {other:?}"),
    }
}

#[test]
fn count_stuck_prompts_returns_count_and_oldest_updated_at() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    // No stuck rows yet.
    let (count, oldest) = store
        .count_stuck_prompts(std::time::Duration::from_secs(STALE_THRESHOLD_SECS))
        .expect("count_stuck_prompts should run");
    assert_eq!(count, 0);
    assert!(oldest.is_none());

    let aged_older = "2019-01-01T00:00:00.000000000Z";
    let aged_newer = "2020-01-01T00:00:00.000000000Z";
    seed_running_prompt_at(&store, "sess_a", "prm_a", aged_older);
    seed_running_prompt_at(&store, "sess_b", "prm_b", aged_newer);

    let (count, oldest) = store
        .count_stuck_prompts(std::time::Duration::from_secs(STALE_THRESHOLD_SECS))
        .expect("count_stuck_prompts should run");
    assert_eq!(count, 2);
    assert_eq!(oldest.as_deref(), Some(aged_older));
}
