use acp_stack::state::{
    EVENT_SOURCE_ACP, EVENT_SOURCE_SYSTEM, ListedSessionRecord, NewPermissionRequest,
    NewPromptRecord, NewSessionRecord, PromptStatus, SESSION_ACTIVITY_ACTOR_AGENT,
    SESSION_ACTIVITY_ACTOR_USER, SESSION_STATUS_ACTIVE, SESSION_STATUS_AVAILABLE,
    SESSION_STATUS_CLOSED, SessionAvailableCommand, StateStore,
};

mod common;
use common::state::fresh_state;

#[test]
fn replace_session_available_commands_replaces_and_advances_updated_at() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let store = StateStore::open(tempdir.path().join("state.sqlite")).expect("state should open");
    store.migrate().expect("migration should pass");

    let missing = store.replace_session_available_commands(
        "sess_missing",
        &[SessionAvailableCommand {
            name: "compact".to_owned(),
            description: String::new(),
            input_hint: None,
        }],
    );
    assert!(matches!(
        missing,
        Err(acp_stack::error::StackError::SessionNotFound { .. })
    ));

    store
        .insert_session(NewSessionRecord {
            id: "sess_cmds".to_owned(),
            agent_id: "fake".to_owned(),
            cwd: "/tmp".to_owned(),
            title: None,
            metadata_json: r#"{"preserved":true}"#.to_owned(),
        })
        .expect("session inserted");
    let before = store
        .get_session("sess_cmds")
        .expect("session lookup")
        .expect("session exists")
        .updated_at;

    let changed = store
        .replace_session_available_commands(
            "sess_cmds",
            &[
                SessionAvailableCommand {
                    name: "compact".to_owned(),
                    description: "Summarize".to_owned(),
                    input_hint: Some("optional instructions".to_owned()),
                },
                SessionAvailableCommand {
                    name: "init".to_owned(),
                    description: "Create AGENTS.md".to_owned(),
                    input_hint: None,
                },
            ],
        )
        .expect("commands stored");
    assert!(changed);
    let session = store
        .get_session("sess_cmds")
        .expect("session lookup")
        .expect("session exists");
    assert!(session.updated_at >= before);
    let metadata: serde_json::Value =
        serde_json::from_str(&session.metadata_json).expect("metadata JSON");
    assert_eq!(metadata["preserved"], true);
    assert_eq!(
        metadata["available_commands"]
            .as_array()
            .expect("commands array")
            .len(),
        2
    );
    assert_eq!(metadata["available_commands"][0]["name"], "compact");
    assert!(metadata["available_commands_updated_at"].is_string());

    // Re-advertising an identical list is a no-op: no row rewrite, no
    // updated_at bump.
    let unchanged_at = session.updated_at.clone();
    std::thread::sleep(std::time::Duration::from_millis(5));
    let changed = store
        .replace_session_available_commands(
            "sess_cmds",
            &[
                SessionAvailableCommand {
                    name: "compact".to_owned(),
                    description: "Summarize".to_owned(),
                    input_hint: Some("optional instructions".to_owned()),
                },
                SessionAvailableCommand {
                    name: "init".to_owned(),
                    description: "Create AGENTS.md".to_owned(),
                    input_hint: None,
                },
            ],
        )
        .expect("identical replace");
    assert!(!changed);
    let session = store
        .get_session("sess_cmds")
        .expect("session lookup")
        .expect("session exists");
    assert_eq!(session.updated_at, unchanged_at);

    // Latest-wins replace, including down to an empty list.
    store
        .replace_session_available_commands("sess_cmds", &[])
        .expect("empty replace");
    let session = store
        .get_session("sess_cmds")
        .expect("session lookup")
        .expect("session exists");
    let metadata: serde_json::Value =
        serde_json::from_str(&session.metadata_json).expect("metadata JSON");
    assert_eq!(
        metadata["available_commands"]
            .as_array()
            .expect("commands array")
            .len(),
        0
    );
    assert_eq!(metadata["preserved"], true);
}

#[test]
fn upsert_listed_sessions_inserts_available_and_preserves_active() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    store
        .insert_session(NewSessionRecord {
            id: "sess_active".to_owned(),
            agent_id: "fake".to_owned(),
            cwd: "/tmp/active".to_owned(),
            title: None,
            metadata_json: "{}".to_owned(),
        })
        .expect("active session inserted");
    store
        .insert_session(NewSessionRecord {
            id: "sess_closed".to_owned(),
            agent_id: "fake".to_owned(),
            cwd: "/tmp/closed".to_owned(),
            title: None,
            metadata_json: "{}".to_owned(),
        })
        .expect("closed session inserted");
    store
        .update_session_status("sess_closed", SESSION_STATUS_CLOSED)
        .expect("session closed");

    let counts = store
        .upsert_listed_sessions(vec![
            ListedSessionRecord {
                id: "sess_active".to_owned(),
                agent_session_id: "sess_active".to_owned(),
                agent_id: "fake".to_owned(),
                cwd: "/tmp/active-listed".to_owned(),
                title: Some("active listed".to_owned()),
                updated_at: Some("2026-05-25T00:00:00Z".to_owned()),
                metadata_json: r#"{"source":"agent_list"}"#.to_owned(),
            },
            ListedSessionRecord {
                id: "sess_closed".to_owned(),
                agent_session_id: "sess_closed".to_owned(),
                agent_id: "fake".to_owned(),
                cwd: "/tmp/closed-listed".to_owned(),
                title: Some("closed listed".to_owned()),
                updated_at: Some("2026-05-25T00:00:02Z".to_owned()),
                metadata_json: r#"{"source":"agent_list"}"#.to_owned(),
            },
            ListedSessionRecord {
                id: "sess_available".to_owned(),
                agent_session_id: "sess_available".to_owned(),
                agent_id: "fake".to_owned(),
                cwd: "/tmp/available".to_owned(),
                title: Some("available listed".to_owned()),
                updated_at: Some("2026-05-25T00:00:01Z".to_owned()),
                metadata_json: r#"{"source":"agent_list"}"#.to_owned(),
            },
        ])
        .expect("listed sessions upsert");

    assert_eq!(counts.upserted, 1);
    assert_eq!(counts.updated, 2);
    let active = store
        .get_session("sess_active")
        .expect("active lookup")
        .expect("active exists");
    assert_eq!(active.status, SESSION_STATUS_ACTIVE);
    assert_eq!(active.updated_at, "2026-05-25T00:00:00.000000000Z");
    assert_eq!(active.cwd, "/tmp/active-listed");
    assert_eq!(active.title.as_deref(), Some("active listed"));
    let closed = store
        .get_session("sess_closed")
        .expect("closed lookup")
        .expect("closed exists");
    assert_eq!(closed.status, SESSION_STATUS_CLOSED);
    assert_eq!(closed.cwd, "/tmp/closed-listed");
    let available = store
        .get_session("sess_available")
        .expect("available lookup")
        .expect("available exists");
    assert_eq!(available.status, SESSION_STATUS_AVAILABLE);
}

#[test]
fn upsert_listed_sessions_normalizes_updated_at_for_range_ordering() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    store
        .upsert_listed_sessions(vec![
            ListedSessionRecord {
                id: "sess_offset".to_owned(),
                agent_session_id: "sess_offset".to_owned(),
                agent_id: "fake".to_owned(),
                cwd: "/tmp/offset".to_owned(),
                title: None,
                updated_at: Some("2026-02-01T08:00:00+08:00".to_owned()),
                metadata_json: "{}".to_owned(),
            },
            ListedSessionRecord {
                id: "sess_fraction".to_owned(),
                agent_session_id: "sess_fraction".to_owned(),
                agent_id: "fake".to_owned(),
                cwd: "/tmp/fraction".to_owned(),
                title: None,
                updated_at: Some("2026-02-01T00:00:00.500Z".to_owned()),
                metadata_json: "{}".to_owned(),
            },
        ])
        .expect("listed sessions upsert");

    let rows = store
        .query_sessions(acp_stack::state::SessionFilter {
            limit: 10,
            since: Some("2026-02-01T00:00:00.250000000Z"),
            until: Some("2026-02-01T00:00:01.000000000Z"),
            ..Default::default()
        })
        .expect("sessions query");
    let ids: Vec<&str> = rows.iter().map(|row| row.id.as_str()).collect();
    assert_eq!(ids, vec!["sess_fraction"]);
    assert_eq!(rows[0].updated_at, "2026-02-01T00:00:00.500000000Z");

    let offset = store
        .get_session("sess_offset")
        .expect("offset lookup")
        .expect("offset exists");
    assert_eq!(offset.updated_at, "2026-02-01T00:00:00.000000000Z");
}

#[test]
fn sessions_store_target_id_and_agent_session_id() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    let primary = store
        .insert_session(NewSessionRecord {
            id: "sess_primary".to_owned(),
            agent_id: "opencode".to_owned(),
            cwd: "/tmp/primary".to_owned(),
            title: None,
            metadata_json: "{}".to_owned(),
        })
        .expect("primary session inserted");
    assert_eq!(primary.target_id, "opencode");
    assert_eq!(primary.agent_session_id, "sess_primary");

    let secondary = store
        .insert_session_for_target(
            "codex",
            "acp_secondary".to_owned(),
            NewSessionRecord {
                id: "sess_secondary".to_owned(),
                agent_id: "codex".to_owned(),
                cwd: "/tmp/secondary".to_owned(),
                title: None,
                metadata_json: "{}".to_owned(),
            },
        )
        .expect("secondary session inserted");
    assert_eq!(secondary.target_id, "codex");
    assert_eq!(secondary.agent_session_id, "acp_secondary");

    let rows = store
        .query_sessions(acp_stack::state::SessionFilter {
            limit: 10,
            target_id: Some("codex"),
            ..Default::default()
        })
        .expect("target-scoped query");
    let ids: Vec<&str> = rows.iter().map(|row| row.id.as_str()).collect();
    assert_eq!(ids, vec!["sess_secondary"]);

    let status_rows = store
        .query_session_status_window("1970-01-01T00:00:00.000000000Z", Some("codex"), 10)
        .expect("target-scoped status query");
    let status_ids: Vec<&str> = status_rows.iter().map(|row| row.id.as_str()).collect();
    assert_eq!(status_ids, vec!["sess_secondary"]);

    store
        .upsert_listed_sessions_for_target(
            "codex",
            vec![ListedSessionRecord {
                id: "local_agent_1".to_owned(),
                agent_session_id: "shared_acp_session".to_owned(),
                agent_id: "codex".to_owned(),
                cwd: "/tmp/shared-one".to_owned(),
                title: Some("one".to_owned()),
                updated_at: Some("2026-04-01T00:00:00Z".to_owned()),
                metadata_json: "{}".to_owned(),
            }],
        )
        .expect("codex listed session upsert");
    store
        .upsert_listed_sessions_for_target(
            "opencode",
            vec![ListedSessionRecord {
                id: "local_agent_2".to_owned(),
                agent_session_id: "shared_acp_session".to_owned(),
                agent_id: "opencode".to_owned(),
                cwd: "/tmp/shared-two".to_owned(),
                title: Some("two".to_owned()),
                updated_at: Some("2026-04-01T00:00:01Z".to_owned()),
                metadata_json: "{}".to_owned(),
            }],
        )
        .expect("opencode listed session upsert");
    store
        .upsert_listed_sessions_for_target(
            "codex",
            vec![ListedSessionRecord {
                id: "should_not_replace_local_id".to_owned(),
                agent_session_id: "shared_acp_session".to_owned(),
                agent_id: "codex".to_owned(),
                cwd: "/tmp/shared-one-updated".to_owned(),
                title: Some("one updated".to_owned()),
                updated_at: Some("2026-04-01T00:00:02Z".to_owned()),
                metadata_json: "{}".to_owned(),
            }],
        )
        .expect("codex listed session update");
    let agent_one = store
        .get_session_by_target_agent_session_id("codex", "shared_acp_session")
        .expect("codex lookup")
        .expect("codex row");
    let agent_two = store
        .get_session_by_target_agent_session_id("opencode", "shared_acp_session")
        .expect("opencode lookup")
        .expect("opencode row");
    assert_eq!(agent_one.id, "local_agent_1");
    assert_eq!(agent_one.title.as_deref(), Some("one updated"));
    assert_eq!(agent_two.id, "local_agent_2");
    assert_eq!(agent_two.title.as_deref(), Some("two"));
}

#[test]
fn renames_session_target_id_for_legacy_agent_switch() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    store
        .insert_session(NewSessionRecord {
            id: "sess_primary".to_owned(),
            agent_id: "opencode".to_owned(),
            cwd: "/tmp/primary".to_owned(),
            title: None,
            metadata_json: "{}".to_owned(),
        })
        .expect("primary session inserted");
    store
        .insert_session_for_target(
            "codex",
            "acp_secondary".to_owned(),
            NewSessionRecord {
                id: "sess_secondary".to_owned(),
                agent_id: "codex".to_owned(),
                cwd: "/tmp/secondary".to_owned(),
                title: None,
                metadata_json: "{}".to_owned(),
            },
        )
        .expect("secondary session inserted");

    let renamed = store
        .rename_session_target_id("opencode", "claude-code")
        .expect("target ids should be renamed");
    assert_eq!(renamed, 1);

    let primary_rows = store
        .query_sessions(acp_stack::state::SessionFilter {
            limit: 10,
            target_id: Some("claude-code"),
            ..Default::default()
        })
        .expect("renamed target query");
    let primary_ids: Vec<&str> = primary_rows.iter().map(|row| row.id.as_str()).collect();
    assert_eq!(primary_ids, vec!["sess_primary"]);

    let secondary_rows = store
        .query_sessions(acp_stack::state::SessionFilter {
            limit: 10,
            target_id: Some("codex"),
            ..Default::default()
        })
        .expect("unchanged target query");
    let secondary_ids: Vec<&str> = secondary_rows.iter().map(|row| row.id.as_str()).collect();
    assert_eq!(secondary_ids, vec!["sess_secondary"]);
}

#[test]
fn insert_session_for_target_rejects_duplicate_agent_session_id() {
    // The UNIQUE(target_id, agent_session_id) index is the sole guard against a
    // duplicate session under one target (insert_session_for_target has no ON
    // CONFLICT). A second insert of the same pair must error.
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    store
        .insert_session_for_target(
            "codex",
            "acp_dup".to_owned(),
            NewSessionRecord {
                id: "sess_one".to_owned(),
                agent_id: "codex".to_owned(),
                cwd: "/tmp/one".to_owned(),
                title: None,
                metadata_json: "{}".to_owned(),
            },
        )
        .expect("first insert");

    let duplicate = store.insert_session_for_target(
        "codex",
        "acp_dup".to_owned(),
        NewSessionRecord {
            id: "sess_two".to_owned(),
            agent_id: "codex".to_owned(),
            cwd: "/tmp/two".to_owned(),
            title: None,
            metadata_json: "{}".to_owned(),
        },
    );
    assert!(
        duplicate.is_err(),
        "duplicate (target_id, agent_session_id) must violate the UNIQUE index",
    );

    // The same agent_session_id under a DIFFERENT target is still allowed.
    store
        .insert_session_for_target(
            "opencode",
            "acp_dup".to_owned(),
            NewSessionRecord {
                id: "sess_three".to_owned(),
                agent_id: "opencode".to_owned(),
                cwd: "/tmp/three".to_owned(),
                title: None,
                metadata_json: "{}".to_owned(),
            },
        )
        .expect("cross-target reuse allowed");
}

#[test]
fn rename_session_target_id_rejects_agent_session_id_collision() {
    // When the destination target already owns a session whose agent_session_id
    // matches one being moved in, the rename must fail fast (before any row
    // moves) rather than surface a raw UNIQUE violation mid-move.
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    store
        .insert_session_for_target(
            "opencode",
            "shared_acp".to_owned(),
            NewSessionRecord {
                id: "sess_old".to_owned(),
                agent_id: "opencode".to_owned(),
                cwd: "/tmp/old".to_owned(),
                title: None,
                metadata_json: "{}".to_owned(),
            },
        )
        .expect("old target session inserted");
    store
        .insert_session_for_target(
            "claude-code",
            "shared_acp".to_owned(),
            NewSessionRecord {
                id: "sess_new".to_owned(),
                agent_id: "claude-code".to_owned(),
                cwd: "/tmp/new".to_owned(),
                title: None,
                metadata_json: "{}".to_owned(),
            },
        )
        .expect("new target session inserted");

    let result = store.rename_session_target_id("opencode", "claude-code");
    assert!(
        matches!(
            result,
            Err(acp_stack::error::StackError::SessionTargetRenameConflict { count: 1, .. })
        ),
        "rename into a colliding target must fail fast; got {result:?}",
    );

    // No partial rename: the source row stays under its original target.
    let old_rows = store
        .query_sessions(acp_stack::state::SessionFilter {
            limit: 10,
            target_id: Some("opencode"),
            ..Default::default()
        })
        .expect("old target query");
    let old_ids: Vec<&str> = old_rows.iter().map(|row| row.id.as_str()).collect();
    assert_eq!(old_ids, vec!["sess_old"]);
}

#[test]
fn active_session_activity_is_empty_without_active_sessions() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    store
        .insert_session(NewSessionRecord {
            id: "sess_closed".to_owned(),
            agent_id: "fake".to_owned(),
            cwd: "/tmp/closed".to_owned(),
            title: None,
            metadata_json: "{}".to_owned(),
        })
        .expect("session inserted");
    store
        .update_session_status("sess_closed", SESSION_STATUS_CLOSED)
        .expect("session closed");

    let rows = store
        .query_active_session_activity(10)
        .expect("activity should query");
    assert!(rows.is_empty());
}

#[test]
fn active_session_activity_falls_back_to_session_update() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    let session = store
        .insert_session(NewSessionRecord {
            id: "sess_active".to_owned(),
            agent_id: "fake".to_owned(),
            cwd: "/tmp/active".to_owned(),
            title: None,
            metadata_json: "{}".to_owned(),
        })
        .expect("session inserted");

    let rows = store
        .query_active_session_activity(10)
        .expect("activity should query");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, "sess_active");
    assert_eq!(rows[0].last_activity_at, session.updated_at);
    assert_eq!(rows[0].last_activity_from, SESSION_ACTIVITY_ACTOR_USER);
}

#[test]
fn active_session_activity_tracks_prompt_submission_as_user() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    store
        .insert_session(NewSessionRecord {
            id: "sess_active".to_owned(),
            agent_id: "fake".to_owned(),
            cwd: "/tmp/active".to_owned(),
            title: None,
            metadata_json: "{}".to_owned(),
        })
        .expect("session inserted");
    let prompt = store
        .insert_prompt(NewPromptRecord {
            id: "prm_active".to_owned(),
            session_id: "sess_active".to_owned(),
            prompt_json: "[]".to_owned(),
        })
        .expect("prompt inserted");

    let rows = store
        .query_active_session_activity(10)
        .expect("activity should query");
    assert_eq!(rows[0].last_activity_at, prompt.created_at);
    assert_eq!(rows[0].last_activity_from, SESSION_ACTIVITY_ACTOR_USER);
}

#[test]
fn prompt_message_id_round_trips_and_acknowledges() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    store
        .insert_session(NewSessionRecord {
            id: "sess_message_id".to_owned(),
            agent_id: "fake".to_owned(),
            cwd: "/tmp/message-id".to_owned(),
            title: None,
            metadata_json: "{}".to_owned(),
        })
        .expect("session inserted");
    let prompt = store
        .insert_prompt_with_message_id(
            NewPromptRecord {
                id: "prm_message_id".to_owned(),
                session_id: "sess_message_id".to_owned(),
                prompt_json: "[]".to_owned(),
            },
            Some("00000000-0000-4000-8000-000000000001".to_owned()),
        )
        .expect("prompt inserted");
    assert_eq!(
        prompt.message_id.as_deref(),
        Some("00000000-0000-4000-8000-000000000001")
    );
    assert!(!prompt.message_id_acknowledged);

    store
        .acknowledge_prompt_message_id("prm_message_id", "00000000-0000-4000-8000-000000000001")
        .expect("prompt message id acknowledged");
    let prompt = store
        .get_prompt_by_message_id("sess_message_id", "00000000-0000-4000-8000-000000000001")
        .expect("prompt lookup")
        .expect("prompt exists");
    assert!(prompt.message_id_acknowledged);
}

#[test]
fn active_session_activity_tracks_prompt_status_update_as_agent() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    store
        .insert_session(NewSessionRecord {
            id: "sess_active".to_owned(),
            agent_id: "fake".to_owned(),
            cwd: "/tmp/active".to_owned(),
            title: None,
            metadata_json: "{}".to_owned(),
        })
        .expect("session inserted");
    store
        .insert_prompt(NewPromptRecord {
            id: "prm_active".to_owned(),
            session_id: "sess_active".to_owned(),
            prompt_json: "[]".to_owned(),
        })
        .expect("prompt inserted");
    std::thread::sleep(std::time::Duration::from_millis(2));
    store
        .update_prompt_status(
            "prm_active",
            PromptStatus::Running,
            None,
            None,
            None,
            None,
            None,
        )
        .expect("prompt status updated");
    let prompt = store
        .get_prompt("prm_active")
        .expect("prompt lookup")
        .expect("prompt exists");

    let rows = store
        .query_active_session_activity(10)
        .expect("activity should query");
    assert_eq!(rows[0].last_activity_at, prompt.updated_at);
    assert_eq!(rows[0].last_activity_from, SESSION_ACTIVITY_ACTOR_AGENT);
}

#[test]
fn active_session_activity_tracks_acp_event_as_agent() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    store
        .insert_session(NewSessionRecord {
            id: "sess_active".to_owned(),
            agent_id: "fake".to_owned(),
            cwd: "/tmp/active".to_owned(),
            title: None,
            metadata_json: "{}".to_owned(),
        })
        .expect("session inserted");
    std::thread::sleep(std::time::Duration::from_millis(2));
    let event = store
        .append_session_event_with_source(
            "sess_active",
            "info",
            "session.update",
            EVENT_SOURCE_ACP,
            "ACP session update",
            "{}",
        )
        .expect("event appended");

    let rows = store
        .query_active_session_activity(10)
        .expect("activity should query");
    assert_eq!(rows[0].last_activity_at, event.created_at);
    assert_eq!(rows[0].last_activity_from, SESSION_ACTIVITY_ACTOR_AGENT);
}

#[test]
fn session_status_window_reports_latest_prompt_and_stream_start() {
    let (_dir, store) = fresh_state("session_status_prompt.sqlite");
    store
        .insert_session(NewSessionRecord {
            id: "sess_status".to_owned(),
            agent_id: "fake".to_owned(),
            cwd: "/tmp/status".to_owned(),
            title: None,
            metadata_json: "{}".to_owned(),
        })
        .expect("session inserted");
    store
        .insert_prompt(NewPromptRecord {
            id: "prm_status".to_owned(),
            session_id: "sess_status".to_owned(),
            prompt_json: "[]".to_owned(),
        })
        .expect("prompt inserted");
    store
        .update_prompt_status(
            "prm_status",
            PromptStatus::Running,
            None,
            None,
            None,
            None,
            None,
        )
        .expect("prompt running");

    let rows = store
        .query_session_status_window("1970-01-01T00:00:00.000000000Z", None, 10)
        .expect("status rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, "sess_status");
    assert_eq!(
        rows[0]
            .latest_prompt
            .as_ref()
            .map(|prompt| prompt.id.as_str()),
        Some("prm_status")
    );
    assert!(rows[0].prompt_stream_started_at.is_none());

    std::thread::sleep(std::time::Duration::from_millis(2));
    let event = store
        .append_session_event_with_source(
            "sess_status",
            "info",
            "session.update",
            EVENT_SOURCE_ACP,
            "ACP session update",
            "{}",
        )
        .expect("session update");

    let rows = store
        .query_session_status_window("1970-01-01T00:00:00.000000000Z", None, 10)
        .expect("status rows");
    assert_eq!(
        rows[0].prompt_stream_started_at.as_deref(),
        Some(event.created_at.as_str())
    );
}

#[test]
fn session_status_window_ignores_non_acp_session_update_for_stream_start() {
    let (_dir, store) = fresh_state("session_status_non_acp_stream.sqlite");
    store
        .insert_session(NewSessionRecord {
            id: "sess_non_acp".to_owned(),
            agent_id: "fake".to_owned(),
            cwd: "/tmp/non-acp".to_owned(),
            title: None,
            metadata_json: "{}".to_owned(),
        })
        .expect("session inserted");
    store
        .insert_prompt(NewPromptRecord {
            id: "prm_non_acp".to_owned(),
            session_id: "sess_non_acp".to_owned(),
            prompt_json: "[]".to_owned(),
        })
        .expect("prompt inserted");
    store
        .append_session_event_with_source(
            "sess_non_acp",
            "info",
            "session.update",
            EVENT_SOURCE_SYSTEM,
            "system session update",
            "{}",
        )
        .expect("system session update");

    let rows = store
        .query_session_status_window("1970-01-01T00:00:00.000000000Z", None, 10)
        .expect("status rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0]
            .latest_prompt
            .as_ref()
            .map(|prompt| prompt.id.as_str()),
        Some("prm_non_acp")
    );
    assert!(rows[0].prompt_stream_started_at.is_none());
}

#[test]
fn session_status_window_uses_oldest_in_flight_prompt_for_streaming() {
    let (_dir, store) = fresh_state("session_status_concurrent_prompt.sqlite");
    store
        .insert_session(NewSessionRecord {
            id: "sess_concurrent".to_owned(),
            agent_id: "fake".to_owned(),
            cwd: "/tmp/concurrent".to_owned(),
            title: None,
            metadata_json: "{}".to_owned(),
        })
        .expect("session inserted");
    for prompt_id in ["prm_first", "prm_second"] {
        store
            .insert_prompt(NewPromptRecord {
                id: prompt_id.to_owned(),
                session_id: "sess_concurrent".to_owned(),
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
            .expect("prompt running");
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    let event = store
        .append_session_event_with_source(
            "sess_concurrent",
            "info",
            "session.update",
            EVENT_SOURCE_ACP,
            "ACP session update",
            "{}",
        )
        .expect("session update");

    let rows = store
        .query_session_status_window("1970-01-01T00:00:00.000000000Z", None, 10)
        .expect("status rows");
    assert_eq!(
        rows[0]
            .latest_prompt
            .as_ref()
            .map(|prompt| prompt.id.as_str()),
        Some("prm_first")
    );
    assert_eq!(
        rows[0].prompt_stream_started_at.as_deref(),
        Some(event.created_at.as_str())
    );
}

#[test]
fn session_status_window_includes_pending_acp_permission() {
    let (_dir, store) = fresh_state("session_status_permission.sqlite");
    store
        .insert_session(NewSessionRecord {
            id: "sess_permission".to_owned(),
            agent_id: "fake".to_owned(),
            cwd: "/tmp/permission".to_owned(),
            title: None,
            metadata_json: "{}".to_owned(),
        })
        .expect("session inserted");
    let permission = store
        .append_permission_request(NewPermissionRequest {
            source: "acp",
            requester: Some("agent"),
            subject_id: Some("sess_permission"),
            detail_json: "{}",
            expires_at: None,
        })
        .expect("permission inserted");

    let rows = store
        .query_session_status_window("1970-01-01T00:00:00.000000000Z", None, 10)
        .expect("status rows");
    assert_eq!(
        rows[0]
            .pending_permission
            .as_ref()
            .map(|pending| pending.id.as_str()),
        Some(permission.id.as_str())
    );
    assert_eq!(rows[0].last_activity_from, SESSION_ACTIVITY_ACTOR_AGENT);
}

#[test]
fn delete_session_removes_row_prompts_and_events_and_repeats_silently() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    store
        .insert_session(NewSessionRecord {
            id: "sess_doomed".to_owned(),
            agent_id: "fake".to_owned(),
            cwd: "/tmp/doomed".to_owned(),
            title: None,
            metadata_json: "{}".to_owned(),
        })
        .expect("session inserted");
    store
        .insert_prompt(NewPromptRecord {
            id: "prm_doomed".to_owned(),
            session_id: "sess_doomed".to_owned(),
            prompt_json: "[]".to_owned(),
        })
        .expect("prompt inserted");
    store
        .append_session_event_with_source(
            "sess_doomed",
            "info",
            "session.update",
            EVENT_SOURCE_ACP,
            "ACP session update",
            "{}",
        )
        .expect("event appended");

    let deleted = store
        .delete_session("sess_doomed")
        .expect("delete succeeds")
        .expect("record returned");
    assert_eq!(deleted.id, "sess_doomed");

    assert!(
        store
            .get_session("sess_doomed")
            .expect("lookup succeeds")
            .is_none()
    );
    assert!(
        store
            .get_prompt("prm_doomed")
            .expect("prompt lookup succeeds")
            .is_none()
    );
    assert!(
        store
            .latest_session_events("sess_doomed", 10)
            .expect("events lookup succeeds")
            .is_empty()
    );

    // Unknown and already-deleted ids succeed silently.
    assert!(
        store
            .delete_session("sess_doomed")
            .expect("repeat")
            .is_none()
    );
    assert!(
        store
            .delete_session("sess_never")
            .expect("unknown")
            .is_none()
    );
}
