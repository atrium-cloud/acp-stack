//! API tests for the status, health, and metrics surfaces.

use acp_stack::config::{HttpHeaderRef, McpConfig, McpHttpServer, McpServerConfig, McpStdioServer};
use acp_stack::state::EventFilter;
use reqwest::StatusCode;
use rusqlite::Connection;
use serde_json::Value;

mod common;
use common::api::{
    ADMIN_KEY, SESSION_KEY, ServerHarness, codex_adapter, seed_command, seed_session, test_config,
};

#[tokio::test]
async fn status_returns_200_with_session_key() {
    let harness = ServerHarness::spawn().await;
    let client = reqwest::Client::new();
    let response = client
        .get(format!("{}/v1/status", harness.base_url))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["ok"], Value::Bool(true));
    assert!(body["data"]["schema_version"].is_number());
    assert!(body["data"]["server"]["version"].is_string());
}

#[tokio::test]
async fn status_rejects_missing_authorization() {
    let harness = ServerHarness::spawn().await;
    let before = harness.auth_failure_count().await;
    let response = reqwest::Client::new()
        .get(format!("{}/v1/status", harness.base_url))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["ok"], Value::Bool(false));
    assert_eq!(body["error"]["code"], "auth.missing");
    assert_eq!(harness.auth_failure_count().await, before + 1);
    let (kind, reason) = harness.latest_auth_failure().await;
    assert_eq!(kind, "unknown");
    assert_eq!(reason, "missing");
}

#[tokio::test]
async fn status_rejects_invalid_bearer_token() {
    let harness = ServerHarness::spawn().await;
    let before = harness.auth_failure_count().await;
    let response = reqwest::Client::new()
        .get(format!("{}/v1/status", harness.base_url))
        .header("Authorization", "Bearer not_a_real_key")
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "auth.invalid");
    assert_eq!(harness.auth_failure_count().await, before + 1);
    let (kind, reason) = harness.latest_auth_failure().await;
    assert_eq!(kind, "unknown");
    assert_eq!(reason, "invalid");
}

#[tokio::test]
async fn status_rejects_admin_key_under_strict_tiering() {
    let harness = ServerHarness::spawn().await;
    let before = harness.auth_failure_count().await;
    let response = reqwest::Client::new()
        .get(format!("{}/v1/status", harness.base_url))
        .header("Authorization", format!("Bearer {ADMIN_KEY}"))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "auth.wrong_kind");
    assert_eq!(harness.auth_failure_count().await, before + 1);
    let (kind, reason) = harness.latest_auth_failure().await;
    assert_eq!(kind, "admin");
    assert_eq!(reason, "wrong_kind");
}

#[tokio::test]
async fn status_rejects_malformed_authorization_header() {
    let harness = ServerHarness::spawn().await;
    let before = harness.auth_failure_count().await;
    let response = reqwest::Client::new()
        .get(format!("{}/v1/status", harness.base_url))
        .header("Authorization", "NotBearer xyz")
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "auth.malformed_header");
    assert_eq!(harness.auth_failure_count().await, before + 1);
    let (_kind, reason) = harness.latest_auth_failure().await;
    assert_eq!(reason, "malformed_header");
}

#[tokio::test]
async fn status_agent_returns_configured_agent_snapshot() {
    let harness = ServerHarness::spawn().await;
    let response = reqwest::Client::new()
        .get(format!("{}/v1/status/agent", harness.base_url))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["ok"], Value::Bool(true));
    assert_eq!(body["data"]["configured"], Value::Bool(true));
    assert_eq!(body["data"]["agent"]["id"], "placebo");
    assert_eq!(body["data"]["agent"]["adapter"], Value::Null);
    assert!(body["data"]["lifecycle_events"].as_array().is_some());
}

#[tokio::test]
async fn status_agent_returns_adapter_metadata_when_configured() {
    let mut config = test_config();
    config.agent.adapter = Some(codex_adapter());
    let harness = ServerHarness::spawn_with_config(config).await;
    let response = reqwest::Client::new()
        .get(format!("{}/v1/status/agent", harness.base_url))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["data"]["agent"]["adapter"]["id"], "codex-acp");
    assert_eq!(
        body["data"]["agent"]["adapter"]["source_url"],
        "https://github.com/agentclientprotocol/codex-acp"
    );
}

#[tokio::test]
async fn status_connections_reports_active_requests() {
    let harness = ServerHarness::spawn().await;
    let response = reqwest::Client::new()
        .get(format!("{}/v1/status/connections", harness.base_url))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["ok"], Value::Bool(true));
    assert!(
        body["data"]["active_requests"].as_u64().unwrap() >= 1,
        "status request itself should be counted as active"
    );
}

#[tokio::test]
async fn health_live_returns_200_with_server_version() {
    let harness = ServerHarness::spawn().await;
    let response = reqwest::Client::new()
        .get(format!("{}/v1/health/live", harness.base_url))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["ok"], Value::Bool(true));
    assert_eq!(body["data"]["ok"], Value::Bool(true));
    assert!(body["data"]["server"]["version"].is_string());
}

#[tokio::test]
async fn health_live_requires_session_tier_auth() {
    let harness = ServerHarness::spawn().await;
    let response = reqwest::Client::new()
        .get(format!("{}/v1/health/live", harness.base_url))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn health_ready_returns_200_when_subsystems_are_healthy() {
    let harness = ServerHarness::spawn().await;
    let response = reqwest::Client::new()
        .get(format!("{}/v1/health/ready", harness.base_url))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["ok"], Value::Bool(true));
    assert_eq!(body["data"]["ok"], Value::Bool(true));
    assert_eq!(body["data"]["failing"], serde_json::json!([]));
    assert_eq!(body["data"]["sqlite"]["reachable"], Value::Bool(true));
    assert_eq!(body["data"]["workspace"]["writable"], Value::Bool(true));
    assert_eq!(body["data"]["agent"]["id"], "placebo");
    assert_eq!(body["data"]["agent"]["orphaned_process_count"], 0);
    // Default fixture has Supabase disabled; sink subsystem should still report
    // but with `enabled=false`.
    assert_eq!(body["data"]["sink"]["enabled"], Value::Bool(false));
    assert_eq!(body["data"]["mcp"]["configured_count"], Value::from(0));
    assert_eq!(body["data"]["mcp"]["failing_count"], Value::from(0));
}

#[tokio::test]
async fn health_ready_reports_healthy_mcp_declarations() {
    let mut config = test_config();
    config.mcp = McpConfig {
        servers: vec![
            McpServerConfig::Stdio(McpStdioServer {
                name: "local-shell".to_owned(),
                command: "sh".to_owned(),
                args: vec![],
                env: vec![],
            }),
            McpServerConfig::Http(McpHttpServer {
                name: "generic-http".to_owned(),
                url: "https://example.com/mcp".to_owned(),
                headers: vec![],
            }),
        ],
    };
    let harness = ServerHarness::spawn_with_config(config).await;
    let response = reqwest::Client::new()
        .get(format!("{}/v1/health/ready", harness.base_url))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["data"]["mcp"]["configured_count"], Value::from(2));
    assert_eq!(body["data"]["mcp"]["failing_count"], Value::from(0));
    assert_eq!(body["data"]["mcp"]["servers"][0]["kind"], "stdio");
    assert_eq!(body["data"]["mcp"]["servers"][0]["ok"], true);
    assert!(body["data"]["mcp"]["servers"][0]["command_path"].is_string());
    assert_eq!(body["data"]["mcp"]["servers"][1]["kind"], "http");
    assert_eq!(body["data"]["mcp"]["servers"][1]["ok"], true);
}

#[tokio::test]
async fn health_ready_marks_mcp_failing_when_secret_ref_is_missing() {
    let mut config = test_config();
    config.mcp = McpConfig {
        servers: vec![McpServerConfig::Http(McpHttpServer {
            name: "linear".to_owned(),
            url: "https://mcp.linear.app/mcp".to_owned(),
            headers: vec![HttpHeaderRef::from_ref("Authorization", "LINEAR_API_KEY")],
        })],
    };
    let harness = ServerHarness::spawn_with_config(config).await;
    let response = reqwest::Client::new()
        .get(format!("{}/v1/health/ready", harness.base_url))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body: Value = response.json().await.expect("json");
    let failing = body["data"]["failing"].as_array().expect("failing array");
    assert!(failing.iter().any(|value| value == "mcp"));
    assert_eq!(body["data"]["mcp"]["configured_count"], Value::from(1));
    assert_eq!(body["data"]["mcp"]["failing_count"], Value::from(1));
    assert_eq!(body["data"]["mcp"]["servers"][0]["ok"], false);
    assert_eq!(
        body["data"]["mcp"]["servers"][0]["missing_secret_refs"],
        serde_json::json!(["LINEAR_API_KEY"])
    );
}

#[tokio::test]
async fn health_ready_returns_503_when_workspace_is_not_writable() {
    let mut config = test_config();
    // Point workspace at a tempdir child that we deliberately never create.
    // The parent tempdir keeps the path host-agnostic, and skipping the
    // mkdir forces the workspace probe into the failing branch without
    // touching filesystem permissions.
    let missing_workspace = tempfile::tempdir().expect("tempdir for missing workspace");
    let missing_root = missing_workspace.path().join("never-created");
    config.workspace.root = missing_root.to_string_lossy().into_owned();
    config.workspace.uploads = missing_root.join("uploads").to_string_lossy().into_owned();
    let harness = ServerHarness::spawn_with_unmodified_workspace(config).await;
    let response = reqwest::Client::new()
        .get(format!("{}/v1/health/ready", harness.base_url))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body: Value = response.json().await.expect("json");
    // 503 envelope follows the api.md convention: top-level `ok` is false
    // for failing readiness, matching the HTTP status code.
    assert_eq!(body["ok"], Value::Bool(false));
    assert_eq!(body["data"]["ok"], Value::Bool(false));
    let failing = body["data"]["failing"].as_array().expect("failing array");
    assert!(failing.iter().any(|v| v == "workspace"));
    assert_eq!(body["data"]["workspace"]["writable"], Value::Bool(false));
}

#[cfg(unix)]
#[tokio::test]
async fn health_ready_reports_orphaned_agent_process_groups() {
    struct ProcessGroupGuard {
        child: std::process::Child,
        pid: u32,
    }

    impl Drop for ProcessGroupGuard {
        fn drop(&mut self) {
            let Ok(pid) = i32::try_from(self.pid) else {
                return;
            };
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
            let _ = self.child.wait();
        }
    }

    use std::os::unix::process::CommandExt as _;

    let child = std::process::Command::new("sh")
        .arg("-c")
        .arg("sleep 60")
        .process_group(0)
        .spawn()
        .expect("spawn process group");
    let orphan = ProcessGroupGuard {
        pid: child.id(),
        child,
    };
    let harness = ServerHarness::spawn().await;
    {
        let guard = harness.state.lock().await;
        guard
            .append_agent_lifecycle(
                "agent.started",
                "agent initialized",
                &serde_json::json!({
                    "agent_id": "placebo",
                    "pid": orphan.pid,
                    "adapter": null,
                })
                .to_string(),
            )
            .expect("append agent.started");
    }

    let response = reqwest::Client::new()
        .get(format!("{}/v1/health/ready", harness.base_url))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body: Value = response.json().await.expect("json");
    let failing = body["data"]["failing"].as_array().expect("failing array");
    assert!(failing.iter().any(|value| value == "agent"));
    assert_eq!(body["data"]["agent"]["orphaned_process_count"], 1);
    assert_eq!(
        body["data"]["agent"]["orphaned_process_pids"],
        serde_json::json!([orphan.pid])
    );
}

#[tokio::test]
async fn health_ready_surfaces_stuck_prompts_in_failing() {
    // Seed an aged running prompt directly into state, then hit
    // /v1/health/ready. The new prompts subsystem must promote
    // "prompts" into the `failing` list and report a non-zero
    // stuck_count without any sweeper run needed.
    let harness = ServerHarness::spawn().await;
    {
        let guard = harness.state.lock().await;
        guard
            .insert_session(acp_stack::state::NewSessionRecord {
                id: "sess_stuck".to_owned(),
                agent_id: "fake".to_owned(),
                cwd: "/tmp".to_owned(),
                title: None,
                metadata_json: "{}".to_owned(),
            })
            .expect("session inserted");
        guard
            .insert_prompt(acp_stack::state::NewPromptRecord {
                id: "prm_stuck".to_owned(),
                session_id: "sess_stuck".to_owned(),
                prompt_json: "[]".to_owned(),
            })
            .expect("prompt inserted");
        guard
            .update_prompt_status(
                "prm_stuck",
                acp_stack::state::PromptStatus::Running,
                None,
                None,
                None,
                None,
                None,
            )
            .expect("prompt flipped to running");
    }
    // Force `updated_at` into the distant past so the configured
    // threshold (default 5m) is well exceeded.
    let connection = Connection::open(&harness.state_path).expect("open sqlite for age override");
    connection
        .execute(
            "UPDATE prompts SET updated_at = ?1 WHERE id = ?2",
            ("2020-01-01T00:00:00.000000000Z", "prm_stuck"),
        )
        .expect("force-set updated_at");
    drop(connection);

    let response = reqwest::Client::new()
        .get(format!("{}/v1/health/ready", harness.base_url))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body: Value = response.json().await.expect("json");
    let failing = body["data"]["failing"].as_array().expect("failing array");
    assert!(
        failing.iter().any(|v| v == "prompts"),
        "expected 'prompts' in failing, got {failing:?}"
    );
    let prompts = &body["data"]["prompts"];
    assert!(
        prompts["stuck_count"].as_i64().unwrap_or(0) >= 1,
        "stuck_count must surface in PromptsHealth, got {prompts:?}"
    );
    assert!(
        prompts["threshold_secs"].as_i64().unwrap_or(0) > 0,
        "threshold_secs must surface in PromptsHealth, got {prompts:?}"
    );
}

#[tokio::test]
async fn metrics_summary_exposes_prompt_failure_breakdowns() {
    let harness = ServerHarness::spawn().await;
    {
        let guard = harness.state.lock().await;
        guard
            .insert_session(acp_stack::state::NewSessionRecord {
                id: "sess_metrics_prompt_failures".to_owned(),
                agent_id: "fake".to_owned(),
                cwd: "/tmp".to_owned(),
                title: None,
                metadata_json: "{}".to_owned(),
            })
            .expect("session inserted");
        guard
            .insert_prompt(acp_stack::state::NewPromptRecord {
                id: "prm_metrics_inference".to_owned(),
                session_id: "sess_metrics_prompt_failures".to_owned(),
                prompt_json: "[]".to_owned(),
            })
            .expect("prompt inserted");
        assert!(
            guard
                .update_prompt_status(
                    "prm_metrics_inference",
                    acp_stack::state::PromptStatus::Errored,
                    None,
                    Some("agent.inference_5xx"),
                    Some("inference endpoint returned 503 (service_unavailable)"),
                    Some(acp_stack::state::FailureClass::Inference5xx.as_str()),
                    Some(r#"{"status_code":503,"reason_category":"service_unavailable"}"#),
                )
                .expect("prompt failure update"),
            "prompt failure update should apply"
        );
        guard
            .append_session_event_with_source(
                "sess_metrics_prompt_failures",
                "warn",
                acp_stack::state::EVENT_KIND_PROMPT_INFERENCE_FAILED,
                acp_stack::state::EVENT_SOURCE_SYSTEM,
                "inference endpoint failure",
                r#"{"prompt_id":"prm_metrics_inference","status_code":503,"reason_category":"service_unavailable"}"#,
            )
            .expect("inference event inserted");
    }

    let response = reqwest::Client::new()
        .get(format!("{}/v1/metrics/summary?since=1h", harness.base_url))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    let prompt_failures = &body["data"]["prompt_failures"];
    assert_eq!(prompt_failures["total"], 1);
    assert_eq!(prompt_failures["inference_5xx"], 1);
    assert_eq!(prompt_failures["by_class"]["inference_5xx"], 1);
    assert_eq!(prompt_failures["by_status_code"]["503"], 1);
    assert_eq!(
        prompt_failures["by_reason_category"]["service_unavailable"],
        1
    );
}

#[tokio::test]
async fn metrics_summary_exposes_maximum_context_window_usage() {
    let harness = ServerHarness::spawn().await;
    {
        let guard = harness.state.lock().await;
        for used in [1024, 4096, 2048] {
            guard
                .append_event_with_source(
                    "info",
                    "usage.reported",
                    acp_stack::state::EVENT_SOURCE_ACP,
                    "agent usage reported",
                    &serde_json::json!({
                        "context_window_used": used,
                        "context_window_max": 32768
                    })
                    .to_string(),
                )
                .expect("append usage event");
        }
    }

    let response = reqwest::Client::new()
        .get(format!("{}/v1/metrics/summary?since=1h", harness.base_url))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["data"]["usage"]["context_window_used_max"], 4096);
    assert_eq!(body["data"]["usage"]["context_window_max"], 32768);
}

#[tokio::test]
async fn metrics_summary_exposes_api_request_breakdowns() {
    let harness = ServerHarness::spawn().await;
    {
        let guard = harness.state.lock().await;
        guard
            .append_event_with_source(
                "info",
                "api.request",
                acp_stack::state::EVENT_SOURCE_API,
                "",
                r#"{"method":"GET","path":"/v1/sessions","status":200,"duration_ms":10,"key_kind":"session","origin":{"origin_kind":"cloudflare","country_code":"US","region_code":"CA"}}"#,
            )
            .expect("append api request");
        guard
            .append_event_with_source(
                "info",
                "api.request",
                acp_stack::state::EVENT_SOURCE_LOCAL,
                "",
                r#"{"method":"POST","path":"/v1/commands","status":503,"duration_ms":20,"key_kind":null,"origin":{"origin_kind":"direct"}}"#,
            )
            .expect("append local api request");
    }

    let response = reqwest::Client::new()
        .get(format!("{}/v1/metrics/summary?since=1h", harness.base_url))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    let api_connections = &body["data"]["api_connections"];
    assert_eq!(api_connections["request_count"], 2);
    assert_eq!(api_connections["by_status"]["2xx"], 1);
    assert_eq!(api_connections["by_status"]["5xx"], 1);
    assert_eq!(api_connections["by_method"]["GET"], 1);
    assert_eq!(api_connections["by_method"]["POST"], 1);
    assert_eq!(api_connections["by_route"]["/v1/sessions"], 1);
    assert_eq!(api_connections["by_route"]["/v1/commands"], 1);
    assert_eq!(api_connections["by_key_kind"]["session"], 1);
    assert_eq!(api_connections["by_key_kind"]["unknown"], 1);
    assert_eq!(api_connections["by_source"]["api"], 1);
    assert_eq!(api_connections["by_source"]["local"], 1);
    assert_eq!(api_connections["by_origin_kind"]["cloudflare"], 1);
    assert_eq!(api_connections["by_origin_kind"]["direct"], 1);
    assert_eq!(api_connections["by_country"]["US"], 1);
    assert_eq!(api_connections["by_country"]["unknown"], 1);
    assert_eq!(api_connections["by_region"]["CA"], 1);
    assert_eq!(api_connections["by_region"]["unknown"], 1);
    assert_eq!(api_connections["average_duration_ms"], 15);
}

#[tokio::test]
async fn mark_stalled_prompts_appends_stalled_event_when_invoked_directly() {
    // Verify the sweeper's persistence path end-to-end without spawning the
    // background task: seed an aged row, invoke `mark_stalled_prompts`, then
    // append the matching session event the sweeper would have emitted, and
    // assert the event surfaces via `GET /v1/sessions/{id}/events`.
    let harness = ServerHarness::spawn().await;
    {
        let guard = harness.state.lock().await;
        guard
            .insert_session(acp_stack::state::NewSessionRecord {
                id: "sess_stall_evt".to_owned(),
                agent_id: "fake".to_owned(),
                cwd: "/tmp".to_owned(),
                title: None,
                metadata_json: "{}".to_owned(),
            })
            .expect("session inserted");
        guard
            .insert_prompt(acp_stack::state::NewPromptRecord {
                id: "prm_stall_evt".to_owned(),
                session_id: "sess_stall_evt".to_owned(),
                prompt_json: "[]".to_owned(),
            })
            .expect("prompt inserted");
        guard
            .update_prompt_status(
                "prm_stall_evt",
                acp_stack::state::PromptStatus::Running,
                None,
                None,
                None,
                None,
                None,
            )
            .expect("prompt flipped to running");
    }
    let connection = Connection::open(&harness.state_path).expect("open sqlite for age override");
    connection
        .execute(
            "UPDATE prompts SET updated_at = ?1 WHERE id = ?2",
            ("2020-01-01T00:00:00.000000000Z", "prm_stall_evt"),
        )
        .expect("force-set updated_at");
    drop(connection);

    {
        let guard = harness.state.lock().await;
        let pairs = guard
            .mark_stalled_prompts(std::time::Duration::from_secs(60), "test stall")
            .expect("mark_stalled_prompts should run");
        assert_eq!(pairs.len(), 1);
        // Mirror the sweeper's emit so the events surface for the API check.
        let payload = serde_json::json!({
            "prompt_id": pairs[0].0,
            "threshold_secs": 60u64,
        })
        .to_string();
        guard
            .append_session_event_with_source(
                &pairs[0].1,
                "warn",
                acp_stack::state::EVENT_KIND_PROMPT_STALLED,
                acp_stack::state::EVENT_SOURCE_SYSTEM,
                "prompt stalled",
                &payload,
            )
            .expect("append prompt.stalled event");
    }

    let response = reqwest::Client::new()
        .get(format!(
            "{}/v1/sessions/sess_stall_evt/events",
            harness.base_url
        ))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    let events = body["data"]["events"]
        .as_array()
        .expect("events array present");
    assert!(
        events
            .iter()
            .any(|event| event["kind"].as_str() == Some("prompt.stalled")),
        "expected prompt.stalled event, got {events:?}"
    );
}

#[tokio::test]
async fn health_live_does_not_persist_api_request_row() {
    // `/v1/health/live` is contracted to skip the state-store touch that
    // every other route gets through `log_api_request`. Regression test for
    // the Codex-audit finding that the original implementation logged each
    // liveness probe as an `api.request` row.
    let harness = ServerHarness::spawn().await;
    let response = reqwest::Client::new()
        .get(format!("{}/v1/health/live", harness.base_url))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let guard = harness.state.lock().await;
    let events = guard
        .query_events(EventFilter {
            limit: 100,
            kind: Some("api.request"),
            ..EventFilter::default()
        })
        .expect("query api.request events");
    assert!(
        !events
            .iter()
            .any(|e| e.payload_json.contains("\"/v1/health/live\"")),
        "`/v1/health/live` should not produce api.request rows, got {events:?}"
    );
}

#[tokio::test]
async fn health_ready_does_not_persist_api_request_row() {
    // Mirror of `health_live_does_not_persist_api_request_row`. The readiness
    // endpoint is the canonical orchestrator poll surface (k8s probes, LBs,
    // Cloudflare health checks), so logging an `api.request` row for each
    // poll would dwarf real traffic — same cardinality concern as
    // `/v1/status*`. Regression test guards the entry in the skip list.
    let harness = ServerHarness::spawn().await;
    let response = reqwest::Client::new()
        .get(format!("{}/v1/health/ready", harness.base_url))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let guard = harness.state.lock().await;
    let events = guard
        .query_events(EventFilter {
            limit: 100,
            kind: Some("api.request"),
            ..EventFilter::default()
        })
        .expect("query api.request events");
    assert!(
        !events
            .iter()
            .any(|e| e.payload_json.contains("\"/v1/health/ready\"")),
        "`/v1/health/ready` should not produce api.request rows, got {events:?}"
    );
}

#[tokio::test]
async fn health_ready_marks_deps_failing_when_last_apply_failed() {
    use acp_stack::state::{
        INSTALLER_METHOD_SHELL, INSTALLER_OPERATION_INSTALL, InstallerRunInput,
    };

    let harness = ServerHarness::spawn().await;
    {
        let guard = harness.state.lock().await;
        guard
            .append_installer_run(InstallerRunInput {
                agent_id: "deps_apply",
                started_at: "2026-05-25T00:00:00.000000000Z",
                finished_at: Some("2026-05-25T00:00:01.000000000Z"),
                status: "failed",
                stdout: "",
                stderr: "boom",
                exit_status: Some(1),
                step: "deps_apply",
                version: None,
                operation: INSTALLER_OPERATION_INSTALL,
                method: Some(INSTALLER_METHOD_SHELL),
                log_dir: None,
                apply_run_id: Some("dap_api_failed"),
            })
            .expect("seed failed deps_apply row");
    }
    let response = reqwest::Client::new()
        .get(format!("{}/v1/health/ready", harness.base_url))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["ok"], Value::Bool(false));
    let failing = body["data"]["failing"].as_array().expect("failing array");
    assert!(failing.iter().any(|v| v == "deps"));
    assert_eq!(body["data"]["deps"]["last_apply_status"], "failed");
    assert_eq!(body["data"]["deps"]["last_apply_exit"], Value::from(1));
    assert_eq!(
        body["data"]["deps"]["last_apply_run_id"],
        Value::from("dap_api_failed")
    );
}

#[tokio::test]
async fn health_ready_marks_sink_failing_when_open_failures_exist() {
    let mut config = test_config();
    if let Some(supabase) = config.logging.supabase.as_mut() {
        supabase.enabled = true;
    }
    let harness = ServerHarness::spawn_with_config(config).await;
    {
        let mut guard = harness.state.lock().await;
        guard.set_external_logging_enabled(true);
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        guard
            .append_event_with_source(
                "info",
                "test.seed",
                acp_stack::state::EVENT_SOURCE_CLI,
                "seed sink_outbox row",
                "{}",
            )
            .expect("append seed event");
        let batch = guard
            .next_sink_outbox_batch(10, &now)
            .expect("read outbox batch");
        let ids: Vec<String> = batch.iter().map(|row| row.id.clone()).collect();
        assert!(!ids.is_empty(), "seed event should enqueue an outbox row");
        guard
            .mark_sink_outbox_failure(&ids, "boom", &now, &now)
            .expect("mark outbox failure");
    }
    let response = reqwest::Client::new()
        .get(format!("{}/v1/health/ready", harness.base_url))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["ok"], Value::Bool(false));
    let failing = body["data"]["failing"].as_array().expect("failing array");
    assert!(failing.iter().any(|v| v == "sink"));
    assert_eq!(body["data"]["sink"]["enabled"], Value::Bool(true));
    assert_eq!(body["data"]["sink"]["open_failure_count"], Value::from(1));
}

#[tokio::test]
async fn metrics_summary_counts_existing_state_rows() {
    let harness = ServerHarness::spawn().await;
    seed_session(
        &harness.state_path,
        "sess_1",
        "open",
        "2026-05-14T00:00:00.000000000Z",
        "2026-05-14T00:00:01.000000000Z",
    );
    seed_command(
        &harness.state_path,
        "cmd_1",
        "succeeded",
        "echo hi",
        Some(0),
        "2026-05-14T00:00:02.000000000Z",
        "2026-05-14T00:00:03.000000000Z",
    );
    {
        let guard = harness.state.lock().await;
        guard
            .append_event("info", "permission.requested", "permission requested", "{}")
            .expect("append permission event");
        guard
            .append_auth_failure("unknown", "invalid", None, Some("/v1/status"), "{}")
            .expect("append auth failure");
        guard
            .append_agent_lifecycle("server.started", "started", "{}")
            .expect("append lifecycle");
    }

    // The default window is 24h; the seeded fixtures use fixed historical dates,
    // so use an absolute lower bound for stable count assertions.
    let response = reqwest::Client::new()
        .get(format!(
            "{}/v1/metrics/summary?since=2000-01-01T00:00:00Z",
            harness.base_url
        ))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    let counts = &body["data"]["counts"];
    assert_eq!(counts["sessions"], Value::Number(1.into()));
    assert_eq!(counts["commands"], Value::Number(1.into()));
    assert_eq!(counts["auth_failures"], Value::Number(1.into()));
    assert_eq!(counts["agent_lifecycle"], Value::Number(1.into()));
    assert_eq!(counts["events"], Value::Number(1.into()));
    // The window envelope should also be present and well-formed.
    assert!(body["data"]["window"]["since"].is_string());
    assert!(body["data"]["window"]["until"].is_string());
    // New derived blocks are always emitted even when their inputs are
    // missing — the metrics consumer relies on the keys being present.
    assert!(body["data"]["sessions"]["active"].is_number());
    assert!(body["data"]["commands"]["total"].is_number());
    assert!(body["data"]["permissions"]["total"].is_number());
    assert!(body["data"]["security"]["auth_failures"].is_number());
}
