use acp_stack::state::{EventFilter, LogOrder, StateStore};

use crate::common::state::fresh_state;

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
            origin: acp_stack::state::CommandOrigin::Operator,
            session_id: None,
        })
        .expect("first command");
    let second = store
        .append_command(NewCommandRecord {
            command: "printf second",
            cwd: None,
            env_json: None,
            origin: acp_stack::state::CommandOrigin::Operator,
            session_id: None,
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
            origin: acp_stack::state::CommandOrigin::Operator,
            session_id: None,
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
