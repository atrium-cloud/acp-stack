use acp_stack::state::{
    EVENT_KIND_PROMPT_INFERENCE_FAILED, EVENT_SOURCE_SYSTEM, EventFilter, FailureClass, LogOrder,
    NewPromptRecord, NewSessionRecord, PromptStatus, SecurityCategory, StateStore,
};

mod common;
use common::state::{fake_event, fake_event_at, fake_session_event, fresh_state};

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
            origin: acp_stack::state::CommandOrigin::Operator,
            session_id: None,
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
        .append_event_with_source(
            "info",
            "usage.reported",
            "acp",
            "",
            r#"{"context_window_used":4096,"context_window_max":16384,"cost_amount":1.25,"cost_currency":"USD"}"#,
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
    assert_eq!(summary.usage.context_window_used_max, Some(4096));
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
            origin: acp_stack::state::CommandOrigin::Operator,
            session_id: None,
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

// === LogFilter::matches coverage ===

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
