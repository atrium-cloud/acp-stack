use std::time::Duration;

use acp_stack::state::{
    EVENT_SOURCE_ACP, FailureClass, NewPromptRecord, NewSessionRecord, PromptSettle,
    PromptSettlement, PromptStaleThresholds, PromptStatus, StateStore,
};
use chrono::{SecondsFormat, Utc};
use rusqlite::Connection;
use rusqlite::params;
use serde_json::json;

use crate::common::state::{
    STALE_REASON, STALE_THRESHOLD_SECS, fresh_state, insert_state_test_session,
    seed_running_prompt_at, uniform_stale_thresholds,
};

const OPEN_TOOL_CALL_THRESHOLD_SECS: u64 = 3_600;
/// Past `STALE_THRESHOLD_SECS`, inside `OPEN_TOOL_CALL_THRESHOLD_SECS`.
const QUIET_PAST_THRESHOLD_SECS: i64 = 120;
/// Past `OPEN_TOOL_CALL_THRESHOLD_SECS` too.
const TOOL_CALL_PAST_THRESHOLD_SECS: i64 = 7_200;
const TOOL_CALL_ID: &str = "tool_1";
/// Keeps rows of consecutive writes on distinct timestamps on coarse clocks.
const WRITE_GAP: Duration = Duration::from_millis(2);

fn stale_thresholds() -> PromptStaleThresholds {
    uniform_stale_thresholds(Duration::from_secs(STALE_THRESHOLD_SECS))
}

fn tool_call_thresholds() -> PromptStaleThresholds {
    PromptStaleThresholds {
        quiet: Duration::from_secs(STALE_THRESHOLD_SECS),
        open_tool_call: Duration::from_secs(OPEN_TOOL_CALL_THRESHOLD_SECS),
    }
}

fn seconds_ago(seconds: i64) -> String {
    (Utc::now() - chrono::Duration::seconds(seconds)).to_rfc3339_opts(SecondsFormat::Nanos, true)
}

/// Append an agent `session/update` row carrying `update`, the shape the
/// session sink persists.
fn append_agent_update(store: &StateStore, session_id: &str, update: serde_json::Value) {
    store
        .append_session_event_with_source(
            session_id,
            "info",
            "session.update",
            EVENT_SOURCE_ACP,
            "ACP session update",
            &json!({ "sessionId": "agent_session", "update": update }).to_string(),
        )
        .expect("append session update");
}

fn tool_call(status: Option<&str>) -> serde_json::Value {
    let mut update = json!({
        "sessionUpdate": "tool_call",
        "toolCallId": TOOL_CALL_ID,
        "title": "sleep 600",
        "kind": "execute",
    });
    if let Some(status) = status {
        update["status"] = json!(status);
    }
    update
}

fn tool_call_update(status: Option<&str>) -> serde_json::Value {
    let mut update = json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": TOOL_CALL_ID,
        "content": [],
    });
    if let Some(status) = status {
        update["status"] = json!(status);
    }
    update
}

fn stalled_ids(store: &StateStore, thresholds: PromptStaleThresholds) -> Vec<String> {
    store
        .mark_stalled_prompts(thresholds, STALE_REASON)
        .expect("mark_stalled_prompts should run")
        .into_iter()
        .map(|prompt| prompt.prompt_id)
        .collect()
}

fn stuck_count(store: &StateStore, thresholds: PromptStaleThresholds) -> i64 {
    store
        .count_stuck_prompts(thresholds)
        .expect("count_stuck_prompts should run")
        .0
}

fn settlement(status: PromptStatus) -> PromptSettlement<'static> {
    PromptSettlement {
        status,
        stop_reason: Some("end_turn"),
        error_code: None,
        error_message: None,
        failure_class: None,
        failure_detail_json: None,
    }
}

#[test]
fn mark_stalled_prompts_flips_only_aged_rows() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

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
        .mark_stalled_prompts(stale_thresholds(), STALE_REASON)
        .expect("mark_stalled_prompts should run");

    assert_eq!(pairs.len(), 1);
    assert_eq!(pairs[0].prompt_id, "prm_aged");
    assert_eq!(pairs[0].session_id, "sess_aged");
    assert_eq!(
        pairs[0].threshold,
        Duration::from_secs(STALE_THRESHOLD_SECS)
    );

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
        .mark_stalled_prompts(stale_thresholds(), STALE_REASON)
        .expect("mark_stalled_prompts should run");
    assert_eq!(first.len(), 1);

    let second = store
        .mark_stalled_prompts(stale_thresholds(), STALE_REASON)
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

    // Terminal rows aged past the threshold: once settled, the durable status is the truth.
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
        .mark_stalled_prompts(stale_thresholds(), STALE_REASON)
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
    // The running flip and the activity touch go through `update_prompt_status`; neither may
    // revive a terminal row. Only `settle_prompt` replaces a `stalled` verdict.
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
        // The supervisor late-settle: no error, but a no-op on the data.
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

    // The no-op handling must not mask a genuinely missing row.
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

    let (count, oldest) = store
        .count_stuck_prompts(stale_thresholds())
        .expect("count_stuck_prompts should run");
    assert_eq!(count, 0);
    assert!(oldest.is_none());

    let aged_older = "2019-01-01T00:00:00.000000000Z";
    let aged_newer = "2020-01-01T00:00:00.000000000Z";
    seed_running_prompt_at(&store, "sess_a", "prm_a", aged_older);
    seed_running_prompt_at(&store, "sess_b", "prm_b", aged_newer);

    let (count, oldest) = store
        .count_stuck_prompts(stale_thresholds())
        .expect("count_stuck_prompts should run");
    assert_eq!(count, 2);
    assert_eq!(oldest.as_deref(), Some(aged_older));
}

// --- Open tool calls ---

#[test]
fn an_open_tool_call_holds_the_prompt_to_the_longer_threshold() {
    let (_tempdir, store) = fresh_state("state.sqlite");
    seed_running_prompt_at(
        &store,
        "sess_tool",
        "prm_tool",
        &seconds_ago(QUIET_PAST_THRESHOLD_SECS),
    );
    append_agent_update(&store, "sess_tool", tool_call(Some("in_progress")));

    assert_eq!(stuck_count(&store, tool_call_thresholds()), 0);
    assert!(stalled_ids(&store, tool_call_thresholds()).is_empty());

    // Once the call closes, the turn is held to the quiet threshold again.
    append_agent_update(&store, "sess_tool", tool_call_update(Some("completed")));
    assert_eq!(stuck_count(&store, tool_call_thresholds()), 1);
    let stalled = store
        .mark_stalled_prompts(tool_call_thresholds(), STALE_REASON)
        .expect("mark_stalled_prompts should run");
    assert_eq!(stalled.len(), 1);
    assert_eq!(stalled[0].prompt_id, "prm_tool");
    assert_eq!(
        stalled[0].threshold,
        Duration::from_secs(STALE_THRESHOLD_SECS)
    );
}

#[test]
fn a_failed_tool_call_is_closed() {
    let (_tempdir, store) = fresh_state("state.sqlite");
    seed_running_prompt_at(
        &store,
        "sess_tool",
        "prm_tool",
        &seconds_ago(QUIET_PAST_THRESHOLD_SECS),
    );
    append_agent_update(&store, "sess_tool", tool_call(Some("in_progress")));
    append_agent_update(&store, "sess_tool", tool_call_update(Some("failed")));

    assert_eq!(
        stalled_ids(&store, tool_call_thresholds()),
        vec!["prm_tool"]
    );
}

#[test]
fn a_tool_call_without_a_status_is_pending_and_open() {
    let (_tempdir, store) = fresh_state("state.sqlite");
    seed_running_prompt_at(
        &store,
        "sess_tool",
        "prm_tool",
        &seconds_ago(QUIET_PAST_THRESHOLD_SECS),
    );
    append_agent_update(&store, "sess_tool", tool_call(None));

    assert!(stalled_ids(&store, tool_call_thresholds()).is_empty());
}

#[test]
fn a_tool_call_update_without_a_status_keeps_the_earlier_status() {
    let (_tempdir, store) = fresh_state("state.sqlite");
    seed_running_prompt_at(
        &store,
        "sess_tool",
        "prm_tool",
        &seconds_ago(QUIET_PAST_THRESHOLD_SECS),
    );
    append_agent_update(&store, "sess_tool", tool_call(Some("in_progress")));
    append_agent_update(&store, "sess_tool", tool_call_update(None));

    assert!(stalled_ids(&store, tool_call_thresholds()).is_empty());
}

#[test]
fn updates_for_a_call_this_turn_never_announced_do_not_hold_the_prompt() {
    let (_tempdir, store) = fresh_state("state.sqlite");
    seed_running_prompt_at(
        &store,
        "sess_tool",
        "prm_tool",
        &seconds_ago(QUIET_PAST_THRESHOLD_SECS),
    );
    append_agent_update(&store, "sess_tool", tool_call_update(None));
    append_agent_update(&store, "sess_tool", tool_call_update(Some("in_progress")));

    assert_eq!(stuck_count(&store, tool_call_thresholds()), 1);
    assert_eq!(
        stalled_ids(&store, tool_call_thresholds()),
        vec!["prm_tool"]
    );
}

#[test]
fn a_tool_call_left_open_by_an_earlier_turn_does_not_hold_the_prompt() {
    let (_tempdir, store) = fresh_state("state.sqlite");
    insert_state_test_session(&store, "sess_tool");
    append_agent_update(&store, "sess_tool", tool_call(Some("in_progress")));
    std::thread::sleep(WRITE_GAP);
    store
        .insert_prompt(NewPromptRecord {
            id: "prm_tool".to_owned(),
            session_id: "sess_tool".to_owned(),
            prompt_json: "[]".to_owned(),
        })
        .expect("prompt inserted");
    store
        .update_prompt_status(
            "prm_tool",
            PromptStatus::Running,
            None,
            None,
            None,
            None,
            None,
        )
        .expect("prompt flipped to running");
    Connection::open(store.path())
        .expect("open sqlite directly for updated_at override")
        .execute(
            "UPDATE prompts SET updated_at = ?1 WHERE id = ?2",
            params![seconds_ago(QUIET_PAST_THRESHOLD_SECS), "prm_tool"],
        )
        .expect("force-set updated_at");

    assert_eq!(stuck_count(&store, tool_call_thresholds()), 1);
    assert_eq!(
        stalled_ids(&store, tool_call_thresholds()),
        vec!["prm_tool"]
    );
}

#[test]
fn an_open_tool_call_past_the_longer_threshold_stalls() {
    let (_tempdir, store) = fresh_state("state.sqlite");
    seed_running_prompt_at(
        &store,
        "sess_tool",
        "prm_tool",
        &seconds_ago(TOOL_CALL_PAST_THRESHOLD_SECS),
    );
    append_agent_update(&store, "sess_tool", tool_call(Some("in_progress")));

    assert_eq!(stuck_count(&store, tool_call_thresholds()), 1);
    let stalled = store
        .mark_stalled_prompts(tool_call_thresholds(), STALE_REASON)
        .expect("mark_stalled_prompts should run");
    assert_eq!(stalled.len(), 1);
    assert_eq!(
        stalled[0].threshold,
        Duration::from_secs(OPEN_TOOL_CALL_THRESHOLD_SECS)
    );
}

// --- settle_prompt ---

fn stalled_prompt(store: &StateStore, session_id: &str, prompt_id: &str) {
    seed_running_prompt_at(
        store,
        session_id,
        prompt_id,
        "2020-01-01T00:00:00.000000000Z",
    );
    let stalled = stalled_ids(store, stale_thresholds());
    assert_eq!(stalled, vec![prompt_id]);
}

#[test]
fn settle_prompt_replaces_a_stall_with_the_agent_verdict() {
    let (_tempdir, store) = fresh_state("state.sqlite");
    stalled_prompt(&store, "sess_settle", "prm_settle");

    let settle = store
        .settle_prompt("prm_settle", &settlement(PromptStatus::Completed), true)
        .expect("settle");

    assert_eq!(settle, PromptSettle::ReplacedStall);
    let row = store
        .get_prompt("prm_settle")
        .expect("prompt lookup")
        .expect("prompt exists");
    assert_eq!(row.status, "completed");
    assert_eq!(row.stop_reason.as_deref(), Some("end_turn"));
    assert!(row.error_code.is_none(), "{row:?}");
    assert!(row.error_message.is_none(), "{row:?}");
    assert!(row.failure_class.is_none(), "{row:?}");
    assert!(row.failure_detail_json.is_none(), "{row:?}");
}

#[test]
fn settle_prompt_keeps_a_stall_unless_asked_to_replace_it() {
    let (_tempdir, store) = fresh_state("state.sqlite");
    stalled_prompt(&store, "sess_settle", "prm_settle");

    let settle = store
        .settle_prompt("prm_settle", &settlement(PromptStatus::Cancelled), false)
        .expect("settle");

    assert_eq!(settle, PromptSettle::AlreadyTerminal);
    let row = store
        .get_prompt("prm_settle")
        .expect("prompt lookup")
        .expect("prompt exists");
    assert_eq!(row.status, "stalled");
    assert_eq!(
        row.failure_class.as_deref(),
        Some(FailureClass::Stalled.as_str())
    );
}

#[test]
fn settle_prompt_never_replaces_another_terminal_status() {
    let (_tempdir, store) = fresh_state("state.sqlite");
    insert_state_test_session(&store, "sess_settle");
    for (prompt_id, first) in [
        ("prm_completed", PromptStatus::Completed),
        ("prm_errored", PromptStatus::Errored),
        ("prm_cancelled", PromptStatus::Cancelled),
    ] {
        store
            .insert_prompt(NewPromptRecord {
                id: prompt_id.to_owned(),
                session_id: "sess_settle".to_owned(),
                prompt_json: "[]".to_owned(),
            })
            .expect("prompt inserted");
        store
            .update_prompt_status(prompt_id, first, None, Some("first.code"), None, None, None)
            .expect("first terminal write");

        let settle = store
            .settle_prompt(prompt_id, &settlement(PromptStatus::Completed), true)
            .expect("settle");

        assert_eq!(settle, PromptSettle::AlreadyTerminal, "{prompt_id}");
        let row = store
            .get_prompt(prompt_id)
            .expect("prompt lookup")
            .expect("prompt exists");
        assert_eq!(row.status, first.as_str());
        assert_eq!(row.error_code.as_deref(), Some("first.code"));
    }
}

#[test]
fn settle_prompt_writes_an_in_flight_row() {
    let (_tempdir, store) = fresh_state("state.sqlite");
    seed_running_prompt_at(
        &store,
        "sess_settle",
        "prm_settle",
        "2020-01-01T00:00:00.000000000Z",
    );
    let detail = r#"{"status_code":503,"reason_category":"overloaded"}"#;

    let settle = store
        .settle_prompt(
            "prm_settle",
            &PromptSettlement {
                status: PromptStatus::Errored,
                stop_reason: None,
                error_code: Some("inference.request_failed"),
                error_message: Some("upstream returned 503"),
                failure_class: Some(FailureClass::Inference5xx.as_str()),
                failure_detail_json: Some(detail),
            },
            false,
        )
        .expect("settle");

    assert_eq!(settle, PromptSettle::Applied);
    let row = store
        .get_prompt("prm_settle")
        .expect("prompt lookup")
        .expect("prompt exists");
    assert_eq!(row.status, "errored");
    assert_eq!(row.error_code.as_deref(), Some("inference.request_failed"));
    assert_eq!(row.error_message.as_deref(), Some("upstream returned 503"));
    assert_eq!(
        row.failure_class.as_deref(),
        Some(FailureClass::Inference5xx.as_str())
    );
    assert_eq!(row.failure_detail_json.as_deref(), Some(detail));
}

#[test]
fn settle_prompt_reports_a_missing_row() {
    let (_tempdir, store) = fresh_state("state.sqlite");

    let missing = store.settle_prompt("prm_missing", &settlement(PromptStatus::Completed), true);

    match missing {
        Err(acp_stack::error::StackError::PromptNotFound { id }) => {
            assert_eq!(id, "prm_missing");
        }
        other => panic!("expected PromptNotFound, got {other:?}"),
    }
}

#[test]
fn settle_prompt_refuses_a_non_terminal_status() {
    let (_tempdir, store) = fresh_state("state.sqlite");
    seed_running_prompt_at(
        &store,
        "sess_settle",
        "prm_settle",
        "2020-01-01T00:00:00.000000000Z",
    );

    let refused = store.settle_prompt("prm_settle", &settlement(PromptStatus::Running), true);

    assert!(
        matches!(
            refused,
            Err(acp_stack::error::StackError::InvalidParam { .. })
        ),
        "{refused:?}"
    );
}
