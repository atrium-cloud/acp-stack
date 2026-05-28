use acp_stack::state::{
    AuthFailureFilter, EVENT_KIND_PROMPT_INFERENCE_FAILED, EVENT_SOURCE_ACP, EVENT_SOURCE_SYSTEM,
    EventFilter, FailureClass, INIT_RUN_FAILED, INIT_RUN_SUCCEEDED, INIT_STEP_FAILED,
    INIT_STEP_PENDING, INIT_STEP_RUNNING, INIT_STEP_SKIPPED, INIT_STEP_SUCCEEDED,
    InstallerRunInput, ListedSessionRecord, NewInitRun, NewInitStep, NewPermissionRequest,
    NewPromptRecord, NewSessionRecord, PermissionStatus, PromptStatus,
    SESSION_ACTIVITY_ACTOR_AGENT, SESSION_ACTIVITY_ACTOR_USER, SESSION_STATUS_ACTIVE,
    SESSION_STATUS_AVAILABLE, SESSION_STATUS_CLOSED, StateStore, default_state_path,
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
        15
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
                agent_id: "fake".to_owned(),
                cwd: "/tmp/active-listed".to_owned(),
                title: Some("active listed".to_owned()),
                updated_at: Some("2026-05-25T00:00:00Z".to_owned()),
                metadata_json: r#"{"source":"agent_list"}"#.to_owned(),
            },
            ListedSessionRecord {
                id: "sess_closed".to_owned(),
                agent_id: "fake".to_owned(),
                cwd: "/tmp/closed-listed".to_owned(),
                title: Some("closed listed".to_owned()),
                updated_at: Some("2026-05-25T00:00:02Z".to_owned()),
                metadata_json: r#"{"source":"agent_list"}"#.to_owned(),
            },
            ListedSessionRecord {
                id: "sess_available".to_owned(),
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
                agent_id: "fake".to_owned(),
                cwd: "/tmp/offset".to_owned(),
                title: None,
                updated_at: Some("2026-02-01T08:00:00+08:00".to_owned()),
                metadata_json: "{}".to_owned(),
            },
            ListedSessionRecord {
                id: "sess_fraction".to_owned(),
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
    assert_eq!(history[1].step, "harness");
    assert_eq!(history[1].agent_id.as_deref(), Some("test-agent"));
    assert_eq!(history[1].version.as_deref(), Some("v1.2.3"));

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
            .contains("state schema version 99 is newer than supported version 15")
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
    // Seed one event, one command, one auth_failure inside the window.
    store
        .append_event_with_source(
            "info",
            "api.request",
            "api",
            "",
            r#"{"status":200,"duration_ms":42}"#,
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
    assert_eq!(summary.api_connections.request_count, Some(1));
    assert_eq!(
        summary
            .api_connections
            .by_status
            .get("2xx")
            .copied()
            .unwrap_or(0),
        1
    );
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
    // Optional metric instruments stay None when no inputs landed in the
    // window — distinguishes "instrument absent" from "instrument has 0 hits"
    // semantically, even when the column counts to 0.
    assert!(summary.usage.tokens_input.is_none());
    assert!(summary.api_connections.request_count.is_none());
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
    store.migrate().expect("migration to 15 should pass");
    assert_eq!(
        store.schema_version().expect("schema version should load"),
        15
    );

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
