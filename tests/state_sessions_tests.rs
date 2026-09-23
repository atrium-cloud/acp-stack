use acp_stack::state::{
    EVENT_SOURCE_ACP, EVENT_SOURCE_SYSTEM, ListedSessionRecord, NewPermissionRequest,
    NewPromptRecord, NewSessionRecord, PromptRecord, PromptStatus, SESSION_ACTIVITY_ACTOR_AGENT,
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

    // Re-advertising an identical list is a no-op: no row rewrite, no updated_at bump.
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
        .rename_session_target_id("opencode", "claude")
        .expect("target ids should be renamed");
    assert_eq!(renamed, 1);

    let primary_rows = store
        .query_sessions(acp_stack::state::SessionFilter {
            limit: 10,
            target_id: Some("claude"),
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
    // `insert_session_for_target` has no ON CONFLICT, so the UNIQUE(target_id, agent_session_id)
    // index is the sole guard against a duplicate session under one target.
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
    // The rename must fail fast, before any row moves, rather than surface a raw UNIQUE violation
    // mid-move.
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
            "claude",
            "shared_acp".to_owned(),
            NewSessionRecord {
                id: "sess_new".to_owned(),
                agent_id: "claude".to_owned(),
                cwd: "/tmp/new".to_owned(),
                title: None,
                metadata_json: "{}".to_owned(),
            },
        )
        .expect("new target session inserted");

    let result = store.rename_session_target_id("opencode", "claude");
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
fn preceding_prompt_carries_the_anchor_of_the_turn_before() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    store
        .insert_session(NewSessionRecord {
            id: "sess_anchor".to_owned(),
            agent_id: "fake".to_owned(),
            cwd: "/tmp/anchor".to_owned(),
            title: None,
            metadata_json: "{}".to_owned(),
        })
        .expect("session inserted");
    // Prompt ids carry a nanosecond prefix, so these stand in as submission order.
    for id in ["prm_00000001", "prm_00000002", "prm_00000003"] {
        store
            .insert_prompt(NewPromptRecord {
                id: id.to_owned(),
                session_id: "sess_anchor".to_owned(),
                prompt_json: "[]".to_owned(),
            })
            .expect("prompt inserted");
    }
    store
        .record_prompt_agent_message_id("prm_00000002", "msg_turn_two")
        .expect("anchor recorded");

    let preceding = store
        .preceding_prompt("sess_anchor", "prm_00000003")
        .expect("preceding lookup")
        .expect("a preceding prompt exists");
    assert_eq!(preceding.id, "prm_00000002");
    assert_eq!(preceding.agent_message_id.as_deref(), Some("msg_turn_two"));

    // A turn whose adapter never emitted a message id leaves no anchor.
    let preceding = store
        .preceding_prompt("sess_anchor", "prm_00000002")
        .expect("preceding lookup")
        .expect("a preceding prompt exists");
    assert_eq!(preceding.id, "prm_00000001");
    assert_eq!(preceding.agent_message_id, None);

    // The first prompt of the session has nothing before it.
    assert_eq!(
        store
            .preceding_prompt("sess_anchor", "prm_00000001")
            .expect("preceding lookup"),
        None
    );
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

#[test]
fn reconcile_orphaned_sessions_demotes_only_active_rows() {
    let (_tempdir, store) = fresh_state("state.sqlite");
    for id in [
        "sess_orphan_a",
        "sess_orphan_b",
        "sess_closed",
        "sess_avail",
    ] {
        store
            .insert_session(NewSessionRecord {
                id: id.to_owned(),
                agent_id: "fake".to_owned(),
                cwd: "/tmp".to_owned(),
                title: None,
                metadata_json: "{}".to_owned(),
            })
            .expect("session inserted");
    }
    store
        .update_session_status("sess_closed", SESSION_STATUS_CLOSED)
        .expect("closed");
    store
        .update_session_status("sess_avail", SESSION_STATUS_AVAILABLE)
        .expect("available");

    let updated_at_before = store
        .get_session("sess_orphan_a")
        .expect("lookup")
        .expect("session exists")
        .updated_at;
    let mut demoted = store
        .reconcile_orphaned_sessions(None)
        .expect("reconcile succeeds");
    demoted.sort();
    assert_eq!(demoted, vec!["sess_orphan_a", "sess_orphan_b"]);
    // Demotion is not activity: the last-activity timestamp must survive.
    assert_eq!(
        store
            .get_session("sess_orphan_a")
            .expect("lookup")
            .expect("session exists")
            .updated_at,
        updated_at_before
    );
    for (id, expected) in [
        ("sess_orphan_a", SESSION_STATUS_AVAILABLE),
        ("sess_orphan_b", SESSION_STATUS_AVAILABLE),
        ("sess_closed", SESSION_STATUS_CLOSED),
        ("sess_avail", SESSION_STATUS_AVAILABLE),
    ] {
        let record = store
            .get_session(id)
            .expect("lookup")
            .expect("session exists");
        assert_eq!(record.status, expected, "unexpected status for {id}");
    }

    assert!(
        store
            .reconcile_orphaned_sessions(None)
            .expect("repeat reconcile")
            .is_empty()
    );
}

#[test]
fn reconcile_orphaned_sessions_scopes_to_target() {
    let (_tempdir, store) = fresh_state("state.sqlite");
    for (target, session) in [("target_a", "sess_a"), ("target_b", "sess_b")] {
        store
            .insert_session_for_target(
                target,
                session.to_owned(),
                NewSessionRecord {
                    id: session.to_owned(),
                    agent_id: target.to_owned(),
                    cwd: "/tmp".to_owned(),
                    title: None,
                    metadata_json: "{}".to_owned(),
                },
            )
            .expect("session inserted");
    }

    let demoted = store
        .reconcile_orphaned_sessions(Some("target_a"))
        .expect("scoped reconcile succeeds");
    assert_eq!(demoted, vec!["sess_a"]);
    assert_eq!(
        store
            .get_session("sess_b")
            .expect("lookup")
            .expect("session exists")
            .status,
        SESSION_STATUS_ACTIVE
    );
}

#[test]
fn mark_idle_sessions_skips_inflight_and_fresh_rows() {
    let (_tempdir, store) = fresh_state("state.sqlite");
    const OLD: &str = "2020-01-01T00:00:00.000000000Z";
    let threshold = std::time::Duration::from_secs(30);

    // In-flight prompt keeps the session active regardless of age.
    common::state::seed_running_prompt_at(&store, "sess_busy", "prm_busy", OLD);

    // A still-pending prompt shields the same way a running one does.
    store
        .insert_session(NewSessionRecord {
            id: "sess_pending".to_owned(),
            agent_id: "fake".to_owned(),
            cwd: "/tmp".to_owned(),
            title: None,
            metadata_json: "{}".to_owned(),
        })
        .expect("session inserted");
    store
        .insert_prompt(NewPromptRecord {
            id: "prm_pending".to_owned(),
            session_id: "sess_pending".to_owned(),
            prompt_json: "[]".to_owned(),
        })
        .expect("prompt inserted");

    // A pending ACP permission request shields even with no in-flight prompt.
    store
        .insert_session(NewSessionRecord {
            id: "sess_permission".to_owned(),
            agent_id: "fake".to_owned(),
            cwd: "/tmp".to_owned(),
            title: None,
            metadata_json: "{}".to_owned(),
        })
        .expect("session inserted");
    store
        .append_permission_request(NewPermissionRequest {
            source: "acp",
            requester: Some("agent"),
            subject_id: Some("sess_permission"),
            detail_json: "{}",
            expires_at: None,
        })
        .expect("permission inserted");

    // Idle past the threshold with no prompts at all.
    store
        .insert_session(NewSessionRecord {
            id: "sess_idle".to_owned(),
            agent_id: "fake".to_owned(),
            cwd: "/tmp".to_owned(),
            title: None,
            metadata_json: "{}".to_owned(),
        })
        .expect("session inserted");

    // Fresh row stays untouched.
    store
        .insert_session(NewSessionRecord {
            id: "sess_fresh".to_owned(),
            agent_id: "fake".to_owned(),
            cwd: "/tmp".to_owned(),
            title: None,
            metadata_json: "{}".to_owned(),
        })
        .expect("session inserted");

    let connection = rusqlite::Connection::open(store.path()).expect("open sqlite directly");
    connection
        .execute(
            "UPDATE sessions SET updated_at = ?1 WHERE id IN (?2, ?3, ?4, ?5)",
            rusqlite::params![
                OLD,
                "sess_idle",
                "sess_busy",
                "sess_pending",
                "sess_permission"
            ],
        )
        .expect("force-set session updated_at");
    connection
        .execute(
            "UPDATE prompts SET updated_at = ?1 WHERE id = ?2",
            rusqlite::params![OLD, "prm_pending"],
        )
        .expect("force-set pending prompt updated_at");

    let demoted = store.mark_idle_sessions(threshold).expect("sweep succeeds");
    assert_eq!(demoted, vec!["sess_idle"]);
    assert_eq!(
        store
            .get_session("sess_idle")
            .expect("lookup")
            .expect("session exists")
            .updated_at,
        OLD
    );

    // The demotion event must not register as activity in the status window.
    store
        .append_session_event_with_source(
            "sess_idle",
            "info",
            "session.available",
            EVENT_SOURCE_SYSTEM,
            "session available",
            r#"{"reason":"idle"}"#,
        )
        .expect("demotion event appended");
    let window = store
        .query_session_status_window("1970-01-01T00:00:00.000000000Z", None, 10)
        .expect("status window");
    let idle_row = window
        .iter()
        .find(|row| row.id == "sess_idle")
        .expect("demoted session in window");
    assert_eq!(idle_row.last_activity_at, OLD);
    for (id, expected) in [
        ("sess_busy", SESSION_STATUS_ACTIVE),
        ("sess_pending", SESSION_STATUS_ACTIVE),
        ("sess_permission", SESSION_STATUS_ACTIVE),
        ("sess_idle", SESSION_STATUS_AVAILABLE),
        ("sess_fresh", SESSION_STATUS_ACTIVE),
    ] {
        let record = store
            .get_session(id)
            .expect("lookup")
            .expect("session exists");
        assert_eq!(record.status, expected, "unexpected status for {id}");
    }

    // A settled prompt with fresh activity still shields its session.
    store
        .update_prompt_status(
            "prm_busy",
            PromptStatus::Completed,
            Some("end_turn"),
            None,
            None,
            None,
            None,
        )
        .expect("prompt settles");
    assert!(
        store
            .mark_idle_sessions(threshold)
            .expect("repeat sweep")
            .is_empty()
    );
}

// --- Fork inheritance ---

const FORK_PARENT: &str = "sess_fork_parent";
const FORK_CHILD: &str = "sess_fork_child";
/// Kinds a fork child inherits; the fixtures below also write the parent's
/// lifecycle and accounting kinds, which the child must not carry.
const CONVERSATION_KINDS: &[&str] = &[
    "session.update",
    "prompt.inference_failed",
    "prompt.stalled",
    "prompt.errored",
    "session.cancel_requested",
    "terminal.finished",
    "permission.approved",
    "permission.denied",
    "permission.cancelled",
    "permission.expired",
];
/// Keeps rows of consecutive writes on distinct timestamps on coarse clocks,
/// the way a client round trip separates one turn from the next prompt.
const TURN_GAP: std::time::Duration = std::time::Duration::from_millis(2);

struct SeededTurn {
    prompt_id: String,
    message_id: String,
}

fn insert_fork_session(store: &StateStore, id: &str) {
    store
        .insert_session(NewSessionRecord {
            id: id.to_owned(),
            agent_id: "fake".to_owned(),
            cwd: format!("/tmp/{id}"),
            title: None,
            metadata_json: "{}".to_owned(),
        })
        .expect("session inserted");
}

fn fork_child(
    store: &StateStore,
    child_id: &str,
    parent_id: &str,
    held_through_prompt_id: Option<&str>,
) {
    store
        .insert_forked_session(
            "fake",
            format!("agent_{child_id}"),
            NewSessionRecord {
                id: child_id.to_owned(),
                agent_id: "fake".to_owned(),
                cwd: format!("/tmp/{child_id}"),
                title: None,
                metadata_json: "{}".to_owned(),
            },
            parent_id,
            held_through_prompt_id,
        )
        .expect("fork child inserted");
}

fn append_session_event(
    store: &StateStore,
    session_id: &str,
    kind: &str,
    source: &str,
    payload: &str,
) {
    store
        .append_session_event_with_source(session_id, "info", kind, source, kind, payload)
        .expect("event appended");
}

/// One turn written the way the supervisor writes it: the prompt row, its user
/// chunk, then the agent's chunk, leaving the prompt `running`.
fn seed_turn(store: &StateStore, session_id: &str, text: &str) -> SeededTurn {
    std::thread::sleep(TURN_GAP);
    let turn = SeededTurn {
        prompt_id: acp_stack::state::next_prompt_id(),
        message_id: acp_stack::state::next_prompt_message_id(),
    };
    store
        .insert_prompt_with_message_id(
            NewPromptRecord {
                id: turn.prompt_id.clone(),
                session_id: session_id.to_owned(),
                prompt_json: serde_json::json!([{ "type": "text", "text": text }]).to_string(),
            },
            Some(turn.message_id.clone()),
        )
        .expect("prompt inserted");
    let user_chunk = serde_json::json!({
        "sessionId": "agent_parent",
        "update": {
            "sessionUpdate": "user_message_chunk",
            "content": { "type": "text", "text": text },
            "messageId": turn.message_id,
            "_meta": { "acpStack": { "promptId": turn.prompt_id } },
        },
    });
    append_session_event(
        store,
        session_id,
        "session.update",
        EVENT_SOURCE_SYSTEM,
        &user_chunk.to_string(),
    );
    store
        .update_prompt_status(
            &turn.prompt_id,
            PromptStatus::Running,
            None,
            None,
            None,
            None,
            None,
        )
        .expect("prompt running");
    let agent_chunk = serde_json::json!({
        "sessionId": "agent_parent",
        "update": {
            "sessionUpdate": "agent_message_chunk",
            "content": { "type": "text", "text": format!("reply to {text}") },
            "messageId": format!("agent_{}", turn.message_id),
        },
    });
    append_session_event(
        store,
        session_id,
        "session.update",
        EVENT_SOURCE_ACP,
        &agent_chunk.to_string(),
    );
    store
        .acknowledge_prompt_message_id(&turn.prompt_id, &turn.message_id)
        .expect("message id acknowledged");
    store
        .record_prompt_agent_message_id(&turn.prompt_id, &format!("agent_{}", turn.message_id))
        .expect("anchor recorded");
    turn
}

fn complete_turn(store: &StateStore, turn: &SeededTurn) {
    store
        .update_prompt_status(
            &turn.prompt_id,
            PromptStatus::Completed,
            Some("end_turn"),
            None,
            None,
            None,
            None,
        )
        .expect("prompt completed");
}

/// Fail a turn on an upstream 503, with the row and event the supervisor writes.
fn fail_turn_on_inference(store: &StateStore, session_id: &str, turn: &SeededTurn) {
    store
        .update_prompt_status(
            &turn.prompt_id,
            PromptStatus::Errored,
            None,
            Some("agent.inference_5xx"),
            Some("inference endpoint returned 503 (service_unavailable)"),
            Some("inference_5xx"),
            Some(r#"{"status_code":503,"reason_category":"service_unavailable"}"#),
        )
        .expect("prompt errored");
    let payload = serde_json::json!({
        "prompt_id": turn.prompt_id,
        "status_code": 503,
        "reason_category": "service_unavailable",
        "cause": "inference endpoint returned 503 (service_unavailable)",
    });
    append_session_event(
        store,
        session_id,
        "prompt.inference_failed",
        EVENT_SOURCE_SYSTEM,
        &payload.to_string(),
    );
}

/// A parent with three turns, interleaved with the lifecycle and accounting
/// rows a real session accumulates: turn one completes after a permission and
/// a terminal run, turn two fails upstream, turn three completes.
fn seed_fork_parent(store: &StateStore) -> [SeededTurn; 3] {
    insert_fork_session(store, FORK_PARENT);
    append_session_event(
        store,
        FORK_PARENT,
        "session.created",
        EVENT_SOURCE_SYSTEM,
        "{}",
    );
    let first = seed_turn(store, FORK_PARENT, "first turn");
    append_session_event(
        store,
        FORK_PARENT,
        "permission.approved",
        "permission",
        r#"{"id":"perm_1"}"#,
    );
    append_session_event(
        store,
        FORK_PARENT,
        "terminal.finished",
        EVENT_SOURCE_ACP,
        r#"{"terminal_id":"term_1"}"#,
    );
    append_session_event(
        store,
        FORK_PARENT,
        "usage.reported",
        EVENT_SOURCE_ACP,
        r#"{"input_tokens":10}"#,
    );
    append_session_event(
        store,
        FORK_PARENT,
        "tool.execute",
        EVENT_SOURCE_ACP,
        r#"{"command":"ls"}"#,
    );
    complete_turn(store, &first);
    append_session_event(
        store,
        FORK_PARENT,
        "session.config_option_set",
        EVENT_SOURCE_SYSTEM,
        r#"{"config_id":"model"}"#,
    );
    append_session_event(
        store,
        FORK_PARENT,
        "session.fork.created_child",
        EVENT_SOURCE_SYSTEM,
        "{}",
    );
    let second = seed_turn(store, FORK_PARENT, "second turn");
    fail_turn_on_inference(store, FORK_PARENT, &second);
    let third = seed_turn(store, FORK_PARENT, "third turn");
    complete_turn(store, &third);
    [first, second, third]
}

/// Every column a fork copy carries over from its original.
fn event_projection(
    event: &acp_stack::state::Event,
) -> (String, String, String, String, String, String) {
    (
        event.created_at.clone(),
        event.level.clone(),
        event.kind.clone(),
        event.source.clone(),
        event.message.clone(),
        event.payload_json.clone(),
    )
}

fn session_log(store: &StateStore, session_id: &str) -> Vec<acp_stack::state::Event> {
    store
        .query_session_events(session_id, None, 1_000)
        .expect("session events")
}

#[test]
fn a_fork_child_carries_the_parent_conversation_before_the_first_prompt_it_does_not_hold() {
    let (_dir, store) = fresh_state("fork_breakpoint.sqlite");
    let [first, second, third] = seed_fork_parent(&store);
    let third_row = store
        .get_prompt(&third.prompt_id)
        .expect("prompt lookup")
        .expect("third prompt exists");

    // A fork at the third prompt holds the first two turns.
    fork_child(&store, FORK_CHILD, FORK_PARENT, Some(&second.prompt_id));

    let parent_log = session_log(&store, FORK_PARENT);
    let expected: Vec<_> = parent_log
        .iter()
        .filter(|event| event.created_at < third_row.created_at)
        .filter(|event| CONVERSATION_KINDS.contains(&event.kind.as_str()))
        .map(event_projection)
        .collect();
    let child_log = session_log(&store, FORK_CHILD);
    assert_eq!(
        child_log.iter().map(event_projection).collect::<Vec<_>>(),
        expected
    );
    // The held turns' permission, terminal, and failure rows all came along.
    for kind in [
        "permission.approved",
        "terminal.finished",
        "prompt.inference_failed",
    ] {
        assert!(
            child_log.iter().any(|event| event.kind == kind),
            "{kind} missing from {child_log:#?}"
        );
    }
    assert!(
        child_log
            .iter()
            .all(|event| !event.payload_json.contains(&third.prompt_id)),
        "the named prompt's turn stays with the parent: {child_log:#?}"
    );
    assert!(
        child_log
            .iter()
            .all(|event| parent_log.iter().all(|original| original.id != event.id)),
        "copies are rows of their own"
    );

    for turn in [&first, &second] {
        let original = store
            .get_prompt_by_message_id(FORK_PARENT, &turn.message_id)
            .expect("parent prompt lookup")
            .expect("parent prompt exists");
        let inherited = store
            .get_prompt_by_message_id(FORK_CHILD, &turn.message_id)
            .expect("child prompt lookup")
            .expect("the child resolves an inherited message id");
        assert_ne!(inherited.id, original.id);
        assert_eq!(inherited.session_id, FORK_CHILD);
        assert_eq!(
            PromptRecord {
                id: original.id.clone(),
                session_id: original.session_id.clone(),
                ..inherited
            },
            original
        );
    }
    assert!(
        store
            .get_prompt_by_message_id(FORK_CHILD, &third.message_id)
            .expect("child prompt lookup")
            .is_none()
    );

    // The inherited anchor resolves through the child's own prompt order.
    let second_inherited = store
        .get_prompt_by_message_id(FORK_CHILD, &second.message_id)
        .expect("child prompt lookup")
        .expect("second prompt inherited");
    let preceding = store
        .preceding_prompt(FORK_CHILD, &second_inherited.id)
        .expect("preceding lookup")
        .expect("the first inherited prompt precedes the second");
    assert_eq!(
        preceding.agent_message_id,
        Some(format!("agent_{}", first.message_id))
    );
}

#[test]
fn a_prompt_sent_to_a_fork_child_sorts_after_every_inherited_prompt() {
    let (_dir, store) = fresh_state("fork_prompt_order.sqlite");
    let [_, _, third] = seed_fork_parent(&store);
    fork_child(&store, FORK_CHILD, FORK_PARENT, Some(&third.prompt_id));

    let own = seed_turn(&store, FORK_CHILD, "fourth turn");
    let third_inherited = store
        .get_prompt_by_message_id(FORK_CHILD, &third.message_id)
        .expect("child prompt lookup")
        .expect("third prompt inherited");
    let preceding = store
        .preceding_prompt(FORK_CHILD, &own.prompt_id)
        .expect("preceding lookup")
        .expect("an inherited prompt precedes the child's own");
    assert_eq!(preceding.id, third_inherited.id);

    // A fork of the child at its own prompt holds every inherited turn.
    fork_child(
        &store,
        "sess_fork_grandchild",
        FORK_CHILD,
        Some(&third_inherited.id),
    );
    let grandchild_log = session_log(&store, "sess_fork_grandchild");
    let child_log = session_log(&store, FORK_CHILD);
    let own_row = store
        .get_prompt(&own.prompt_id)
        .expect("prompt lookup")
        .expect("own prompt exists");
    assert_eq!(
        grandchild_log
            .iter()
            .map(event_projection)
            .collect::<Vec<_>>(),
        child_log
            .iter()
            .filter(|event| event.created_at < own_row.created_at)
            .map(event_projection)
            .collect::<Vec<_>>()
    );
}

#[test]
fn a_fork_child_reports_idle_until_its_own_first_prompt() {
    let (_dir, store) = fresh_state("fork_status.sqlite");
    let [_, _, third] = seed_fork_parent(&store);
    fork_child(&store, FORK_CHILD, FORK_PARENT, Some(&third.prompt_id));
    let child_status = |store: &StateStore| {
        store
            .query_session_status_window("1970-01-01T00:00:00.000000000Z", None, 10)
            .expect("status rows")
            .into_iter()
            .find(|row| row.id == FORK_CHILD)
            .expect("the child is in the window")
    };

    let fresh = child_status(&store);
    assert_eq!(fresh.latest_prompt, None);
    assert_eq!(fresh.prompt_stream_started_at, None);

    let own = seed_turn(&store, FORK_CHILD, "fourth turn");
    assert_eq!(
        child_status(&store).latest_prompt.map(|prompt| prompt.id),
        Some(own.prompt_id)
    );
}

#[test]
fn a_head_fork_holds_the_settled_turns_before_a_turn_in_flight() {
    let (_dir, store) = fresh_state("fork_head.sqlite");
    insert_fork_session(&store, FORK_PARENT);
    assert_eq!(
        store
            .newest_settled_prompt_id(FORK_PARENT)
            .expect("settled lookup"),
        None
    );
    let first = seed_turn(&store, FORK_PARENT, "first turn");
    complete_turn(&store, &first);
    let second = seed_turn(&store, FORK_PARENT, "second turn");

    let held = store
        .newest_settled_prompt_id(FORK_PARENT)
        .expect("settled lookup");
    assert_eq!(held.as_deref(), Some(first.prompt_id.as_str()));
    fork_child(&store, FORK_CHILD, FORK_PARENT, held.as_deref());
    let child_log = session_log(&store, FORK_CHILD);
    assert_eq!(child_log.len(), 2, "one settled turn: {child_log:#?}");
    assert!(
        child_log
            .iter()
            .all(|event| !event.payload_json.contains(&second.prompt_id)),
        "the running turn stays with the parent: {child_log:#?}"
    );
    assert!(
        store
            .get_prompt_by_message_id(FORK_CHILD, &second.message_id)
            .expect("child prompt lookup")
            .is_none()
    );

    complete_turn(&store, &second);
    assert_eq!(
        store
            .newest_settled_prompt_id(FORK_PARENT)
            .expect("settled lookup")
            .as_deref(),
        Some(second.prompt_id.as_str())
    );
}

#[test]
fn a_fork_of_a_session_with_no_prompts_carries_its_whole_conversation() {
    let (_dir, store) = fresh_state("fork_promptless.sqlite");
    insert_fork_session(&store, FORK_PARENT);
    append_session_event(
        &store,
        FORK_PARENT,
        "session.created",
        EVENT_SOURCE_SYSTEM,
        "{}",
    );
    append_session_event(
        &store,
        FORK_PARENT,
        "session.update",
        EVENT_SOURCE_ACP,
        r#"{"sessionId":"agent_parent","update":{"sessionUpdate":"available_commands_update","availableCommands":[]}}"#,
    );

    fork_child(&store, FORK_CHILD, FORK_PARENT, None);

    let child_log = session_log(&store, FORK_CHILD);
    assert_eq!(child_log.len(), 1, "{child_log:#?}");
    assert_eq!(child_log[0].kind, "session.update");
}

#[test]
fn metrics_count_an_inherited_turn_on_the_session_that_ran_it() {
    let (_dir, store) = fresh_state("fork_metrics.sqlite");
    let [_, _, third] = seed_fork_parent(&store);
    let window = || acp_stack::state::MetricsWindow {
        since: "1970-01-01T00:00:00.000000000Z".to_owned(),
        until: "2999-01-01T00:00:00.000000000Z".to_owned(),
    };
    let before = store.metrics_summary(window()).expect("metrics");

    fork_child(&store, FORK_CHILD, FORK_PARENT, Some(&third.prompt_id));

    let after = store.metrics_summary(window()).expect("metrics");
    assert_eq!(after.turns.total, before.turns.total);
    assert_eq!(after.turns.by_status, before.turns.by_status);
    assert_eq!(after.prompt_failures.total, before.prompt_failures.total);
    assert_eq!(
        after.prompt_failures.by_status_code,
        before.prompt_failures.by_status_code
    );
}

#[test]
fn a_fork_child_and_its_inherited_rows_are_queued_for_the_mirror() {
    let (_dir, mut store) = fresh_state("fork_outbox.sqlite");
    let [first, second, _] = seed_fork_parent(&store);
    store.set_external_logging_enabled(true);

    fork_child(&store, FORK_CHILD, FORK_PARENT, Some(&second.prompt_id));

    let mut queued: Vec<String> = store
        .next_sink_outbox_batch(1_000, "2999-01-01T00:00:00.000000000Z")
        .expect("outbox batch")
        .into_iter()
        .map(|row| row.id)
        .collect();
    let mut expected = vec![format!("sessions:{FORK_CHILD}")];
    for event in session_log(&store, FORK_CHILD) {
        expected.push(format!("events:{}", event.id));
    }
    for turn in [&first, &second] {
        let prompt = store
            .get_prompt_by_message_id(FORK_CHILD, &turn.message_id)
            .expect("child prompt lookup")
            .expect("child prompt exists");
        expected.push(format!("prompts:{}", prompt.id));
    }
    queued.sort();
    expected.sort();
    assert_eq!(queued, expected);
}
