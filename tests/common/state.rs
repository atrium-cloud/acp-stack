//! Shared fixtures for the `state_*_tests` binaries: tempdir-backed stores,
//! row seeders, and in-memory `Event` builders for `LogFilter::matches`.

use acp_stack::state::{Event, NewPromptRecord, NewSessionRecord, PromptStatus, StateStore};
use rusqlite::Connection;
use rusqlite::params;

// CONSTANTS for the mark_stalled_prompts tests.
pub const STALE_THRESHOLD_SECS: u64 = 60;
pub const STALE_REASON: &str = "test stall reason";

pub fn fresh_state(name: &str) -> (tempfile::TempDir, StateStore) {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let path = tempdir.path().join(name);
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migrate");
    (tempdir, store)
}

pub fn insert_state_test_session(store: &StateStore, session_id: &str) {
    store
        .insert_session(NewSessionRecord {
            id: session_id.to_owned(),
            agent_id: "fake".to_owned(),
            cwd: format!("/tmp/{session_id}"),
            title: None,
            metadata_json: "{}".to_owned(),
        })
        .expect("session inserted");
}

/// Helper: insert a session + one prompt, flip the prompt to running,
/// then overwrite its `updated_at` directly so the test controls the
/// "how old is this row" axis without sleeping for minutes.
pub fn seed_running_prompt_at(
    store: &StateStore,
    session_id: &str,
    prompt_id: &str,
    updated_at: &str,
) {
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
        .update_prompt_status(
            prompt_id,
            PromptStatus::Running,
            None,
            None,
            None,
            None,
            None,
        )
        .expect("prompt flipped to running");
    // Force `updated_at` so the test does not have to wait for the
    // threshold to actually elapse on wall-clock time.
    let connection =
        Connection::open(store.path()).expect("open sqlite directly for updated_at override");
    connection
        .execute(
            "UPDATE prompts SET updated_at = ?1 WHERE id = ?2",
            params![updated_at, prompt_id],
        )
        .expect("force-set updated_at");
}

pub fn fake_event(kind: &str, level: &str, source: &str, payload_json: &str) -> Event {
    fake_event_at(kind, level, source, payload_json, "2026-05-25T12:00:00Z")
}

pub fn fake_event_at(
    kind: &str,
    level: &str,
    source: &str,
    payload_json: &str,
    created_at: &str,
) -> Event {
    Event {
        id: format!("evt_{kind}_{level}"),
        created_at: created_at.to_owned(),
        level: level.to_owned(),
        kind: kind.to_owned(),
        message: String::new(),
        payload_json: payload_json.to_owned(),
        source: source.to_owned(),
        session_id: None,
    }
}

pub fn fake_session_event(
    kind: &str,
    level: &str,
    source: &str,
    payload_json: &str,
    session_id: Option<&str>,
) -> Event {
    Event {
        id: format!("evt_{kind}_{level}"),
        created_at: "2026-05-25T12:00:00Z".to_owned(),
        level: level.to_owned(),
        kind: kind.to_owned(),
        message: String::new(),
        payload_json: payload_json.to_owned(),
        source: source.to_owned(),
        session_id: session_id.map(str::to_owned),
    }
}
