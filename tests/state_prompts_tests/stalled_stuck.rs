use acp_stack::state::{FailureClass, NewPromptRecord, NewSessionRecord, PromptStatus, StateStore};
use rusqlite::Connection;
use rusqlite::params;

use crate::common::state::{STALE_REASON, STALE_THRESHOLD_SECS, seed_running_prompt_at};

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
