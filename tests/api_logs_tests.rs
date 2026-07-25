use reqwest::StatusCode;
use serde_json::Value;

mod common;
use common::api::{SESSION_KEY, ServerHarness, seed_command, seed_session};

#[tokio::test]
async fn logs_events_returns_array_envelope() {
    let harness = ServerHarness::spawn().await;
    // Seed an event so the array is non-empty.
    {
        let guard = harness.state.lock().await;
        guard
            .append_event("info", "test.kind", "hello", "{}")
            .expect("append event");
    }
    let response = reqwest::Client::new()
        .get(format!("{}/v1/logs/events?limit=10", harness.base_url))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    let events = body["data"]["events"].as_array().expect("events array");
    assert!(!events.is_empty());
    // `kind` matches the seeded value; the source column round-trips into the
    // response envelope; the next_cursor key is always present.
    assert!(events.iter().any(|e| e["kind"] == "test.kind"));
    assert!(events.iter().any(|e| e["source"].is_string()));
    assert!(body["data"].get("next_cursor").is_some());
}

#[tokio::test]
async fn logs_events_supports_kind_filter() {
    let harness = ServerHarness::spawn().await;
    {
        let guard = harness.state.lock().await;
        guard
            .append_event_with_source("info", "command.started", "command", "", "{}")
            .expect("append");
        guard
            .append_event_with_source("info", "command.exited", "command", "", "{}")
            .expect("append");
        guard
            .append_event("info", "session.update", "", "{}")
            .expect("append");
    }
    let response = reqwest::Client::new()
        .get(format!(
            "{}/v1/logs/events?kind=command.&limit=10",
            harness.base_url
        ))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    let events = body["data"]["events"].as_array().expect("events array");
    assert_eq!(events.len(), 2);
    for event in events {
        assert!(event["kind"].as_str().unwrap().starts_with("command."));
    }
}

#[tokio::test]
async fn logs_events_supports_source_filter() {
    let harness = ServerHarness::spawn().await;
    {
        let guard = harness.state.lock().await;
        guard
            .append_event_with_source("info", "command.exited", "command", "", "{}")
            .expect("append");
        guard
            .append_event_with_source("info", "permission.created", "permission", "", "{}")
            .expect("append");
    }
    let response = reqwest::Client::new()
        .get(format!(
            "{}/v1/logs/events?source=command&limit=10",
            harness.base_url
        ))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    let events = body["data"]["events"].as_array().expect("events array");
    assert!(events.iter().all(|e| e["source"] == "command"));
}

#[tokio::test]
async fn logs_events_pagination_cursor_advances_page() {
    let harness = ServerHarness::spawn().await;
    {
        let guard = harness.state.lock().await;
        for i in 0..5 {
            guard
                .append_event("info", "test.page", &format!("row-{i}"), "{}")
                .expect("append");
        }
    }
    let first = reqwest::Client::new()
        .get(format!(
            "{}/v1/logs/events?kind=test.page&limit=2",
            harness.base_url
        ))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("send")
        .json::<Value>()
        .await
        .expect("json");
    let next_cursor = first["data"]["next_cursor"]
        .as_str()
        .expect("next_cursor present when page saturates limit")
        .to_owned();
    let second = reqwest::Client::new()
        .get(format!(
            "{}/v1/logs/events?kind=test.page&limit=2&after={next_cursor}",
            harness.base_url
        ))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("send")
        .json::<Value>()
        .await
        .expect("json");
    let second_events = second["data"]["events"].as_array().expect("events array");
    assert_eq!(second_events.len(), 2);
    // The cursor must not echo back in the next page.
    assert!(
        second_events
            .iter()
            .all(|e| e["id"].as_str().unwrap() != next_cursor)
    );
}

#[tokio::test]
async fn logs_events_category_filters_security_kinds_via_route() {
    let harness = ServerHarness::spawn().await;
    {
        let guard = harness.state.lock().await;
        guard
            .append_event("warn", "security.cors_origin_denied", "denied", "{}")
            .expect("seed cors");
        guard
            .append_event("warn", "security.ws_origin_denied", "denied", "{}")
            .expect("seed ws cors");
        guard
            .append_event("warn", "security.rate_limited", "rate", "{}")
            .expect("seed rate");
    }
    let response = reqwest::Client::new()
        .get(format!(
            "{}/v1/logs/events?category=origin_cors&limit=10",
            harness.base_url
        ))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    let events = body["data"]["events"].as_array().expect("events array");
    assert_eq!(events.len(), 2, "only origin_cors kinds must match");
    for event in events {
        let kind = event["kind"].as_str().expect("kind");
        assert!(
            kind == "security.cors_origin_denied" || kind == "security.ws_origin_denied",
            "unexpected kind: {kind}"
        );
    }
}

#[tokio::test]
async fn logs_events_order_asc_returns_oldest_first_via_route() {
    let harness = ServerHarness::spawn().await;
    {
        let guard = harness.state.lock().await;
        for index in 0..3 {
            guard
                .append_event("info", "test.ordered", &format!("row-{index}"), "{}")
                .expect("seed");
        }
    }
    let first = reqwest::Client::new()
        .get(format!(
            "{}/v1/logs/events?kind=test.ordered&order=asc&limit=2",
            harness.base_url
        ))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("send")
        .json::<Value>()
        .await
        .expect("json");
    let first_events = first["data"]["events"].as_array().expect("events");
    assert_eq!(first_events.len(), 2);
    assert_eq!(first_events[0]["message"], "row-0");
    assert_eq!(first_events[1]["message"], "row-1");
    let next_cursor = first["data"]["next_cursor"]
        .as_str()
        .expect("next_cursor")
        .to_owned();

    let second = reqwest::Client::new()
        .get(format!(
            "{}/v1/logs/events?kind=test.ordered&order=asc&limit=2&after={next_cursor}",
            harness.base_url
        ))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("send")
        .json::<Value>()
        .await
        .expect("json");
    let second_events = second["data"]["events"].as_array().expect("events");
    assert_eq!(second_events.len(), 1);
    assert_eq!(second_events[0]["message"], "row-2");
}

#[tokio::test]
async fn logs_events_invalid_category_returns_400() {
    let harness = ServerHarness::spawn().await;
    let response = reqwest::Client::new()
        .get(format!(
            "{}/v1/logs/events?category=nonsense",
            harness.base_url
        ))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("json");
    let message = body["error"]["message"].as_str().unwrap_or("");
    assert!(
        message.contains("category"),
        "error message should mention `category`: {message}"
    );
}

#[tokio::test]
async fn api_request_middleware_records_event_with_status_and_duration() {
    let harness = ServerHarness::spawn().await;
    let response = reqwest::Client::new()
        .get(format!("{}/v1/logs/events?limit=1", harness.base_url))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK);

    // Inspect SQLite directly — the writer runs inside the response future.
    let guard = harness.state.lock().await;
    let rows = guard
        .query_events(acp_stack::state::LogFilter {
            limit: 50,
            kind: Some("api.request"),
            ..acp_stack::state::LogFilter::default()
        })
        .expect("query");
    assert!(
        rows.iter()
            .any(|r| r.payload_json.contains("\"status\":200")),
        "expected an api.request row with status=200"
    );
    let recorded = rows
        .iter()
        .find(|r| r.payload_json.contains("\"status\":200"))
        .expect("matching row");
    assert_eq!(recorded.source, "api");
    let payload: Value = serde_json::from_str(&recorded.payload_json).expect("payload json");
    assert_eq!(payload["method"].as_str(), Some("GET"));
    assert!(payload["duration_ms"].is_number());
}

#[tokio::test]
async fn api_request_middleware_skips_status_routes() {
    let harness = ServerHarness::spawn().await;
    // Hit /v1/status repeatedly; the skip list must keep `api.request` rows
    // out of SQLite for this path so polling clients don't bloat the table.
    for _ in 0..3 {
        let _ = reqwest::Client::new()
            .get(format!("{}/v1/status", harness.base_url))
            .header("Authorization", format!("Bearer {SESSION_KEY}"))
            .send()
            .await
            .expect("send");
    }
    let guard = harness.state.lock().await;
    let rows = guard
        .query_events(acp_stack::state::LogFilter {
            limit: 100,
            kind: Some("api.request"),
            ..acp_stack::state::LogFilter::default()
        })
        .expect("query");
    assert!(
        rows.iter()
            .all(|r| !r.payload_json.contains("\"/v1/status\"")
                && !r.payload_json.contains("\\\"/v1/status\\\"")),
        "no api.request rows should be recorded for /v1/status",
    );
}

#[tokio::test]
async fn log_query_routes_return_seeded_records_newest_first() {
    let harness = ServerHarness::spawn().await;
    seed_session(
        &harness.state_path,
        "sess_old",
        "closed",
        "2026-05-14T00:00:00.000000000Z",
        "2026-05-14T00:00:01.000000000Z",
    );
    seed_session(
        &harness.state_path,
        "sess_new",
        "open",
        "2026-05-14T00:00:02.000000000Z",
        "2026-05-14T00:00:03.000000000Z",
    );
    seed_command(
        &harness.state_path,
        "cmd_old",
        "failed",
        "false",
        Some(1),
        "2026-05-14T00:00:04.000000000Z",
        "2026-05-14T00:00:05.000000000Z",
    );
    seed_command(
        &harness.state_path,
        "cmd_new",
        "succeeded",
        "true",
        Some(0),
        "2026-05-14T00:00:06.000000000Z",
        "2026-05-14T00:00:07.000000000Z",
    );
    {
        let guard = harness.state.lock().await;
        guard
            .append_event("info", "permission.requested", "old permission", "{}")
            .expect("append permission event");
        guard
            .append_event("info", "permissions.decided", "new permission", "{}")
            .expect("append permission event");
        guard
            .append_auth_failure("unknown", "missing", None, Some("/v1/status"), "{}")
            .expect("append auth failure");
    }

    let client = reqwest::Client::new();
    let commands: Value = client
        .get(format!("{}/v1/logs/commands?limit=1", harness.base_url))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("send")
        .json()
        .await
        .expect("json");
    assert_eq!(commands["data"]["commands"][0]["id"], "cmd_new");
    assert_eq!(commands["data"]["commands"].as_array().unwrap().len(), 1);

    let sessions: Value = client
        .get(format!("{}/v1/logs/sessions?limit=1", harness.base_url))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("send")
        .json()
        .await
        .expect("json");
    assert_eq!(sessions["data"]["sessions"][0]["id"], "sess_new");
    assert_eq!(sessions["data"]["sessions"].as_array().unwrap().len(), 1);

    let permissions: Value = client
        .get(format!("{}/v1/logs/permissions?limit=10", harness.base_url))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("send")
        .json()
        .await
        .expect("json");
    assert_eq!(
        permissions["data"]["events"][0]["kind"],
        "permissions.decided"
    );
    assert_eq!(permissions["data"]["events"].as_array().unwrap().len(), 2);

    let security: Value = client
        .get(format!("{}/v1/logs/security?limit=10", harness.base_url))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("send")
        .json()
        .await
        .expect("json");
    assert_eq!(security["data"]["auth_failures"][0]["reason"], "missing");
}

#[tokio::test]
async fn logs_security_pages_auth_failures_and_events_independently() {
    let harness = ServerHarness::spawn().await;
    {
        let guard = harness.state.lock().await;
        guard
            .append_auth_failure("unknown", "missing", None, Some("/v1/a"), "{}")
            .expect("append auth failure");
        guard
            .append_auth_failure("unknown", "invalid", None, Some("/v1/b"), "{}")
            .expect("append auth failure");
        guard
            .append_event_with_source("warn", "security.first", "api", "", "{}")
            .expect("append security event");
        guard
            .append_event_with_source("warn", "security.second", "api", "", "{}")
            .expect("append security event");
    }

    let client = reqwest::Client::new();
    let first: Value = client
        .get(format!("{}/v1/logs/security?limit=1", harness.base_url))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("send")
        .json()
        .await
        .expect("json");
    let auth_cursor = first["data"]["auth_failures_next_cursor"]
        .as_str()
        .expect("auth cursor")
        .to_owned();
    let event_cursor = first["data"]["events_next_cursor"]
        .as_str()
        .expect("event cursor")
        .to_owned();
    let first_auth_id = first["data"]["auth_failures"][0]["id"]
        .as_str()
        .expect("auth id")
        .to_owned();
    let first_event_id = first["data"]["events"][0]["id"]
        .as_str()
        .expect("event id")
        .to_owned();

    let auth_paged: Value = client
        .get(format!(
            "{}/v1/logs/security?limit=1&auth_failures_after={auth_cursor}",
            harness.base_url
        ))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("send")
        .json()
        .await
        .expect("json");
    assert_ne!(
        auth_paged["data"]["auth_failures"][0]["id"]
            .as_str()
            .expect("auth id"),
        first_auth_id,
        "auth cursor should advance auth_failures"
    );
    assert_eq!(
        auth_paged["data"]["events"][0]["id"]
            .as_str()
            .expect("event id"),
        first_event_id,
        "auth cursor must not advance security events"
    );

    let events_paged: Value = client
        .get(format!(
            "{}/v1/logs/security?limit=1&events_after={event_cursor}",
            harness.base_url
        ))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("send")
        .json()
        .await
        .expect("json");
    assert_eq!(
        events_paged["data"]["auth_failures"][0]["id"]
            .as_str()
            .expect("auth id"),
        first_auth_id,
        "event cursor must not advance auth_failures"
    );
    assert_ne!(
        events_paged["data"]["events"][0]["id"]
            .as_str()
            .expect("event id"),
        first_event_id,
        "event cursor should advance security events"
    );
}

#[tokio::test]
async fn logs_security_order_asc_applies_to_auth_failures_and_events() {
    let harness = ServerHarness::spawn().await;
    {
        let guard = harness.state.lock().await;
        guard
            .append_auth_failure("unknown", "missing", None, Some("/v1/a"), "{}")
            .expect("append auth failure");
        guard
            .append_auth_failure("unknown", "invalid", None, Some("/v1/b"), "{}")
            .expect("append auth failure");
        guard
            .append_event_with_source("warn", "security.first", "api", "", "{}")
            .expect("append security event");
        guard
            .append_event_with_source("warn", "security.second", "api", "", "{}")
            .expect("append security event");
    }

    let body: Value = reqwest::Client::new()
        .get(format!(
            "{}/v1/logs/security?limit=10&order=asc",
            harness.base_url
        ))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("send")
        .json()
        .await
        .expect("json");
    let auth_reasons = body["data"]["auth_failures"]
        .as_array()
        .expect("auth failures")
        .iter()
        .map(|row| row["reason"].as_str().expect("reason"))
        .collect::<Vec<_>>();
    assert_eq!(auth_reasons, ["missing", "invalid"]);

    let event_kinds = body["data"]["events"]
        .as_array()
        .expect("events")
        .iter()
        .map(|row| row["kind"].as_str().expect("kind"))
        .collect::<Vec<_>>();
    assert_eq!(event_kinds, ["security.first", "security.second"]);
}

#[tokio::test]
async fn logs_security_legacy_after_still_pages_when_specific_cursor_absent() {
    let harness = ServerHarness::spawn().await;
    {
        let guard = harness.state.lock().await;
        guard
            .append_auth_failure("unknown", "missing", None, Some("/v1/a"), "{}")
            .expect("append auth failure");
        guard
            .append_auth_failure("unknown", "invalid", None, Some("/v1/b"), "{}")
            .expect("append auth failure");
    }

    let client = reqwest::Client::new();
    let first: Value = client
        .get(format!("{}/v1/logs/security?limit=1", harness.base_url))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("send")
        .json()
        .await
        .expect("json");
    let cursor = first["data"]["auth_failures_next_cursor"]
        .as_str()
        .expect("cursor")
        .to_owned();
    let first_id = first["data"]["auth_failures"][0]["id"]
        .as_str()
        .expect("auth id")
        .to_owned();

    let second: Value = client
        .get(format!(
            "{}/v1/logs/security?limit=1&after={cursor}",
            harness.base_url
        ))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("send")
        .json()
        .await
        .expect("json");
    assert_ne!(
        second["data"]["auth_failures"][0]["id"]
            .as_str()
            .expect("auth id"),
        first_id
    );
}

#[tokio::test]
async fn logs_events_limit_is_capped() {
    // Seed 1500 events; even with `limit=10000`, the handler must cap rows
    // at MAX_LOGS_LIMIT (1000) so an authenticated session cannot ask sqlite
    // for billions of rows.
    let harness = ServerHarness::spawn().await;
    {
        let guard = harness.state.lock().await;
        for i in 0..1500 {
            guard
                .append_event("info", "burst", &format!("e{i}"), "{}")
                .expect("append");
        }
    }
    let response = reqwest::Client::new()
        .get(format!("{}/v1/logs/events?limit=10000", harness.base_url))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    let events = body["data"]["events"].as_array().expect("events array");
    assert_eq!(events.len(), 1000);
}
