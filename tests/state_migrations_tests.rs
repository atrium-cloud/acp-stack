use acp_stack::state::{
    AuthFailureFilter, LogOrder, NewPromptRecord, NewSessionRecord, PromptStatus, StateStore,
    default_state_path,
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
        25
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
            .contains("state schema version 99 is newer than supported version 25")
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
fn migration_024_rewrites_legacy_single_l_cancelled_spelling() {
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
        include_str!("../migrations/022_array_sessions.sqlite.sql"),
        include_str!("../migrations/023_commands_origin.sqlite.sql"),
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
                (21, 'm21', '2026-05-13T00:00:00Z'),
                (22, 'm22', '2026-05-13T00:00:00Z'),
                (23, 'm23', '2026-05-13T00:00:00Z');

            INSERT INTO commands (id, created_at, updated_at, status, command)
            VALUES
                ('cmd_legacy', '2026-05-13T00:00:00Z', '2026-05-13T00:01:00Z', 'canceled', 'sleep 30'),
                ('cmd_failed', '2026-05-13T00:00:00Z', '2026-05-13T00:01:00Z', 'failed', 'false');

            INSERT INTO permission_requests
                (id, created_at, updated_at, status, source, requester, subject_id, detail_json, expires_at)
            VALUES
                ('prm_legacy', '2026-05-13T00:00:00Z', '2026-05-13T00:01:00Z', 'canceled', 'command',
                 'command:cmd_legacy', 'cmd_legacy', '{}', NULL);

            INSERT INTO permission_decisions
                (id, request_id, created_at, decision, deciding_principal, reason)
            VALUES
                ('dec_legacy', 'prm_legacy', '2026-05-13T00:01:00Z', 'canceled', 'system', 'command-canceled');

            INSERT INTO events (id, created_at, level, kind, message, payload_json)
            VALUES
                ('evt_cmd', '2026-05-13T00:01:00Z', 'info', 'command.canceled', 'command canceled',
                 '{"command_id":"cmd_legacy"}'),
                ('evt_prm', '2026-05-13T00:01:00Z', 'info', 'permission.canceled', 'permission canceled',
                 '{"permission_id":"prm_legacy"}'),
                ('evt_other', '2026-05-13T00:01:00Z', 'info', 'command.failed', 'command failed', '{}');
            "#,
        )
        .expect("legacy rows with the old spelling should seed");
    drop(connection);

    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");
    drop(store);

    let inspection = Connection::open(&path).expect("sqlite inspection should open");
    let scalar = |sql: &str| -> String {
        inspection
            .query_row(sql, [], |row| row.get::<_, String>(0))
            .expect("legacy row should still be readable")
    };

    assert_eq!(
        scalar("SELECT status FROM commands WHERE id = 'cmd_legacy'"),
        "cancelled"
    );
    assert_eq!(
        scalar("SELECT status FROM permission_requests WHERE id = 'prm_legacy'"),
        "cancelled"
    );
    assert_eq!(
        scalar("SELECT decision FROM permission_decisions WHERE id = 'dec_legacy'"),
        "cancelled"
    );
    assert_eq!(
        scalar("SELECT kind FROM events WHERE id = 'evt_cmd'"),
        "command.cancelled"
    );
    assert_eq!(
        scalar("SELECT kind FROM events WHERE id = 'evt_prm'"),
        "permission.cancelled"
    );

    // Rows that never carried the old spelling must be left exactly as they were.
    assert_eq!(
        scalar("SELECT status FROM commands WHERE id = 'cmd_failed'"),
        "failed"
    );
    assert_eq!(
        scalar("SELECT kind FROM events WHERE id = 'evt_other'"),
        "command.failed"
    );

    // A spelling rename is not a state transition: `updated_at` must not move,
    // and the append-only payload/message columns are deliberately untouched.
    assert_eq!(
        scalar("SELECT updated_at FROM commands WHERE id = 'cmd_legacy'"),
        "2026-05-13T00:01:00Z"
    );
    assert_eq!(
        scalar("SELECT updated_at FROM permission_requests WHERE id = 'prm_legacy'"),
        "2026-05-13T00:01:00Z"
    );
    assert_eq!(
        scalar("SELECT reason FROM permission_decisions WHERE id = 'dec_legacy'"),
        "command-canceled"
    );
    assert_eq!(
        scalar("SELECT message FROM events WHERE id = 'evt_prm'"),
        "permission canceled"
    );
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
        25
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
