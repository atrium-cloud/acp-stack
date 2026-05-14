use acp_stack::state::{EventFilter, StateStore, default_state_path};
use rusqlite::Connection;

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
        1
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
            level: None,
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
            VALUES (2, '002_future', '2026-05-13T00:00:00Z');
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
            .contains("state schema version 2 is newer than supported version 1")
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
            level: None,
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
