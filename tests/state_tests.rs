use acp_stack::state::{
    AuthFailureFilter, EVENT_KIND_PROMPT_INFERENCE_FAILED, EVENT_SOURCE_ACP, EVENT_SOURCE_SYSTEM,
    Event, EventFilter, FailureClass, INIT_RUN_FAILED, INIT_RUN_SUCCEEDED, INIT_STEP_FAILED,
    INIT_STEP_PENDING, INIT_STEP_RUNNING, INIT_STEP_SKIPPED, INIT_STEP_SUCCEEDED,
    INSTALLER_METHOD_GITHUB, INSTALLER_OPERATION_INSTALL, InstallerRunInput, ListedSessionRecord,
    LogOrder, NewInitRun, NewInitStep, NewPermissionRequest, NewPromptRecord, NewSessionRecord,
    NewStackUpdateRun, PermissionStatus, PromptStatus, SESSION_ACTIVITY_ACTOR_AGENT,
    SESSION_ACTIVITY_ACTOR_USER, SESSION_STATUS_ACTIVE, SESSION_STATUS_AVAILABLE,
    SESSION_STATUS_CLOSED, STACK_UPDATE_OPERATION_CHECK, STACK_UPDATE_STATUS_SUCCEEDED,
    SecurityCategory, StateStore, default_state_path,
};
use rusqlite::Connection;
use rusqlite::params;
use std::str::FromStr;

#[test]
fn resolves_default_state_path_under_home() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = default_state_path(tempdir.path());

    assert_eq!(
        path,
        tempdir
            .path()
            .join(".local")
            .join("share")
            .join("acp-stack")
            .join("state.sqlite")
    );
}

#[test]
fn migrations_are_idempotent() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");

    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("first migration should pass");
    store.migrate().expect("second migration should pass");

    assert_eq!(
        store.schema_version().expect("schema version should load"),
        22
    );
}

#[test]
fn migration_020_adds_prompt_status_window_indexes() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");
    drop(store);

    let connection = Connection::open(&path).expect("sqlite should open for inspection");
    let prompt_index_columns = |index_name: &str| -> Vec<String> {
        connection
            .prepare(&format!("PRAGMA index_info({index_name})"))
            .and_then(|mut statement| {
                let rows = statement.query_map([], |row| row.get::<_, String>(2))?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
            })
            .expect("prompt index columns should query")
    };

    assert_eq!(
        prompt_index_columns("prompts_created_at_idx"),
        vec!["created_at", "session_id", "id"]
    );
    assert_eq!(
        prompt_index_columns("prompts_updated_at_idx"),
        vec!["updated_at", "session_id", "id"]
    );
}

#[test]
fn rejects_unversioned_existing_state_tables() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let connection = Connection::open(&path).expect("sqlite should open");
    connection
        .execute("CREATE TABLE events (id TEXT PRIMARY KEY)", [])
        .expect("malformed table should be created");
    drop(connection);

    let store = StateStore::open(&path).expect("state should open");
    let error = store
        .migrate()
        .expect_err("unversioned managed table should be rejected");

    assert!(error.to_string().contains("existing state table `events`"));
}

#[test]
fn appends_and_queries_events_newest_first() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    store
        .append_event("info", "init.completed", "initialized", "{}")
        .expect("first event should append");
    store
        .append_event("error", "cli.error", "failed", r#"{"command":"status"}"#)
        .expect("second event should append");

    let all = store
        .query_events(EventFilter {
            limit: 10,
            ..EventFilter::default()
        })
        .expect("events should query");
    assert_eq!(all.len(), 2);
    assert!(all[0].created_at.contains('T'));
    assert!(all[0].created_at.ends_with('Z'));
    assert_eq!(all[0].kind, "cli.error");
    assert_eq!(all[1].kind, "init.completed");

    let errors = store
        .query_events(EventFilter {
            limit: 10,
            level: Some("error"),
            ..EventFilter::default()
        })
        .expect("filtered events should query");
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].level, "error");
    assert_eq!(errors[0].message, "failed");
}

#[test]
fn command_output_query_filters_by_command_and_pages_forward() {
    use acp_stack::state::NewCommandRecord;

    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");
    let first = store
        .append_command(NewCommandRecord {
            command: "printf first",
            cwd: None,
            env_json: None,
        })
        .expect("first command");
    let second = store
        .append_command(NewCommandRecord {
            command: "printf second",
            cwd: None,
            env_json: None,
        })
        .expect("second command");

    let stdout = store
        .append_command_output(&first.id, "stdout", 0, "one")
        .expect("stdout output");
    store
        .append_command_output(&second.id, "stdout", 0, "other")
        .expect("other command output");
    let stderr = store
        .append_command_output(&first.id, "stderr", 1, "two")
        .expect("stderr output");

    let first_page = store
        .query_command_output_events(&first.id, 1, None, LogOrder::Asc)
        .expect("first page");
    assert_eq!(first_page.len(), 1);
    assert_eq!(first_page[0].id, stdout.id);
    let second_page = store
        .query_command_output_events(&first.id, 10, Some(&first_page[0].id), LogOrder::Asc)
        .expect("second page");
    assert_eq!(second_page.len(), 1);
    assert_eq!(second_page[0].id, stderr.id);
}

#[test]
fn command_output_and_progress_update_reconnect_fields() {
    use acp_stack::state::NewCommandRecord;

    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");
    let command = store
        .append_command(NewCommandRecord {
            command: "sleep",
            cwd: None,
            env_json: None,
        })
        .expect("command");
    let output = store
        .append_command_output(&command.id, "stdout", 4, "hello")
        .expect("output");

    let after_output = store
        .get_command(&command.id)
        .expect("lookup")
        .expect("command exists");
    assert_eq!(
        after_output.last_output_event_id.as_deref(),
        Some(output.id.as_str())
    );
    assert_eq!(
        after_output.last_output_at.as_deref(),
        Some(output.created_at.as_str())
    );
    assert_eq!(after_output.last_output_seq, Some(4));
    assert_eq!(after_output.output_bytes, 5);
    assert_eq!(
        after_output.last_progress_at.as_deref(),
        Some(output.created_at.as_str())
    );

    let progress = store
        .append_command_progress(&command.id)
        .expect("progress event");
    let after_progress = store
        .get_command(&command.id)
        .expect("lookup")
        .expect("command exists");
    assert_eq!(
        after_progress.last_output_event_id,
        after_output.last_output_event_id
    );
    assert_eq!(
        after_progress.last_progress_at.as_deref(),
        Some(progress.created_at.as_str())
    );
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
fn installer_runs_round_trip_records_and_returns_version() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    store
        .append_installer_run(InstallerRunInput {
            agent_id: "test-agent",
            started_at: "2026-05-21T00:00:00.000000000Z",
            finished_at: Some("2026-05-21T00:00:01.000000000Z"),
            status: "ran",
            stdout: "",
            stderr: "",
            exit_status: Some(0),
            step: "harness",
            version: Some("v1.2.3"),
            operation: INSTALLER_OPERATION_INSTALL,
            method: Some(INSTALLER_METHOD_GITHUB),
            log_dir: None,
            apply_run_id: None,
        })
        .expect("harness row should append");
    store
        .append_installer_run(InstallerRunInput {
            agent_id: "test-agent",
            started_at: "2026-05-21T00:00:02.000000000Z",
            finished_at: Some("2026-05-21T00:00:03.000000000Z"),
            status: "ran",
            stdout: "",
            stderr: "",
            exit_status: Some(0),
            step: "adapter",
            version: None,
            operation: INSTALLER_OPERATION_INSTALL,
            method: None,
            log_dir: None,
            apply_run_id: None,
        })
        .expect("adapter row should append");

    let history = store
        .query_installer_runs(10)
        .expect("history should query");
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].step, "adapter");
    assert_eq!(history[0].agent_id.as_deref(), Some("test-agent"));
    assert!(history[0].version.is_none());
    assert_eq!(history[0].operation, INSTALLER_OPERATION_INSTALL);
    assert!(history[0].method.is_none());
    assert_eq!(history[1].step, "harness");
    assert_eq!(history[1].agent_id.as_deref(), Some("test-agent"));
    assert_eq!(history[1].version.as_deref(), Some("v1.2.3"));
    assert_eq!(history[1].method.as_deref(), Some(INSTALLER_METHOD_GITHUB));

    let latest = store
        .latest_successful_installer_runs_for_agent("test-agent")
        .expect("latest-by-step should query");
    assert_eq!(latest.len(), 2);
    let harness = latest
        .iter()
        .find(|row| row.step == "harness")
        .expect("harness row");
    assert_eq!(harness.version.as_deref(), Some("v1.2.3"));
    let adapter = latest
        .iter()
        .find(|row| row.step == "adapter")
        .expect("adapter row");
    assert!(adapter.version.is_none());
}

#[test]
fn stack_update_runs_round_trip() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    store
        .append_stack_update_run(NewStackUpdateRun {
            operation: STACK_UPDATE_OPERATION_CHECK,
            status: STACK_UPDATE_STATUS_SUCCEEDED,
            current_version: "0.1.0",
            target_version: Some("0.1.1"),
            target_tag: Some("v0.1.1"),
            classification: Some("security-critical"),
            breaking: false,
            major_upgrade: false,
            policy: "security-critical",
            auto: true,
            message: Some("eligible"),
            payload_json: r#"{"decision":"install"}"#,
        })
        .expect("stack update row should append");

    let runs = store
        .query_stack_update_runs(10)
        .expect("stack update runs should query");
    assert_eq!(runs.len(), 1);
    let run = &runs[0];
    assert_eq!(run.operation, STACK_UPDATE_OPERATION_CHECK);
    assert_eq!(run.status, STACK_UPDATE_STATUS_SUCCEEDED);
    assert_eq!(run.current_version, "0.1.0");
    assert_eq!(run.target_version.as_deref(), Some("0.1.1"));
    assert_eq!(run.target_tag.as_deref(), Some("v0.1.1"));
    assert_eq!(run.classification.as_deref(), Some("security-critical"));
    assert!(run.auto);
    assert_eq!(run.policy, "security-critical");
    assert_eq!(run.message.as_deref(), Some("eligible"));
    assert_eq!(run.payload_json, r#"{"decision":"install"}"#);
}

#[test]
fn latest_successful_installer_runs_are_scoped_by_agent_id() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    store
        .append_installer_run(InstallerRunInput {
            agent_id: "first-agent",
            started_at: "2026-05-21T00:00:00.000000000Z",
            finished_at: Some("2026-05-21T00:00:01.000000000Z"),
            status: "ran",
            stdout: "",
            stderr: "",
            exit_status: Some(0),
            step: "harness",
            version: Some("v1.0.0"),
            operation: INSTALLER_OPERATION_INSTALL,
            method: Some(INSTALLER_METHOD_GITHUB),
            log_dir: None,
            apply_run_id: None,
        })
        .expect("first agent row should append");
    store
        .append_installer_run(InstallerRunInput {
            agent_id: "second-agent",
            started_at: "2026-05-21T00:00:02.000000000Z",
            finished_at: Some("2026-05-21T00:00:03.000000000Z"),
            status: "ran",
            stdout: "",
            stderr: "",
            exit_status: Some(0),
            step: "harness",
            version: Some("v9.9.9"),
            operation: INSTALLER_OPERATION_INSTALL,
            method: Some(INSTALLER_METHOD_GITHUB),
            log_dir: None,
            apply_run_id: None,
        })
        .expect("second agent row should append");

    let latest = store
        .latest_successful_installer_runs_for_agent("first-agent")
        .expect("latest-by-step should query");
    assert_eq!(latest.len(), 1);
    assert_eq!(latest[0].agent_id.as_deref(), Some("first-agent"));
    assert_eq!(latest[0].version.as_deref(), Some("v1.0.0"));
}

#[test]
fn installer_runs_round_trip_records_log_dir() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    store
        .append_installer_run(InstallerRunInput {
            agent_id: "test-agent",
            started_at: "2026-05-22T10:00:00.000000000Z",
            finished_at: Some("2026-05-22T10:00:01.000000000Z"),
            status: "ran",
            stdout: "out",
            stderr: "err",
            exit_status: Some(0),
            step: "harness",
            version: Some("v1.0.0"),
            operation: INSTALLER_OPERATION_INSTALL,
            method: Some(INSTALLER_METHOD_GITHUB),
            log_dir: Some("/var/lib/acp-stack/installer-logs/test-agent/2026-05-22T10:00:00.000000000Z/harness"),
            apply_run_id: Some("dap_test"),
        })
        .expect("row with log_dir should append");

    let history = store.query_installer_runs(10).expect("query");
    assert_eq!(history.len(), 1);
    assert_eq!(
        history[0].log_dir.as_deref(),
        Some("/var/lib/acp-stack/installer-logs/test-agent/2026-05-22T10:00:00.000000000Z/harness")
    );
    assert_eq!(history[0].apply_run_id.as_deref(), Some("dap_test"));
}

#[test]
fn latest_successful_installer_runs_skips_failed_rows() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    store
        .append_installer_run(InstallerRunInput {
            agent_id: "test-agent",
            started_at: "2026-05-21T00:00:00.000000000Z",
            finished_at: Some("2026-05-21T00:00:01.000000000Z"),
            status: "ran",
            stdout: "",
            stderr: "",
            exit_status: Some(0),
            step: "install",
            version: Some("v1.0.0"),
            operation: INSTALLER_OPERATION_INSTALL,
            method: Some(INSTALLER_METHOD_GITHUB),
            log_dir: None,
            apply_run_id: None,
        })
        .expect("first ran row should append");
    store
        .append_installer_run(InstallerRunInput {
            agent_id: "test-agent",
            started_at: "2026-05-21T00:00:02.000000000Z",
            finished_at: Some("2026-05-21T00:00:03.000000000Z"),
            status: "failed",
            stdout: "",
            stderr: "boom",
            exit_status: Some(1),
            step: "install",
            version: None,
            operation: INSTALLER_OPERATION_INSTALL,
            method: Some(INSTALLER_METHOD_GITHUB),
            log_dir: None,
            apply_run_id: None,
        })
        .expect("second failed row should append");

    let latest = store
        .latest_successful_installer_runs_for_agent("test-agent")
        .expect("latest-by-step should query");
    assert_eq!(latest.len(), 1);
    assert_eq!(latest[0].status, "ran");
    assert_eq!(latest[0].version.as_deref(), Some("v1.0.0"));
}

#[test]
fn rejects_invalid_event_payload_json() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    let error = store
        .append_event("info", "bad.payload", "bad", "{not json")
        .expect_err("invalid JSON should fail");

    assert!(
        error
            .to_string()
            .contains("event payload must be valid JSON")
    );
}

#[test]
fn rejects_state_database_from_newer_schema_version() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let connection = Connection::open(&path).expect("sqlite should open");
    connection
        .execute_batch(
            r#"
            CREATE TABLE schema_migrations (
                version INTEGER PRIMARY KEY,
                name TEXT NOT NULL,
                applied_at TEXT NOT NULL
            );
            INSERT INTO schema_migrations (version, name, applied_at)
            VALUES (99, '099_future', '2026-05-13T00:00:00Z');
            "#,
        )
        .expect("future migration should be inserted");
    drop(connection);

    let store = StateStore::open(&path).expect("state should open");
    let error = store
        .migrate()
        .expect_err("future schema should be rejected");

    assert!(
        error
            .to_string()
            .contains("state schema version 99 is newer than supported version 22")
    );
}

#[test]
fn each_manifest_migration_applied_exactly_once() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");

    store.migrate().expect("first migrate should pass");
    store
        .migrate()
        .expect("second migrate should be idempotent");
    store.migrate().expect("third migrate should be idempotent");

    let connection = Connection::open(&path).expect("sqlite should open for inspection");
    let row_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM schema_migrations WHERE version = 1",
            [],
            |row| row.get(0),
        )
        .expect("schema_migrations row count should query");
    assert_eq!(row_count, 1, "001_init should appear exactly once");

    let name: String = connection
        .query_row(
            "SELECT name FROM schema_migrations WHERE version = 1",
            [],
            |row| row.get(0),
        )
        .expect("name should query");
    assert_eq!(name, "init");
}

#[test]
fn migrate_fails_when_baseline_tables_missing_for_recorded_version() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let connection = Connection::open(&path).expect("sqlite should open");
    connection
        .execute_batch(
            r#"
            CREATE TABLE schema_migrations (
                version INTEGER PRIMARY KEY,
                name TEXT NOT NULL,
                applied_at TEXT NOT NULL
            );
            INSERT INTO schema_migrations (version, name, applied_at)
            VALUES (1, 'init', '2026-05-13T00:00:00Z');
            "#,
        )
        .expect("preexisting migration row should insert");
    drop(connection);

    let store = StateStore::open(&path).expect("state should open");
    let error = store
        .migrate()
        .expect_err("missing baseline tables should be rejected");

    assert!(
        error
            .to_string()
            .contains("state database is missing the required `events` table"),
        "{error}",
    );
}

#[test]
fn migration_002_preserves_legacy_auth_failure_rows() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let connection = Connection::open(&path).expect("sqlite should open");
    connection
        .execute_batch(include_str!("../migrations/001_init.sqlite.sql"))
        .expect("001 schema should apply");
    connection
        .execute_batch(
            r#"
            CREATE TABLE schema_migrations (
                version INTEGER PRIMARY KEY,
                name TEXT NOT NULL,
                applied_at TEXT NOT NULL
            );
            INSERT INTO schema_migrations (version, name, applied_at)
            VALUES (1, 'init', '2026-05-13T00:00:00Z');
            INSERT INTO auth_failures (id, created_at, client_label, reason)
            VALUES ('legacy_af_1', '2026-05-13T01:02:03.000000000Z', '127.0.0.1', 'invalid');
            "#,
        )
        .expect("legacy state should be seeded");
    drop(connection);

    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    let rows = store
        .query_auth_failures(AuthFailureFilter {
            limit: 10,
            ..AuthFailureFilter::default()
        })
        .expect("auth failures should query");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, "legacy_af_1");
    assert_eq!(rows[0].key_kind, "unknown");
    assert_eq!(rows[0].reason, "invalid");
    assert_eq!(rows[0].client_ip.as_deref(), Some("127.0.0.1"));
    assert!(rows[0].route.is_none());

    let payload: serde_json::Value =
        serde_json::from_str(&rows[0].payload_json).expect("payload should parse");
    assert_eq!(payload["legacy_client_label"], "127.0.0.1");
    assert_eq!(payload["reason"], "invalid");
}

#[test]
fn migration_022_backfills_array_session_columns() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let connection = Connection::open(&path).expect("sqlite should open");
    let migrations = [
        include_str!("../migrations/001_init.sqlite.sql"),
        include_str!("../migrations/002_auth_failures_schema.sqlite.sql"),
        include_str!("../migrations/003_agent_capabilities.sqlite.sql"),
        include_str!("../migrations/004_sessions.sqlite.sql"),
        include_str!("../migrations/005_commands_schema.sqlite.sql"),
        include_str!("../migrations/006_permissions.sqlite.sql"),
        include_str!("../migrations/007_events_source.sqlite.sql"),
        include_str!("../migrations/008_sink_outbox.sqlite.sql"),
        include_str!("../migrations/009_installer_runs_step.sqlite.sql"),
        include_str!("../migrations/010_installer_runs_version.sqlite.sql"),
        include_str!("../migrations/011_installer_runs_log_dir.sqlite.sql"),
        include_str!("../migrations/012_init_runs.sqlite.sql"),
        include_str!("../migrations/013_installer_runs_apply_run_id.sqlite.sql"),
        include_str!("../migrations/014_security_runs.sqlite.sql"),
        include_str!("../migrations/015_prompts_lifecycle_extension.sqlite.sql"),
        include_str!("../migrations/016_command_output_reconnect.sqlite.sql"),
        include_str!("../migrations/017_prompt_message_ids.sqlite.sql"),
        include_str!("../migrations/018_installer_runs_operation_method.sqlite.sql"),
        include_str!("../migrations/019_stack_update_runs.sqlite.sql"),
        include_str!("../migrations/020_prompt_status_indexes.sqlite.sql"),
        include_str!("../migrations/021_auth_keys.sqlite.sql"),
    ];
    for migration in migrations {
        connection
            .execute_batch(migration)
            .expect("legacy migration should apply");
    }
    connection
        .execute_batch(
            r#"
            CREATE TABLE schema_migrations (
                version INTEGER PRIMARY KEY,
                name TEXT NOT NULL,
                applied_at TEXT NOT NULL
            );
            INSERT INTO schema_migrations (version, name, applied_at)
            VALUES
                (1, 'm1', '2026-05-13T00:00:00Z'),
                (2, 'm2', '2026-05-13T00:00:00Z'),
                (3, 'm3', '2026-05-13T00:00:00Z'),
                (4, 'm4', '2026-05-13T00:00:00Z'),
                (5, 'm5', '2026-05-13T00:00:00Z'),
                (6, 'm6', '2026-05-13T00:00:00Z'),
                (7, 'm7', '2026-05-13T00:00:00Z'),
                (8, 'm8', '2026-05-13T00:00:00Z'),
                (9, 'm9', '2026-05-13T00:00:00Z'),
                (10, 'm10', '2026-05-13T00:00:00Z'),
                (11, 'm11', '2026-05-13T00:00:00Z'),
                (12, 'm12', '2026-05-13T00:00:00Z'),
                (13, 'm13', '2026-05-13T00:00:00Z'),
                (14, 'm14', '2026-05-13T00:00:00Z'),
                (15, 'm15', '2026-05-13T00:00:00Z'),
                (16, 'm16', '2026-05-13T00:00:00Z'),
                (17, 'm17', '2026-05-13T00:00:00Z'),
                (18, 'm18', '2026-05-13T00:00:00Z'),
                (19, 'm19', '2026-05-13T00:00:00Z'),
                (20, 'm20', '2026-05-13T00:00:00Z'),
                (21, 'm21', '2026-05-13T00:00:00Z');
            INSERT INTO sessions
                (id, created_at, updated_at, status, agent_id, cwd, title, metadata_json)
            VALUES
                ('local_session', '2026-05-13T00:00:00Z', '2026-05-13T00:00:00Z', 'active', 'opencode', '/workspace', NULL, '{}');
            "#,
        )
        .expect("legacy state should be seeded");
    drop(connection);

    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");
    let session = store
        .get_session("local_session")
        .expect("session should query")
        .expect("session should exist");

    assert_eq!(session.target_id, "opencode");
    assert_eq!(session.agent_session_id, "local_session");
}

#[test]
fn auth_failure_filter_order_asc_returns_oldest_first_and_cursor_advances_forward() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    for reason in ["missing", "invalid", "blocked"] {
        store
            .append_auth_failure("session", reason, None, Some("/v1/test"), "{}")
            .expect("auth failure should append");
    }

    let first_page = store
        .query_auth_failures(AuthFailureFilter {
            limit: 2,
            order: LogOrder::Asc,
            ..AuthFailureFilter::default()
        })
        .expect("asc auth failures should query");
    assert_eq!(first_page.len(), 2);
    assert_eq!(first_page[0].reason, "missing");
    assert_eq!(first_page[1].reason, "invalid");

    let cursor = first_page.last().expect("cursor row").id.clone();
    let second_page = store
        .query_auth_failures(AuthFailureFilter {
            limit: 2,
            after_id: Some(&cursor),
            order: LogOrder::Asc,
            ..AuthFailureFilter::default()
        })
        .expect("asc auth failures page should advance");
    assert_eq!(second_page.len(), 1);
    assert_eq!(second_page[0].reason, "blocked");
}

#[test]
fn agent_lifecycle_round_trips_through_sqlite() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    let event = store
        .append_agent_lifecycle(
            "server.started",
            "listening on 127.0.0.1:7700",
            r#"{"bind":"127.0.0.1:7700"}"#,
        )
        .expect("agent lifecycle event should append");
    assert!(event.id.starts_with("agl_"));
    assert!(event.created_at.contains('T'));

    let connection = Connection::open(&path).expect("sqlite should open for inspection");
    let stored: (String, String, String, String) = connection
        .query_row(
            "SELECT event_kind, message, payload_json, created_at FROM agent_lifecycle WHERE id = ?1",
            params![event.id.clone()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("row should be readable");
    assert_eq!(stored.0, "server.started");
    assert_eq!(stored.1, "listening on 127.0.0.1:7700");
    assert_eq!(stored.2, r#"{"bind":"127.0.0.1:7700"}"#);
    assert_eq!(stored.3, event.created_at);
}

#[test]
fn latest_agent_failure_filters_by_agent_and_extracts_reason() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    store
        .append_agent_lifecycle(
            "agent.spawn_failed",
            "agent spawn failed",
            r#"{"agent_id":"other","reason":"wrong agent"}"#,
        )
        .expect("other failure");
    let failure = store
        .append_agent_lifecycle(
            "agent.spawn_failed",
            "agent spawn failed",
            r#"{"agent_id":"opencode","reason":"binary not found"}"#,
        )
        .expect("target failure");
    let restart_failure = store
        .append_agent_lifecycle(
            "agent.restart_failed",
            "agent restart failed",
            r#"{"agent_id":"opencode","reason":"restart binary not found"}"#,
        )
        .expect("restart failure");

    let latest = store
        .latest_agent_failure("opencode")
        .expect("query latest")
        .expect("failure row");
    assert_ne!(latest.id, failure.id);
    assert_eq!(latest.id, restart_failure.id);
    assert_eq!(latest.event_kind, "agent.restart_failed");
    assert_eq!(latest.reason, "restart binary not found");
    assert!(
        store
            .latest_agent_failure("missing")
            .expect("query missing")
            .is_none()
    );
}

#[test]
fn agent_lifecycle_rejects_invalid_payload_json() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    let error = store
        .append_agent_lifecycle("server.starting", "starting", "{not json")
        .expect_err("invalid JSON payload should fail");
    assert!(
        error
            .to_string()
            .contains("event payload must be valid JSON")
    );
}

#[test]
fn event_ids_stay_sorted_when_appended_in_quick_succession() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    let mut last_id: Option<String> = None;
    for index in 0..200 {
        let event = store
            .append_event("info", "test.burst", &format!("event {index}"), "{}")
            .expect("event should append");
        if let Some(prev) = &last_id {
            assert!(
                prev < &event.id,
                "event ids must be strictly increasing: prev={prev} curr={curr}",
                curr = event.id,
            );
        }
        last_id = Some(event.id);
    }

    let descending = store
        .query_events(EventFilter {
            limit: 200,
            ..EventFilter::default()
        })
        .expect("events should query");
    // Newest-first ordering should match the reverse insertion order.
    assert_eq!(descending.len(), 200);
    for window in descending.windows(2) {
        assert!(
            window[0].id > window[1].id,
            "descending query must yield strictly decreasing ids",
        );
    }
}

fn fresh_state(name: &str) -> (tempfile::TempDir, StateStore) {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let path = tempdir.path().join(name);
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migrate");
    (tempdir, store)
}

fn insert_state_test_session(store: &StateStore, session_id: &str) {
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

// ----- LogFilter / source / metrics tests (Phase 3 batch A+B) ----------------

#[test]
fn append_event_default_source_is_system() {
    let (_dir, store) = fresh_state("source_default.sqlite");
    let event = store
        .append_event("info", "test.kind", "msg", "{}")
        .expect("append");
    assert_eq!(event.source, "system");
}

#[test]
fn append_event_with_source_round_trips_label() {
    let (_dir, store) = fresh_state("source_round_trip.sqlite");
    let event = store
        .append_event_with_source("info", "test.kind", "api", "msg", "{}")
        .expect("append");
    assert_eq!(event.source, "api");
    let events = store
        .query_events(EventFilter {
            limit: 10,
            source: Some("api"),
            ..EventFilter::default()
        })
        .expect("query");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].id, event.id);
}

#[test]
fn log_filter_kind_prefix_matches_dotted_namespace() {
    let (_dir, store) = fresh_state("kind_prefix.sqlite");
    store
        .append_event("info", "command.started", "", "{}")
        .unwrap();
    store
        .append_event("info", "command.exited", "", "{}")
        .unwrap();
    store
        .append_event("info", "session.update", "", "{}")
        .unwrap();
    let rows = store
        .query_events(EventFilter {
            limit: 100,
            kind_prefix: Some("command."),
            ..EventFilter::default()
        })
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| r.kind.starts_with("command.")));
}

#[test]
fn log_filter_session_id_predicate_only_returns_matching_rows() {
    let (_dir, store) = fresh_state("session_filter.sqlite");
    store
        .append_session_event("sess_a", "info", "session.update", "", "{}")
        .unwrap();
    store
        .append_session_event("sess_b", "info", "session.update", "", "{}")
        .unwrap();
    store.append_event("info", "system.note", "", "{}").unwrap();
    let rows = store
        .query_events(EventFilter {
            limit: 100,
            session_id: Some("sess_a"),
            ..EventFilter::default()
        })
        .unwrap();
    assert_eq!(rows.len(), 1);
}

#[test]
fn log_filter_command_id_payload_correlation() {
    let (_dir, store) = fresh_state("command_filter.sqlite");
    let payload_match = serde_json::json!({"command_id": "cmd_match"}).to_string();
    let payload_other = serde_json::json!({"command_id": "cmd_other"}).to_string();
    store
        .append_event_with_source("info", "command.started", "command", "", &payload_match)
        .unwrap();
    store
        .append_event_with_source("info", "command.started", "command", "", &payload_other)
        .unwrap();
    let rows = store
        .query_events(EventFilter {
            limit: 10,
            command_id: Some("cmd_match"),
            ..EventFilter::default()
        })
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].payload_json.contains("cmd_match"));
}

#[test]
fn log_filter_permission_id_matches_legacy_id_payload() {
    let (_dir, store) = fresh_state("permission_legacy_filter.sqlite");
    let payload_match = serde_json::json!({"id": "perm_match"}).to_string();
    let payload_other = serde_json::json!({"id": "perm_other"}).to_string();
    store
        .append_event_with_source(
            "info",
            "permission.expired",
            "permission",
            "",
            &payload_match,
        )
        .unwrap();
    store
        .append_event_with_source(
            "info",
            "permission.expired",
            "permission",
            "",
            &payload_other,
        )
        .unwrap();
    store
        .append_event_with_source("info", "system.note", "system", "", &payload_match)
        .unwrap();
    let rows = store
        .query_events(EventFilter {
            limit: 10,
            permission_id: Some("perm_match"),
            ..EventFilter::default()
        })
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].payload_json.contains("perm_match"));
}

#[test]
fn log_filter_since_until_window_excludes_rows_outside_range() {
    let (_dir, store) = fresh_state("time_range.sqlite");
    // Seed events with explicit timestamps so the window is deterministic.
    let connection = rusqlite::Connection::open(_dir.path().join("time_range.sqlite")).unwrap();
    connection
        .execute(
            "INSERT INTO events (id, created_at, level, kind, message, payload_json, source) \
             VALUES ('e_old', '2026-05-10T00:00:00.000000000Z', 'info', 'x', '', '{}', 'system'), \
                    ('e_mid', '2026-05-14T12:00:00.000000000Z', 'info', 'x', '', '{}', 'system'), \
                    ('e_new', '2026-05-16T00:00:00.000000000Z', 'info', 'x', '', '{}', 'system')",
            [],
        )
        .unwrap();
    let rows = store
        .query_events(EventFilter {
            limit: 100,
            since: Some("2026-05-14T00:00:00Z"),
            until: Some("2026-05-15T00:00:00Z"),
            ..EventFilter::default()
        })
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, "e_mid");
}

#[test]
fn log_filter_cursor_paginates_across_timestamp_ties() {
    let (_dir, store) = fresh_state("cursor.sqlite");
    // Three events with the same timestamp — the cursor must still progress.
    let connection = rusqlite::Connection::open(_dir.path().join("cursor.sqlite")).unwrap();
    connection
        .execute(
            "INSERT INTO events (id, created_at, level, kind, message, payload_json, source) \
             VALUES ('e_1', '2026-05-15T00:00:00.000000000Z', 'info', 'x', '', '{}', 'system'), \
                    ('e_2', '2026-05-15T00:00:00.000000000Z', 'info', 'x', '', '{}', 'system'), \
                    ('e_3', '2026-05-15T00:00:00.000000000Z', 'info', 'x', '', '{}', 'system')",
            [],
        )
        .unwrap();
    let first_page = store
        .query_events(EventFilter {
            limit: 2,
            ..EventFilter::default()
        })
        .unwrap();
    assert_eq!(first_page.len(), 2);
    let cursor = first_page.last().unwrap().id.clone();
    let second_page = store
        .query_events(EventFilter {
            limit: 2,
            after_id: Some(&cursor),
            ..EventFilter::default()
        })
        .unwrap();
    assert_eq!(second_page.len(), 1);
    assert_ne!(second_page[0].id, cursor);
}

#[test]
fn metrics_summary_aggregates_within_window() {
    use acp_stack::state::{MetricsWindow, NewCommandRecord};
    let (_dir, store) = fresh_state("metrics.sqlite");
    // Seed API request events plus one command and one auth_failure inside the window.
    store
        .append_event_with_source(
            "info",
            "api.request",
            "api",
            "",
            r#"{"method":"GET","path":"/v1/sessions/{id}","status":200,"duration_ms":42,"key_kind":"session","origin":{"origin_kind":"cloudflare","country_code":"US","region_code":"CA"}}"#,
        )
        .unwrap();
    store
        .append_event_with_source(
            "info",
            "api.request",
            "local",
            "",
            r#"{"method":"POST","path":"/v1/commands","status":404,"duration_ms":62,"key_kind":null,"origin":{"origin_kind":"direct"}}"#,
        )
        .unwrap();
    store
        .append_command(NewCommandRecord {
            command: "echo hi",
            cwd: None,
            env_json: None,
        })
        .unwrap();
    store
        .append_auth_failure("session", "invalid", None, Some("/v1/x"), "{}")
        .unwrap();
    let now = chrono::Utc::now();
    let since =
        (now - chrono::Duration::hours(1)).to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    let until =
        (now + chrono::Duration::minutes(5)).to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    let summary = store
        .metrics_summary(MetricsWindow { since, until })
        .unwrap();
    assert_eq!(summary.commands.total, 1);
    assert_eq!(summary.security.auth_failures, 1);
    assert_eq!(summary.api_connections.request_count, Some(2));
    assert_eq!(
        summary
            .api_connections
            .by_status
            .get("2xx")
            .copied()
            .unwrap_or(0),
        1
    );
    assert_eq!(
        summary.api_connections.by_status.get("4xx").copied(),
        Some(1)
    );
    assert_eq!(
        summary.api_connections.by_method.get("GET").copied(),
        Some(1)
    );
    assert_eq!(
        summary.api_connections.by_method.get("POST").copied(),
        Some(1)
    );
    assert_eq!(
        summary
            .api_connections
            .by_route
            .get("/v1/sessions/{id}")
            .copied(),
        Some(1)
    );
    assert_eq!(
        summary.api_connections.by_key_kind.get("session").copied(),
        Some(1)
    );
    assert_eq!(
        summary.api_connections.by_key_kind.get("unknown").copied(),
        Some(1)
    );
    assert_eq!(
        summary.api_connections.by_source.get("api").copied(),
        Some(1)
    );
    assert_eq!(
        summary.api_connections.by_source.get("local").copied(),
        Some(1)
    );
    assert_eq!(
        summary
            .api_connections
            .by_origin_kind
            .get("cloudflare")
            .copied(),
        Some(1)
    );
    assert_eq!(
        summary
            .api_connections
            .by_origin_kind
            .get("direct")
            .copied(),
        Some(1)
    );
    assert_eq!(
        summary.api_connections.by_country.get("US").copied(),
        Some(1)
    );
    assert_eq!(
        summary.api_connections.by_country.get("unknown").copied(),
        Some(1)
    );
    assert_eq!(
        summary.api_connections.by_region.get("CA").copied(),
        Some(1)
    );
    assert_eq!(
        summary.api_connections.by_region.get("unknown").copied(),
        Some(1)
    );
    assert_eq!(summary.api_connections.average_duration_ms, Some(52));
}

#[test]
fn metrics_summary_exposes_usage_and_websocket_metrics() {
    use acp_stack::state::MetricsWindow;
    let (_dir, store) = fresh_state("metrics_usage_ws.sqlite");
    store
        .append_event_with_source(
            "info",
            "usage.reported",
            "acp",
            "",
            r#"{"input_tokens":123,"output_tokens":45,"context_window_max":8192}"#,
        )
        .unwrap();
    store
        .append_event_with_source(
            "info",
            "usage.reported",
            "acp",
            "",
            r#"{"input_tokens":7,"output_tokens":5,"context_window_max":32768}"#,
        )
        .unwrap();
    store
        .append_event_with_source("info", "ws.client_connected", "api", "", "{}")
        .unwrap();
    store
        .append_event_with_source(
            "info",
            "ws.client_disconnected",
            "api",
            "",
            r#"{"duration_ms":250}"#,
        )
        .unwrap();
    store
        .append_event_with_source(
            "info",
            "ws.client_disconnected",
            "api",
            "",
            r#"{"duration_ms":750}"#,
        )
        .unwrap();

    let now = chrono::Utc::now();
    let since =
        (now - chrono::Duration::hours(1)).to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    let until =
        (now + chrono::Duration::minutes(5)).to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    let summary = store
        .metrics_summary(MetricsWindow { since, until })
        .unwrap();

    assert_eq!(summary.usage.tokens_input, Some(130));
    assert_eq!(summary.usage.tokens_output, Some(50));
    assert_eq!(summary.usage.context_window_max, Some(32768));
    assert_eq!(summary.ws_connections.connections_opened, Some(1));
    assert_eq!(summary.ws_connections.connections_closed, Some(2));
    assert_eq!(summary.ws_connections.average_duration_ms, Some(500));
}

#[test]
fn metrics_summary_exposes_prompt_failure_counters() {
    use acp_stack::state::{MetricsWindow, NewCommandRecord};
    let (_dir, store) = fresh_state("metrics_prompt_failures.sqlite");
    store
        .insert_session(NewSessionRecord {
            id: "sess_metrics_failures".to_owned(),
            agent_id: "fake".to_owned(),
            cwd: "/tmp".to_owned(),
            title: None,
            metadata_json: "{}".to_owned(),
        })
        .expect("session inserted");

    for (prompt_id, status, failure_class) in [
        (
            "prm_inference_5xx",
            PromptStatus::Errored,
            FailureClass::Inference5xx,
        ),
        (
            "prm_agent_process",
            PromptStatus::Errored,
            FailureClass::AgentProcess,
        ),
        ("prm_stalled", PromptStatus::Stalled, FailureClass::Stalled),
    ] {
        store
            .insert_prompt(NewPromptRecord {
                id: prompt_id.to_owned(),
                session_id: "sess_metrics_failures".to_owned(),
                prompt_json: "[]".to_owned(),
            })
            .expect("prompt inserted");
        assert!(
            store
                .update_prompt_status(
                    prompt_id,
                    status,
                    None,
                    Some("prompt.failed"),
                    Some("prompt failed"),
                    Some(failure_class.as_str()),
                    None,
                )
                .expect("prompt terminal update"),
            "terminal update for {prompt_id} should apply"
        );
    }
    store
        .append_session_event_with_source(
            "sess_metrics_failures",
            "warn",
            EVENT_KIND_PROMPT_INFERENCE_FAILED,
            EVENT_SOURCE_SYSTEM,
            "inference endpoint failure",
            r#"{"prompt_id":"prm_inference_5xx","status_code":503,"reason_category":"service_unavailable"}"#,
        )
        .expect("inference event inserted");
    store
        .append_command(NewCommandRecord {
            command: "echo keep window nonempty",
            cwd: None,
            env_json: None,
        })
        .expect("command inserted");

    let now = chrono::Utc::now();
    let since =
        (now - chrono::Duration::hours(1)).to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    let until =
        (now + chrono::Duration::minutes(5)).to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    let summary = store
        .metrics_summary(MetricsWindow { since, until })
        .unwrap();

    assert_eq!(summary.prompt_failures.total, 3);
    assert_eq!(summary.prompt_failures.inference_5xx, 1);
    assert_eq!(summary.prompt_failures.agent_process, 1);
    assert_eq!(summary.prompt_failures.stalled, 1);
    assert_eq!(
        summary
            .prompt_failures
            .by_class
            .get(FailureClass::Inference5xx.as_str())
            .copied(),
        Some(1)
    );
    assert_eq!(
        summary.prompt_failures.by_status_code.get("503").copied(),
        Some(1)
    );
    assert_eq!(
        summary
            .prompt_failures
            .by_reason_category
            .get("service_unavailable")
            .copied(),
        Some(1)
    );
}

#[test]
fn metrics_summary_returns_zero_when_window_misses_all_rows() {
    use acp_stack::state::MetricsWindow;
    let (_dir, store) = fresh_state("metrics_empty.sqlite");
    store.append_event("info", "x.y", "", "{}").unwrap();
    let summary = store
        .metrics_summary(MetricsWindow {
            since: "2000-01-01T00:00:00.000000000Z".to_owned(),
            until: "2000-01-02T00:00:00.000000000Z".to_owned(),
        })
        .unwrap();
    assert_eq!(summary.counts.events, 0);
    // Usage remains optional because agents may never emit it. API request
    // instrumentation is part of the running binary, so a quiet window reports
    // an explicit zero.
    assert!(summary.usage.tokens_input.is_none());
    assert_eq!(summary.api_connections.request_count, Some(0));
    assert_eq!(summary.prompt_failures.total, 0);
}

#[test]
fn init_run_records_round_trip_with_steps() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    let run = store
        .create_init_run(NewInitRun {
            runtime_user: Some("acp"),
            agent_id: Some("codex"),
            args_json: r#"{"agent":"codex"}"#,
        })
        .expect("init run should append");
    assert_eq!(run.status, "pending");
    assert!(run.id.starts_with("irun_"));

    let step = store
        .append_init_step(NewInitStep {
            run_id: &run.id,
            ordinal: 1,
            kind: "agent_install",
            payload_json: r#"{"step":"agent_install"}"#,
        })
        .expect("step should append");
    assert_eq!(step.status, INIT_STEP_PENDING);

    store
        .mark_init_step_running(&step.id)
        .expect("running mark should succeed");
    store
        .mark_init_step_succeeded(
            &step.id,
            Some("/tmp/install-logs/agent_install"),
            r#"{"installer_run_id":"ins_abc"}"#,
        )
        .expect("succeeded mark should succeed");

    let steps = store.query_init_steps(&run.id).expect("steps should query");
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].status, INIT_STEP_SUCCEEDED);
    assert_eq!(
        steps[0].log_dir.as_deref(),
        Some("/tmp/install-logs/agent_install"),
    );
    assert!(steps[0].started_at.is_some());
    assert!(steps[0].finished_at.is_some());

    store
        .finalize_init_run(&run.id, INIT_RUN_SUCCEEDED)
        .expect("finalize should succeed");
    let reloaded = store
        .lookup_init_run(&run.id)
        .expect("lookup should succeed")
        .expect("run row should exist");
    assert_eq!(reloaded.status, INIT_RUN_SUCCEEDED);
    assert!(reloaded.finished_at.is_some());
}

#[test]
fn init_step_skipped_keeps_started_at_and_clears_error() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    let run = store
        .create_init_run(NewInitRun {
            runtime_user: None,
            agent_id: None,
            args_json: "{}",
        })
        .expect("init run should append");
    let step = store
        .append_init_step(NewInitStep {
            run_id: &run.id,
            ordinal: 1,
            kind: "config_validate",
            payload_json: "{}",
        })
        .expect("step should append");

    store
        .mark_init_step_running(&step.id)
        .expect("running mark");
    store
        .mark_init_step_failed(
            &step.id,
            None,
            "config.invalid",
            "missing field foo",
            r#"{"attempt":1}"#,
        )
        .expect("failed mark");

    let steps = store.query_init_steps(&run.id).expect("steps");
    assert_eq!(steps[0].status, INIT_STEP_FAILED);
    assert_eq!(steps[0].error_kind.as_deref(), Some("config.invalid"));

    // Re-run: verifier-skipped path must clear the prior error tuple.
    store
        .mark_init_step_skipped(&step.id, r#"{"attempt":1,"verified":true}"#)
        .expect("skipped mark");
    let steps = store.query_init_steps(&run.id).expect("steps reloaded");
    assert_eq!(steps[0].status, INIT_STEP_SKIPPED);
    assert!(steps[0].error_kind.is_none());
    assert!(steps[0].error_detail.is_none());
    assert!(steps[0].payload_json.contains("\"verified\":true"));
}

#[test]
fn init_run_finalize_failed_records_terminal_status() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    let run = store
        .create_init_run(NewInitRun {
            runtime_user: None,
            agent_id: None,
            args_json: "{}",
        })
        .expect("init run should append");
    let step = store
        .append_init_step(NewInitStep {
            run_id: &run.id,
            ordinal: 1,
            kind: "agent_install",
            payload_json: "{}",
        })
        .expect("step should append");
    store.mark_init_step_running(&step.id).expect("running");
    store
        .mark_init_step_failed(&step.id, None, "installer.exit_nonzero", "exit=1", "{}")
        .expect("failed");
    store
        .finalize_init_run(&run.id, INIT_RUN_FAILED)
        .expect("finalize failed");

    let latest = store
        .latest_init_run()
        .expect("latest")
        .expect("latest row");
    assert_eq!(latest.id, run.id);
    assert_eq!(latest.status, INIT_RUN_FAILED);
    let _ = INIT_STEP_RUNNING;
}

#[test]
fn init_step_payload_must_be_valid_json() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    let run = store
        .create_init_run(NewInitRun {
            runtime_user: None,
            agent_id: None,
            args_json: "{}",
        })
        .expect("init run");
    let error = store
        .append_init_step(NewInitStep {
            run_id: &run.id,
            ordinal: 1,
            kind: "agent_install",
            payload_json: "not json",
        })
        .expect_err("invalid payload should be rejected");
    assert!(error.to_string().to_lowercase().contains("json"));
}

#[test]
fn duplicate_ordinal_within_run_is_rejected() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    let run = store
        .create_init_run(NewInitRun {
            runtime_user: None,
            agent_id: None,
            args_json: "{}",
        })
        .expect("init run");
    store
        .append_init_step(NewInitStep {
            run_id: &run.id,
            ordinal: 1,
            kind: "agent_install",
            payload_json: "{}",
        })
        .expect("first step");
    let error = store
        .append_init_step(NewInitStep {
            run_id: &run.id,
            ordinal: 1,
            kind: "config_validate",
            payload_json: "{}",
        })
        .expect_err("duplicate ordinal should fail UNIQUE");
    assert!(error.to_string().to_lowercase().contains("unique"));
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
fn migration_015_accepts_every_lifecycle_status() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    store
        .insert_session(NewSessionRecord {
            id: "sess_all_statuses".to_owned(),
            agent_id: "fake".to_owned(),
            cwd: "/tmp/all".to_owned(),
            title: None,
            metadata_json: "{}".to_owned(),
        })
        .expect("session inserted");

    let statuses = [
        "pending",
        "running",
        "completed",
        "errored",
        "cancelled",
        "stalled",
    ];
    for status in statuses {
        let id = format!("prm_{status}");
        store
            .insert_prompt(NewPromptRecord {
                id: id.clone(),
                session_id: "sess_all_statuses".to_owned(),
                prompt_json: "[]".to_owned(),
            })
            .expect("prompt inserted");
        // insert_prompt always writes 'pending'; flip to the target status
        // through update_prompt_status. PromptStatus::from_str guards the
        // matrix and `terminal()` is enforced by callers, not the DB.
        let prompt_status =
            PromptStatus::from_str(status).expect("status should round-trip via PromptStatus");
        if prompt_status != PromptStatus::Pending {
            store
                .update_prompt_status(&id, prompt_status, None, None, None, None, None)
                .unwrap_or_else(|err| panic!("status {status} should be accepted: {err}"));
        }
        let prompt = store
            .get_prompt(&id)
            .expect("prompt lookup")
            .expect("prompt exists");
        assert_eq!(prompt.status, status);
    }
}

#[test]
fn migration_015_preserves_rows_inserted_at_schema_14() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let connection = Connection::open(&path).expect("sqlite should open");
    // Replay every pre-015 migration so the prompts table matches the
    // shape callers wrote against before this batch landed.
    connection
        .execute_batch(include_str!("../migrations/001_init.sqlite.sql"))
        .expect("001 schema should apply");
    connection
        .execute_batch(include_str!(
            "../migrations/002_auth_failures_schema.sqlite.sql"
        ))
        .expect("002 schema should apply");
    connection
        .execute_batch(include_str!(
            "../migrations/003_agent_capabilities.sqlite.sql"
        ))
        .expect("003 schema should apply");
    connection
        .execute_batch(include_str!("../migrations/004_sessions.sqlite.sql"))
        .expect("004 schema should apply");
    connection
        .execute_batch(include_str!("../migrations/005_commands_schema.sqlite.sql"))
        .expect("005 schema should apply");
    connection
        .execute_batch(include_str!("../migrations/006_permissions.sqlite.sql"))
        .expect("006 schema should apply");
    connection
        .execute_batch(include_str!("../migrations/007_events_source.sqlite.sql"))
        .expect("007 schema should apply");
    connection
        .execute_batch(include_str!("../migrations/008_sink_outbox.sqlite.sql"))
        .expect("008 schema should apply");
    connection
        .execute_batch(include_str!(
            "../migrations/009_installer_runs_step.sqlite.sql"
        ))
        .expect("009 schema should apply");
    connection
        .execute_batch(include_str!(
            "../migrations/010_installer_runs_version.sqlite.sql"
        ))
        .expect("010 schema should apply");
    connection
        .execute_batch(include_str!(
            "../migrations/011_installer_runs_log_dir.sqlite.sql"
        ))
        .expect("011 schema should apply");
    connection
        .execute_batch(include_str!("../migrations/012_init_runs.sqlite.sql"))
        .expect("012 schema should apply");
    connection
        .execute_batch(include_str!(
            "../migrations/013_installer_runs_apply_run_id.sqlite.sql"
        ))
        .expect("013 schema should apply");
    connection
        .execute_batch(
            r#"
            CREATE TABLE schema_migrations (
                version INTEGER PRIMARY KEY,
                name TEXT NOT NULL,
                applied_at TEXT NOT NULL
            );
            INSERT INTO schema_migrations (version, name, applied_at) VALUES
                (1,  'init',                          '2026-05-13T00:00:00Z'),
                (2,  'auth_failures_schema',          '2026-05-13T00:00:00Z'),
                (3,  'agent_capabilities',            '2026-05-13T00:00:00Z'),
                (4,  'sessions',                      '2026-05-13T00:00:00Z'),
                (5,  'commands_schema',               '2026-05-13T00:00:00Z'),
                (6,  'permissions',                   '2026-05-13T00:00:00Z'),
                (7,  'events_source',                 '2026-05-13T00:00:00Z'),
                (8,  'sink_outbox',                   '2026-05-13T00:00:00Z'),
                (9,  'installer_runs_step',           '2026-05-13T00:00:00Z'),
                (10, 'installer_runs_version',        '2026-05-13T00:00:00Z'),
                (11, 'installer_runs_log_dir',        '2026-05-13T00:00:00Z'),
                (12, 'init_runs',                     '2026-05-13T00:00:00Z'),
                (13, 'installer_runs_apply_run_id',   '2026-05-13T00:00:00Z');
            "#,
        )
        .expect("schema_migrations should seed");
    // Seed a session + two prompts using the pre-015 column set so the
    // rebuild path has actual data to copy across.
    connection
        .execute_batch(
            r#"
            INSERT INTO sessions (id, created_at, updated_at, status, agent_id, cwd, title, metadata_json)
            VALUES ('sess_legacy', '2026-05-13T00:00:00.000000000Z', '2026-05-13T00:00:00.000000000Z',
                    'active', 'fake', '/tmp/legacy', NULL, '{}');
            INSERT INTO prompts (id, session_id, created_at, updated_at, status, stop_reason, error_code, error_message, prompt_json)
            VALUES ('prm_legacy_done', 'sess_legacy', '2026-05-13T00:01:00.000000000Z', '2026-05-13T00:01:30.000000000Z',
                    'completed', 'end_turn', NULL, NULL, '[]');
            INSERT INTO prompts (id, session_id, created_at, updated_at, status, stop_reason, error_code, error_message, prompt_json)
            VALUES ('prm_legacy_err',  'sess_legacy', '2026-05-13T00:02:00.000000000Z', '2026-05-13T00:02:30.000000000Z',
                    'errored',  NULL, 'agent.protocol_error', 'boom', '[]');
            "#,
        )
        .expect("legacy prompts should seed");
    drop(connection);

    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration to latest should pass");
    assert_eq!(
        store.schema_version().expect("schema version should load"),
        22
    );
    let inspection = Connection::open(&path).expect("sqlite inspection should open");
    let columns = inspection
        .prepare("PRAGMA table_info(installer_runs)")
        .and_then(|mut statement| {
            let rows = statement.query_map([], |row| row.get::<_, String>(1))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })
        .expect("installer_runs columns should query");
    assert!(columns.iter().any(|name| name == "operation"));
    assert!(columns.iter().any(|name| name == "method"));

    let done = store
        .get_prompt("prm_legacy_done")
        .expect("legacy completed lookup")
        .expect("legacy completed exists");
    assert_eq!(done.status, "completed");
    assert_eq!(done.stop_reason.as_deref(), Some("end_turn"));
    assert!(done.failure_class.is_none());
    assert!(done.failure_detail_json.is_none());

    let err = store
        .get_prompt("prm_legacy_err")
        .expect("legacy errored lookup")
        .expect("legacy errored exists");
    assert_eq!(err.status, "errored");
    assert_eq!(err.error_code.as_deref(), Some("agent.protocol_error"));
    assert!(err.failure_class.is_none());
    assert!(err.failure_detail_json.is_none());
}

// CONSTANTS for the mark_stalled_prompts tests below.
const STALE_THRESHOLD_SECS: u64 = 60;
const STALE_REASON: &str = "test stall reason";

/// Helper: insert a session + one prompt, flip the prompt to running,
/// then overwrite its `updated_at` directly so the test controls the
/// "how old is this row" axis without sleeping for minutes.
fn seed_running_prompt_at(store: &StateStore, session_id: &str, prompt_id: &str, updated_at: &str) {
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

// === LogFilter::matches coverage ===

fn fake_event(kind: &str, level: &str, source: &str, payload_json: &str) -> Event {
    fake_event_at(kind, level, source, payload_json, "2026-05-25T12:00:00Z")
}

fn fake_event_at(
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

fn fake_session_event(
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

#[test]
fn log_filter_matches_level_kind_and_kind_prefix() {
    let event = fake_event("command.started", "info", "command", "{}");

    let level_match = EventFilter {
        level: Some("info"),
        ..EventFilter::default()
    };
    assert!(level_match.matches(&event));

    let level_miss = EventFilter {
        level: Some("error"),
        ..EventFilter::default()
    };
    assert!(!level_miss.matches(&event));

    let kind_exact = EventFilter {
        kind: Some("command.started"),
        ..EventFilter::default()
    };
    assert!(kind_exact.matches(&event));

    let kind_prefix = EventFilter {
        kind_prefix: Some("command."),
        ..EventFilter::default()
    };
    assert!(kind_prefix.matches(&event));

    let kind_prefix_miss = EventFilter {
        kind_prefix: Some("permission."),
        ..EventFilter::default()
    };
    assert!(!kind_prefix_miss.matches(&event));
}

#[test]
fn log_filter_matches_source_filter() {
    let event = fake_event("acp.session_update", "info", "acp", "{}");

    let source_hit = EventFilter {
        source: Some("acp"),
        ..EventFilter::default()
    };
    assert!(source_hit.matches(&event));

    let source_miss = EventFilter {
        source: Some("command"),
        ..EventFilter::default()
    };
    assert!(!source_miss.matches(&event));
}

#[test]
fn log_filter_matches_session_id_via_column_with_payload_fallback() {
    // Modern path: typed `session_id` column populated by
    // `append_session_event_with_source`. Matcher must hit on the column
    // even when the payload is empty.
    let column_event =
        fake_session_event("acp.session_update", "info", "acp", "{}", Some("sess_abc"));
    let session_hit = EventFilter {
        session_id: Some("sess_abc"),
        ..EventFilter::default()
    };
    assert!(session_hit.matches(&column_event));

    let session_miss = EventFilter {
        session_id: Some("sess_other"),
        ..EventFilter::default()
    };
    assert!(!session_miss.matches(&column_event));

    // Legacy fallback: the column is None but the payload embeds session_id.
    // This keeps pre-Phase-5 events queryable while the SQL still requires the
    // column directly.
    let legacy_event = fake_event(
        "acp.session_update",
        "info",
        "acp",
        r#"{"session_id":"sess_legacy"}"#,
    );
    let legacy_filter = EventFilter {
        session_id: Some("sess_legacy"),
        ..EventFilter::default()
    };
    assert!(legacy_filter.matches(&legacy_event));
}

#[test]
fn log_filter_matches_since_and_until_bounds() {
    let event = fake_event_at("test.kind", "info", "system", "{}", "2026-05-25T12:00:00Z");

    let since_open = EventFilter {
        since: Some("2026-05-25T11:00:00Z"),
        ..EventFilter::default()
    };
    assert!(since_open.matches(&event));

    let since_after = EventFilter {
        since: Some("2026-05-25T13:00:00Z"),
        ..EventFilter::default()
    };
    assert!(!since_after.matches(&event));

    let until_open = EventFilter {
        until: Some("2026-05-25T13:00:00Z"),
        ..EventFilter::default()
    };
    assert!(until_open.matches(&event));

    // until is strict (exclusive); equal value drops the row.
    let until_equal = EventFilter {
        until: Some("2026-05-25T12:00:00Z"),
        ..EventFilter::default()
    };
    assert!(!until_equal.matches(&event));
}

#[test]
fn log_filter_matches_command_id_payload_field() {
    let event = fake_event(
        "command.exited",
        "info",
        "command",
        r#"{"command_id":"cmd_42"}"#,
    );

    let hit = EventFilter {
        command_id: Some("cmd_42"),
        ..EventFilter::default()
    };
    assert!(hit.matches(&event));

    let miss = EventFilter {
        command_id: Some("cmd_99"),
        ..EventFilter::default()
    };
    assert!(!miss.matches(&event));
}

#[test]
fn log_filter_matches_permission_id_with_legacy_id_fallback() {
    // Modern publisher path: `$.permission_id` populated.
    let modern = fake_event(
        "permission.created",
        "info",
        "permission",
        r#"{"permission_id":"perm_1"}"#,
    );
    let modern_filter = EventFilter {
        permission_id: Some("perm_1"),
        ..EventFilter::default()
    };
    assert!(modern_filter.matches(&modern));

    // Legacy / timeout path: only `$.id` is populated, on a permission-shaped
    // row (kind starts with `permission.`).
    let legacy = fake_event(
        "permission.timeout",
        "info",
        "permission",
        r#"{"id":"perm_2"}"#,
    );
    let legacy_filter = EventFilter {
        permission_id: Some("perm_2"),
        ..EventFilter::default()
    };
    assert!(legacy_filter.matches(&legacy));

    // Same `$.id` payload but on a non-permission-shaped row must not match.
    let unrelated = fake_event("command.exited", "info", "command", r#"{"id":"perm_2"}"#);
    assert!(!legacy_filter.matches(&unrelated));
}

#[test]
fn log_filter_matches_security_category() {
    let rate_limited = fake_event("security.rate_limited", "warn", "api", "{}");
    let cors_denied = fake_event("security.cors_origin_denied", "warn", "api", "{}");
    let unrelated = fake_event("command.exited", "info", "command", "{}");

    let rate_filter = EventFilter {
        security_category: Some(SecurityCategory::RateLimit),
        ..EventFilter::default()
    };
    assert!(rate_filter.matches(&rate_limited));
    assert!(!rate_filter.matches(&cors_denied));
    assert!(!rate_filter.matches(&unrelated));

    let cors_filter = EventFilter {
        security_category: Some(SecurityCategory::OriginCors),
        ..EventFilter::default()
    };
    assert!(cors_filter.matches(&cors_denied));
    assert!(!cors_filter.matches(&rate_limited));
}

#[test]
fn log_filter_security_category_query_returns_only_matching_kinds() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    for kind in [
        "security.rate_limited",
        "security.cors_origin_denied",
        "security.ws_origin_denied",
        "security.ip_block_active",
        "security.ip_block_applied",
        "security.request_oversized",
    ] {
        store
            .append_event("warn", kind, "", "{}")
            .expect("seed security event");
    }

    let cors_only = store
        .query_events(EventFilter {
            limit: 50,
            security_category: Some(SecurityCategory::OriginCors),
            ..EventFilter::default()
        })
        .expect("category-filtered query");

    let kinds: std::collections::BTreeSet<_> = cors_only.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(
        kinds,
        ["security.cors_origin_denied", "security.ws_origin_denied"]
            .into_iter()
            .collect()
    );
}

#[test]
fn log_filter_order_asc_returns_oldest_first_and_cursor_advances_forward() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    for index in 0..6 {
        store
            .append_event("info", "test.ordered", &format!("row-{index}"), "{}")
            .expect("seed");
    }

    let first_page = store
        .query_events(EventFilter {
            limit: 2,
            kind: Some("test.ordered"),
            order: LogOrder::Asc,
            ..EventFilter::default()
        })
        .expect("asc page");
    assert_eq!(first_page.len(), 2);
    assert_eq!(first_page[0].message, "row-0");
    assert_eq!(first_page[1].message, "row-1");

    let cursor = first_page.last().expect("cursor row").id.clone();
    let second_page = store
        .query_events(EventFilter {
            limit: 2,
            kind: Some("test.ordered"),
            after_id: Some(&cursor),
            order: LogOrder::Asc,
            ..EventFilter::default()
        })
        .expect("asc page 2");
    assert_eq!(second_page[0].message, "row-2");
    assert_eq!(second_page[1].message, "row-3");
}

// === Concurrent-write pagination stability ===

#[test]
fn cursor_pagination_stable_under_concurrent_writes() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");

    // `StateStore::open` enables WAL and a busy timeout on every connection, so
    // a background writer can append while the foreground reader paginates
    // without `SQLITE_BUSY`. This exercises that guarantee: two independent
    // StateStore handles (separate connections, no shared mutex) write and read
    // the same file concurrently. The tighter per-handle timeout below keeps
    // rare file-header contention well under the test harness's budget.
    let reader = StateStore::open(&path).expect("reader open");
    reader.migrate().expect("migration should pass");
    reader
        .set_busy_timeout_for_test(std::time::Duration::from_secs(2))
        .expect("reader busy timeout");

    for index in 0..200 {
        reader
            .append_event("info", "test.page", &format!("seed-{index}"), "{}")
            .expect("seed");
    }

    // Second, independent StateStore — its own rusqlite::Connection against
    // the same path. No shared Mutex; both handles commit independently and
    // SQLite serializes the writes at the file layer under WAL.
    let writer_store = StateStore::open(&path).expect("writer open");
    writer_store
        .set_busy_timeout_for_test(std::time::Duration::from_secs(2))
        .expect("writer busy timeout");

    let stop = Arc::new(AtomicBool::new(false));
    let writer_stop = Arc::clone(&stop);
    let writer = std::thread::spawn(move || {
        let mut counter: u64 = 0;
        while !writer_stop.load(Ordering::SeqCst) {
            writer_store
                .append_event("info", "test.background", &format!("bg-{counter}"), "{}")
                .expect("background append");
            counter = counter.wrapping_add(1);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    });

    // DESC walk: must collect all 200 seeded rows exactly once in strictly
    // monotone-decreasing id order, even while background writes commit.
    let mut collected_desc: Vec<String> = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let after = cursor.clone();
        let page = reader
            .query_events(EventFilter {
                limit: 20,
                kind: Some("test.page"),
                after_id: after.as_deref(),
                order: LogOrder::Desc,
                ..EventFilter::default()
            })
            .expect("desc page");
        if page.is_empty() {
            break;
        }
        // Interleave a tiny sleep between pages so the background writer
        // actually gets to commit between our reads. Without this, the reader
        // might race through all 200 rows before any concurrent writes land.
        std::thread::sleep(std::time::Duration::from_millis(2));
        for event in &page {
            collected_desc.push(event.id.clone());
        }
        cursor = page.last().map(|e| e.id.clone());
        if page.len() < 20 {
            break;
        }
    }

    assert_eq!(collected_desc.len(), 200, "all 200 seeded ids must appear");
    let unique: std::collections::BTreeSet<_> = collected_desc.iter().collect();
    assert_eq!(
        unique.len(),
        200,
        "ids must be unique under concurrent writes"
    );
    for pair in collected_desc.windows(2) {
        assert!(
            pair[0] > pair[1],
            "DESC walk must produce strictly decreasing ids: {} !> {}",
            pair[0],
            pair[1]
        );
    }

    // ASC walk: the 200 pre-existing ids must all appear in strictly
    // increasing order. Newer rows appended mid-walk may also land in the
    // page; we accept that and just check the seeded subset.
    let seeded_subset: std::collections::BTreeSet<_> = collected_desc.iter().cloned().collect();
    let mut collected_asc: Vec<String> = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let after = cursor.clone();
        let page = reader
            .query_events(EventFilter {
                limit: 20,
                kind: Some("test.page"),
                after_id: after.as_deref(),
                order: LogOrder::Asc,
                ..EventFilter::default()
            })
            .expect("asc page");
        if page.is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
        for event in &page {
            collected_asc.push(event.id.clone());
        }
        cursor = page.last().map(|e| e.id.clone());
        if page.len() < 20 {
            break;
        }
    }

    let asc_seeded: Vec<&String> = collected_asc
        .iter()
        .filter(|id| seeded_subset.contains(*id))
        .collect();
    assert_eq!(
        asc_seeded.len(),
        200,
        "ASC walk must surface every seeded id"
    );
    let asc_unique: std::collections::BTreeSet<_> = collected_asc.iter().collect();
    assert_eq!(
        asc_unique.len(),
        collected_asc.len(),
        "ASC walk must not duplicate any id"
    );
    for pair in asc_seeded.windows(2) {
        assert!(
            pair[0] < pair[1],
            "ASC walk must produce strictly increasing ids: {} !< {}",
            pair[0],
            pair[1]
        );
    }

    stop.store(true, Ordering::SeqCst);
    writer.join().expect("writer thread join");
}
