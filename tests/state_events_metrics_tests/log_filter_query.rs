use acp_stack::state::{EventFilter, SecurityCategory, StateStore};

use crate::common::state::fresh_state;

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
