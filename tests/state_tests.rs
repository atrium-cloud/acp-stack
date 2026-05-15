use acp_stack::state::{
    AuthFailureFilter, EventFilter, NewPermissionRequest, PermissionStatus, StateStore,
    default_state_path,
};
use rusqlite::Connection;
use rusqlite::params;

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
        7
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
            .contains("state schema version 99 is newer than supported version 7")
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
}
