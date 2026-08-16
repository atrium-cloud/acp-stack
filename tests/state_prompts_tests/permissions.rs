use acp_stack::state::{CommandOrigin, NewCommandRecord, NewPermissionRequest, PermissionStatus};
use rusqlite::Connection;
use rusqlite::params;

use crate::common::state::fresh_state;

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
fn reconcile_orphaned_commands_settles_dependent_permissions() {
    let (_dir, store) = fresh_state("cmd_reconcile.sqlite");
    let command = store
        .append_command(NewCommandRecord {
            command: "sudo true",
            cwd: None,
            env_json: None,
            origin: CommandOrigin::Operator,
            session_id: None,
        })
        .expect("command row");
    let requester = format!("command:{}", command.id);
    let dependent = store
        .append_permission_request(NewPermissionRequest {
            source: "command",
            requester: Some(&requester),
            subject_id: Some(&command.id),
            detail_json: "{}",
            expires_at: None,
        })
        .expect("dependent permission");
    // An ACP-source pending row belongs to the permission sweep, not the
    // command sweep — it must survive untouched.
    let acp_pending = store
        .append_permission_request(NewPermissionRequest {
            source: "acp",
            requester: Some("sess_a"),
            subject_id: Some("sess_a"),
            detail_json: "{}",
            expires_at: None,
        })
        .expect("acp permission");

    let (reconciled, permissions_canceled) =
        store.reconcile_orphaned_commands().expect("reconcile");
    assert_eq!(reconciled, vec![command.id.clone()]);
    assert_eq!(permissions_canceled, 1);

    let command_row = store
        .get_command(&command.id)
        .expect("get command")
        .expect("command row");
    assert_eq!(command_row.status, "failed");

    let dependent_row = store
        .get_permission_request(&dependent.id)
        .expect("get")
        .expect("row");
    assert_eq!(dependent_row.status, "canceled");

    let acp_row = store
        .get_permission_request(&acp_pending.id)
        .expect("get")
        .expect("row");
    assert_eq!(acp_row.status, "pending");

    let connection = Connection::open(store.path()).expect("open sqlite directly");
    let reason: String = connection
        .query_row(
            "SELECT reason FROM permission_decisions WHERE request_id = ?1",
            params![dependent.id],
            |row| row.get(0),
        )
        .expect("decision row");
    assert_eq!(reason, "command-reconciled");

    // Invariant the incident violated: after the command sweep alone (no
    // permission sweep), no failed command may leave an approvable
    // command-source permission behind.
    let orphaned: i64 = connection
        .query_row(
            r#"
            SELECT COUNT(*)
            FROM commands c
            JOIN permission_requests p ON p.subject_id = c.id AND p.source = 'command'
            WHERE c.status = 'failed' AND p.status = 'pending'
            "#,
            [],
            |row| row.get(0),
        )
        .expect("invariant query");
    assert_eq!(orphaned, 0);
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
