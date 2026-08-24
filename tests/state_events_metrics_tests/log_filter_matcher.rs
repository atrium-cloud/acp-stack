use acp_stack::state::{EventFilter, LogOrder, SecurityCategory, StateStore};

use crate::common::state::{fake_event, fake_event_at, fake_session_event};

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

    // Legacy / timeout path: only `$.id` is populated, on a permission-shaped row.
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

#[test]
fn cursor_pagination_stable_under_concurrent_writes() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");

    // Exercises the WAL + busy-timeout guarantee: two independent StateStore handles write and read
    // the same file concurrently without `SQLITE_BUSY`.
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
        // Sleep between pages so the background writer actually commits between reads.
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

    // Rows appended mid-walk may also land in a page, so only the seeded subset is checked.
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
