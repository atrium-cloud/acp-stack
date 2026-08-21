use acp_stack::state::{
    EVENT_KIND_PROMPT_INFERENCE_FAILED, EVENT_SOURCE_SYSTEM, FailureClass, NewPromptRecord,
    NewSessionRecord, PromptStatus,
};

use crate::common::state::fresh_state;

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
    assert_eq!(summary.api_connections.request_count, 2);
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
    assert_eq!(summary.api_connections.request_count, 0);
    assert_eq!(summary.prompt_failures.total, 0);
}
