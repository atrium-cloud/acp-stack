//! API security-surface integration tests: `/v1/security/*`, HTTP hardening
//! (body limits, origins, rate limiting) and auth-failure audit behavior.

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use acp_stack::api::{self, AppState, RuntimePaths};
use acp_stack::config::{DependenciesConfig, DependencyEntry};
use acp_stack::state::StateStore;
use reqwest::StatusCode;
use serde_json::Value;
use tokio::net::TcpListener;

mod common;
use common::api::{
    ADMIN_KEY, SESSION_KEY, ServerHarness, create_runtime_files, seed_auth_failure, test_config,
};

#[tokio::test]
async fn wrong_kind_auth_failure_uses_trusted_forwarded_client_ip() {
    let mut config = test_config();
    config.security.http.trust_proxy_headers = true;
    config.security.http.trusted_proxies = vec!["127.0.0.1".to_owned()];
    let harness = ServerHarness::spawn_with_config(config).await;

    let response = reqwest::Client::new()
        .get(format!("{}/v1/status", harness.base_url))
        .header("Authorization", format!("Bearer {ADMIN_KEY}"))
        .header("X-Forwarded-For", "203.0.113.9")
        .send()
        .await
        .expect("send");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let (kind, reason) = harness.latest_auth_failure().await;
    assert_eq!(kind, "admin");
    assert_eq!(reason, "wrong_kind");
    assert_eq!(
        harness.latest_auth_failure_client_ip().await.as_deref(),
        Some("203.0.113.9")
    );
}

#[tokio::test]
async fn security_check_requires_admin_key() {
    let harness = ServerHarness::spawn().await;
    let client = reqwest::Client::new();

    let session_response = client
        .get(format!("{}/v1/security/check", harness.base_url))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("send");
    assert_eq!(session_response.status(), StatusCode::UNAUTHORIZED);
    let body: Value = session_response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "auth.wrong_kind");

    let admin_response = client
        .get(format!("{}/v1/security/check", harness.base_url))
        .header("Authorization", format!("Bearer {ADMIN_KEY}"))
        .send()
        .await
        .expect("send");
    assert_eq!(admin_response.status(), StatusCode::OK);
    let body: Value = admin_response.json().await.expect("json");
    assert_eq!(body["ok"], Value::Bool(true));
    assert_eq!(body["data"]["ok"], Value::Bool(true));
    assert_eq!(body["data"]["status"], Value::String("succeeded".into()));
    assert!(
        body["data"]["findings"]
            .as_array()
            .expect("findings")
            .is_empty()
    );
    // run_id is the durable handle into `acps security show`; it must be
    // present even on clean runs so the operator can correlate the response
    // with the persisted history row.
    let run_id = body["data"]["run_id"].as_str().expect("run_id present");
    assert!(
        run_id.starts_with("srun_"),
        "run_id should follow the srun_ prefix convention, got {run_id}"
    );
}

#[tokio::test]
async fn security_check_persists_history_row() {
    let harness = ServerHarness::spawn().await;
    let client = reqwest::Client::new();
    let first = client
        .get(format!("{}/v1/security/check", harness.base_url))
        .header("Authorization", format!("Bearer {ADMIN_KEY}"))
        .send()
        .await
        .expect("send")
        .json::<Value>()
        .await
        .expect("json");
    let run_id = first["data"]["run_id"]
        .as_str()
        .expect("run_id present")
        .to_owned();

    let history = client
        .get(format!("{}/v1/security/history", harness.base_url))
        .header("Authorization", format!("Bearer {ADMIN_KEY}"))
        .send()
        .await
        .expect("send")
        .json::<Value>()
        .await
        .expect("json");
    let runs = history["data"]["runs"]
        .as_array()
        .expect("runs array")
        .clone();
    assert_eq!(runs.len(), 1);
    let summary = &runs[0];
    assert_eq!(summary["id"], Value::String(run_id.clone()));
    assert_eq!(summary["status"], Value::String("succeeded".into()));
    assert_eq!(summary["ok"], Value::Bool(true));
    assert_eq!(summary["critical_count"], Value::from(0));
    assert_eq!(summary["warning_count"], Value::from(0));

    let show = client
        .get(format!("{}/v1/security/history/{run_id}", harness.base_url))
        .header("Authorization", format!("Bearer {ADMIN_KEY}"))
        .send()
        .await
        .expect("send")
        .json::<Value>()
        .await
        .expect("json");
    assert_eq!(show["data"]["run"]["id"], Value::String(run_id));
    assert!(
        show["data"]["findings"]
            .as_array()
            .expect("findings")
            .is_empty()
    );
}

#[tokio::test]
async fn security_history_requires_admin_key() {
    let harness = ServerHarness::spawn().await;
    let client = reqwest::Client::new();

    let session_response = client
        .get(format!("{}/v1/security/history", harness.base_url))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("send");
    assert_eq!(session_response.status(), StatusCode::UNAUTHORIZED);

    let session_show = client
        .get(format!(
            "{}/v1/security/history/srun_does_not_exist",
            harness.base_url
        ))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("send");
    assert_eq!(session_show.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn security_history_show_returns_404_for_unknown_run() {
    let harness = ServerHarness::spawn().await;
    let response = reqwest::Client::new()
        .get(format!(
            "{}/v1/security/history/srun_does_not_exist",
            harness.base_url
        ))
        .header("Authorization", format!("Bearer {ADMIN_KEY}"))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "security.run_not_found");
}

#[tokio::test]
async fn security_history_paginates_with_keyset_cursor() {
    let harness = ServerHarness::spawn().await;
    let client = reqwest::Client::new();
    // Three sequential checks; each creates a fresh history row.
    let mut ids = Vec::new();
    for _ in 0..3 {
        let body: Value = client
            .get(format!("{}/v1/security/check", harness.base_url))
            .header("Authorization", format!("Bearer {ADMIN_KEY}"))
            .send()
            .await
            .expect("send")
            .json()
            .await
            .expect("json");
        ids.push(body["data"]["run_id"].as_str().expect("run_id").to_owned());
    }

    let first: Value = client
        .get(format!("{}/v1/security/history?limit=2", harness.base_url))
        .header("Authorization", format!("Bearer {ADMIN_KEY}"))
        .send()
        .await
        .expect("send")
        .json()
        .await
        .expect("json");
    let first_runs = first["data"]["runs"].as_array().expect("runs").clone();
    assert_eq!(first_runs.len(), 2);
    assert_eq!(first_runs[0]["id"], Value::String(ids[2].clone()));
    assert_eq!(first_runs[1]["id"], Value::String(ids[1].clone()));
    let cursor = first["data"]["next_cursor"]
        .as_str()
        .expect("cursor present when page full")
        .to_owned();
    assert_eq!(cursor, ids[1]);

    let second: Value = client
        .get(format!(
            "{}/v1/security/history?limit=2&after={cursor}",
            harness.base_url
        ))
        .header("Authorization", format!("Bearer {ADMIN_KEY}"))
        .send()
        .await
        .expect("send")
        .json()
        .await
        .expect("json");
    let second_runs = second["data"]["runs"].as_array().expect("runs").clone();
    assert_eq!(second_runs.len(), 1);
    assert_eq!(second_runs[0]["id"], Value::String(ids[0].clone()));
    assert!(
        second["data"].get("next_cursor").is_none() || second["data"]["next_cursor"].is_null(),
        "next_cursor should be absent on the final page"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn security_history_show_preserves_finding_order_and_details() {
    let harness = ServerHarness::spawn().await;
    // Loosen the state DB so a critical path_mode_loose finding is emitted
    // with structured details attached to it.
    std::fs::set_permissions(&harness.state_path, std::fs::Permissions::from_mode(0o644))
        .expect("loosen state db mode");
    let client = reqwest::Client::new();
    let check: Value = client
        .get(format!("{}/v1/security/check", harness.base_url))
        .header("Authorization", format!("Bearer {ADMIN_KEY}"))
        .send()
        .await
        .expect("send")
        .json()
        .await
        .expect("json");
    let run_id = check["data"]["run_id"].as_str().expect("run_id").to_owned();
    let live_findings = check["data"]["findings"]
        .as_array()
        .expect("findings")
        .clone();
    let live_codes: Vec<&str> = live_findings
        .iter()
        .map(|f| f["code"].as_str().expect("code"))
        .collect();
    assert!(live_codes.contains(&"runtime.path_mode_loose"));

    let show: Value = client
        .get(format!("{}/v1/security/history/{run_id}", harness.base_url))
        .header("Authorization", format!("Bearer {ADMIN_KEY}"))
        .send()
        .await
        .expect("send")
        .json()
        .await
        .expect("json");
    let recorded = show["data"]["findings"]
        .as_array()
        .expect("findings")
        .clone();
    let recorded_codes: Vec<&str> = recorded
        .iter()
        .map(|f| f["code"].as_str().expect("code"))
        .collect();
    assert_eq!(live_codes, recorded_codes, "order must be preserved");

    let path_mode = recorded
        .iter()
        .find(|f| f["code"] == "runtime.path_mode_loose")
        .expect("path_mode_loose finding");
    let details = path_mode
        .get("details")
        .expect("details payload present on path_mode_loose");
    assert!(
        details["path"].as_str().is_some(),
        "details.path should be set"
    );
    assert!(
        details["kind"].as_str().is_some(),
        "details.kind should be set"
    );
}

#[tokio::test]
async fn security_check_reports_public_bind_proxy_and_auth_failure_findings() {
    let mut config = test_config();
    config.api.bind = "0.0.0.0:7700".to_owned();
    config.security.http.allowed_origins = vec!["*".to_owned()];
    config.security.http.trust_proxy_headers = true;
    config.security.http.auth_failures_per_minute = 2;
    let harness = ServerHarness::spawn_with_config(config).await;
    {
        let guard = harness.state.lock().await;
        for _ in 0..2 {
            guard
                .append_auth_failure("unknown", "invalid", None, Some("/v1/status"), "{}")
                .expect("append auth failure");
        }
    }

    let response = reqwest::Client::new()
        .get(format!("{}/v1/security/check", harness.base_url))
        .header("Authorization", format!("Bearer {ADMIN_KEY}"))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["data"]["ok"], Value::Bool(false));
    let findings = body["data"]["findings"].as_array().expect("findings");
    let codes: Vec<&str> = findings
        .iter()
        .map(|finding| finding["code"].as_str().expect("finding code"))
        .collect();
    assert!(codes.contains(&"api.public_bind"));
    assert!(codes.contains(&"http.wildcard_origin_public_bind"));
    assert!(codes.contains(&"http.trust_proxy_without_trusted_proxies"));
    assert!(codes.contains(&"auth.failure_threshold"));

    // Every finding in the response must carry an operator-actionable
    // remediation string. Asserted here so a regression in `SecurityFinding`
    // construction shows up in the integration tier, not just in the unit
    // tests for `security::check`.
    for finding in findings {
        let code = finding["code"].as_str().expect("code");
        let remediation = finding["remediation"]
            .as_str()
            .unwrap_or_else(|| panic!("finding {code} has no remediation in JSON"));
        assert!(
            !remediation.is_empty(),
            "finding {code} has an empty remediation string"
        );
    }

    // Spot-check that hint text actually names something the operator can do,
    // not just describe the problem again.
    let trust_proxy = findings
        .iter()
        .find(|f| f["code"] == "http.trust_proxy_without_trusted_proxies")
        .expect("trust_proxy finding present");
    assert!(
        trust_proxy["remediation"]
            .as_str()
            .expect("remediation")
            .contains("trusted_proxies")
    );
    let auth_threshold = findings
        .iter()
        .find(|f| f["code"] == "auth.failure_threshold")
        .expect("auth_failure_threshold finding present");
    assert!(
        auth_threshold["remediation"]
            .as_str()
            .expect("remediation")
            .contains("/v1/logs/security")
    );
}

#[tokio::test]
async fn security_check_persists_required_dependency_finding() {
    let mut config = test_config();
    config.dependencies = DependenciesConfig {
        commands: vec![DependencyEntry {
            name: "definitely-missing-required-dep-12345".to_owned(),
            required: true,
            feature: Some("test-feature".to_owned()),
            install: None,
        }],
        ..DependenciesConfig::default()
    };
    let harness = ServerHarness::spawn_with_config(config).await;
    let client = reqwest::Client::new();

    let check: Value = client
        .get(format!("{}/v1/security/check", harness.base_url))
        .header("Authorization", format!("Bearer {ADMIN_KEY}"))
        .send()
        .await
        .expect("send")
        .json()
        .await
        .expect("json");
    let run_id = check["data"]["run_id"].as_str().expect("run_id");
    let live_finding = check["data"]["findings"]
        .as_array()
        .expect("findings")
        .iter()
        .find(|finding| finding["code"] == "deps.required_unavailable")
        .expect("dependency finding");
    assert_eq!(live_finding["details"]["total"], Value::from(1));
    assert_eq!(
        live_finding["details"]["dependencies"][0]["name"],
        "definitely-missing-required-dep-12345"
    );
    assert!(
        live_finding["remediation"]
            .as_str()
            .expect("remediation")
            .contains("acps deps check")
    );

    let show: Value = client
        .get(format!("{}/v1/security/history/{run_id}", harness.base_url))
        .header("Authorization", format!("Bearer {ADMIN_KEY}"))
        .send()
        .await
        .expect("send")
        .json()
        .await
        .expect("json");
    let recorded = show["data"]["findings"]
        .as_array()
        .expect("recorded findings")
        .iter()
        .find(|finding| finding["code"] == "deps.required_unavailable")
        .expect("recorded dependency finding");
    assert_eq!(recorded["details"], live_finding["details"]);
    assert_eq!(recorded["remediation"], live_finding["remediation"]);
}

#[cfg(unix)]
#[tokio::test]
async fn security_check_reports_loose_state_db_mode() {
    let harness = ServerHarness::spawn().await;
    std::fs::set_permissions(&harness.state_path, std::fs::Permissions::from_mode(0o644))
        .expect("loosen state db mode");

    let response = reqwest::Client::new()
        .get(format!("{}/v1/security/check", harness.base_url))
        .header("Authorization", format!("Bearer {ADMIN_KEY}"))
        .send()
        .await
        .expect("security check response");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    let findings = body["data"]["findings"].as_array().expect("findings");
    let finding = findings
        .iter()
        .find(|finding| {
            finding["code"] == "runtime.path_mode_loose"
                && finding["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("state database"))
        })
        .expect("state database mode finding");
    assert!(
        finding["remediation"]
            .as_str()
            .expect("remediation")
            .contains("chmod 0600")
    );
}

#[tokio::test]
async fn security_check_uses_effective_bind_and_recent_auth_failures_only() {
    let mut config = test_config();
    config.api.bind = "127.0.0.1:7700".to_owned();
    config.security.http.auth_failures_per_minute = 1;
    let tempdir = tempfile::tempdir().expect("tempdir");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state open");
    store.migrate().expect("migrate");
    seed_auth_failure(&path, "af_old", "2000-01-01T00:00:00.000000000Z", "invalid");
    let config_path = create_runtime_files(tempdir.path(), &path);
    let app_state = AppState::with_effective_bind_and_runtime_paths(
        config,
        store,
        SESSION_KEY.to_owned(),
        ADMIN_KEY.to_owned(),
        "0.0.0.0:7700".to_owned(),
        RuntimePaths::new(config_path.clone(), path.clone()),
    );
    let state = app_state.state.clone();
    let local_session_auth = app_state.local_session_auth.clone();
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let local = listener.local_addr().expect("local addr");
    let join = tokio::spawn(async move { api::serve(app_state, listener).await });
    let harness = ServerHarness {
        base_url: format!("http://{local}"),
        state,
        local_session_auth,
        config_path,
        state_path: path,
        join,
        _tempdir: tempdir,
    };

    let response = reqwest::Client::new()
        .get(format!("{}/v1/security/check", harness.base_url))
        .header("Authorization", format!("Bearer {ADMIN_KEY}"))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    let findings = body["data"]["findings"].as_array().expect("findings");
    let codes: Vec<&str> = findings
        .iter()
        .map(|finding| finding["code"].as_str().expect("finding code"))
        .collect();
    assert!(codes.contains(&"api.public_bind"));
    assert!(
        !codes.contains(&"auth.failure_threshold"),
        "old auth failures should not trip the per-minute threshold"
    );
}

#[tokio::test]
async fn duplicate_authorization_headers_are_rejected() {
    let harness = ServerHarness::spawn().await;
    let before = harness.auth_failure_count().await;
    // reqwest accepts multiple values for the same header; send two so the
    // server sees a request with two Authorization values.
    let response = reqwest::Client::new()
        .get(format!("{}/v1/status", harness.base_url))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
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
async fn unknown_path_returns_envelope_not_plain_404() {
    let harness = ServerHarness::spawn().await;
    // Authenticated session calling a route that does not exist. Without the
    // envelope-rewrapping middleware, axum's fallback would return a plain
    // text 404 with no `{ok:false, ...}` body.
    let response = reqwest::Client::new()
        .get(format!("{}/v1/nope", harness.base_url))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body: Value = response.json().await.expect("envelope json");
    assert_eq!(body["ok"], Value::Bool(false));
    assert_eq!(body["error"]["code"], "not_found");
}

#[tokio::test]
async fn medium_request_body_does_not_hit_axum_default_limit() {
    // Axum's default extractor limit is 2 MiB. Confirm a 4 MiB body — well
    // below the configured 100 MiB cap — is accepted (with a 400 only from
    // TOML parsing). Without DefaultBodyLimit::disable() this would 413.
    let harness = ServerHarness::spawn().await;
    let body = vec![b'a'; 4 * 1024 * 1024];
    let response = reqwest::Client::new()
        .post(format!("{}/v1/config/validate", harness.base_url))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .body(body)
        .send()
        .await
        .expect("send");
    // 4 MiB of `a`s is invalid TOML, so 400 (config.invalid) is the expected
    // outcome — what matters is that the body was not silently size-capped.
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let json: Value = response.json().await.expect("json");
    assert_eq!(json["error"]["code"], "config.invalid");
}

#[tokio::test]
async fn oversize_body_with_bad_auth_records_auth_failure_first() {
    // Reorder ensures auth runs ahead of body_limit: an oversized body with
    // missing/invalid auth must still leave an `auth_failures` row. Without
    // this ordering, body_limit shortcircuits to 413 and the durable
    // hardening trail is broken.
    //
    // The server returns 401 immediately on bad auth and closes the
    // connection; reqwest may surface that as either a clean 401 response
    // or a `ConnectionReset` (when it was still streaming the oversize
    // body). The durable signal is the `auth_failures` row, which is
    // written before the response is sent.
    let harness = ServerHarness::spawn().await;
    let before = harness.auth_failure_count().await;
    let body = vec![b'a'; 200 * 1024 * 1024]; // 200 MiB, well over the 100 MiB cap
    let outcome = reqwest::Client::new()
        .post(format!("{}/v1/config/validate", harness.base_url))
        .header("Authorization", "Bearer not_a_real_key")
        .body(body)
        .send()
        .await;
    if let Ok(response) = outcome {
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    assert_eq!(harness.auth_failure_count().await, before + 1);
    let (kind, reason) = harness.latest_auth_failure().await;
    assert_eq!(kind, "unknown");
    assert_eq!(reason, "invalid");
}

#[tokio::test]
async fn method_not_allowed_preserves_allow_header() {
    // POST against a GET-only route. axum returns 405 with an `Allow`
    // header. ensure_envelope rewraps the body but must preserve the
    // semantic header so method-discovery keeps working.
    let harness = ServerHarness::spawn().await;
    let response = reqwest::Client::new()
        .post(format!("{}/v1/status", harness.base_url))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    let allow = response
        .headers()
        .get("Allow")
        .expect("Allow header preserved")
        .to_str()
        .expect("Allow ASCII");
    assert!(
        allow.contains("GET"),
        "Allow header should advertise GET, got {allow:?}"
    );
}

#[tokio::test]
async fn oversize_body_with_admin_key_on_session_route_logs_wrong_kind() {
    // Strict-tiering contract: admin keys on session routes are rejected
    // BEFORE body_limit sees the request, even when the body is oversized.
    // Otherwise tower-http would 413 and swallow the wrong_kind signal.
    let harness = ServerHarness::spawn().await;
    let before = harness.auth_failure_count().await;
    let body = vec![b'a'; 200 * 1024 * 1024]; // 200 MiB, well over the 100 MiB cap
    let outcome = reqwest::Client::new()
        .post(format!("{}/v1/config/validate", harness.base_url))
        .header("Authorization", format!("Bearer {ADMIN_KEY}"))
        .body(body)
        .send()
        .await;
    if let Ok(response) = outcome {
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    assert_eq!(harness.auth_failure_count().await, before + 1);
    let (kind, reason) = harness.latest_auth_failure().await;
    assert_eq!(kind, "admin");
    assert_eq!(reason, "wrong_kind");
}

#[tokio::test]
async fn oversize_request_body_returns_413() {
    let mut config = test_config();
    config.api.max_request_bytes = 16;
    config.security.http.max_request_bytes = 16;
    let harness = ServerHarness::spawn_with_config(config).await;
    let body = vec![b'a'; 17];
    let response = reqwest::Client::new()
        .post(format!("{}/v1/config/validate", harness.base_url))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .body(body)
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn oversize_request_body_records_security_event() {
    let mut config = test_config();
    config.api.max_request_bytes = 16;
    config.security.http.max_request_bytes = 16;
    let harness = ServerHarness::spawn_with_config(config).await;

    let response = reqwest::Client::new()
        .post(format!("{}/v1/config/validate", harness.base_url))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .body(vec![b'a'; 17])
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "request.too_large");

    let logs_response = reqwest::Client::new()
        .get(format!("{}/v1/logs/security", harness.base_url))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("send");
    assert_eq!(logs_response.status(), StatusCode::OK);
    let logs_body: Value = logs_response.json().await.expect("json");
    let events = logs_body["data"]["events"]
        .as_array()
        .expect("events array");
    let event = events
        .iter()
        .find(|e| e["kind"] == "security.request_oversized")
        .expect("expected oversized security event");
    let payload: Value =
        serde_json::from_str(event["payload_json"].as_str().expect("payload_json"))
            .expect("payload json");
    assert_eq!(payload["route"], "/v1/config/validate");
    assert_eq!(payload["method"], "POST");
    assert_eq!(payload["limit_bytes"], 16);
    assert!(payload.get("body").is_none());
    assert!(payload.get("bearer").is_none());
}

#[tokio::test]
async fn disallowed_http_origin_returns_403_and_records_security_event() {
    let mut config = test_config();
    config.security.http.allowed_origins = vec!["https://allowed.example".to_owned()];
    let harness = ServerHarness::spawn_with_config(config).await;

    let response = reqwest::Client::new()
        .get(format!("{}/v1/status", harness.base_url))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .header("Origin", "https://blocked.example")
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "auth.origin_not_allowed");

    let logs_response = reqwest::Client::new()
        .get(format!("{}/v1/logs/security", harness.base_url))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("send");
    assert_eq!(logs_response.status(), StatusCode::OK);
    let logs_body: Value = logs_response.json().await.expect("json");
    let events = logs_body["data"]["events"]
        .as_array()
        .expect("events array");
    let event = events
        .iter()
        .find(|e| e["kind"] == "security.cors_origin_denied")
        .expect("expected cors denial security event");
    let payload: Value =
        serde_json::from_str(event["payload_json"].as_str().expect("payload_json"))
            .expect("payload json");
    assert_eq!(payload["origin"], "https://blocked.example");
    assert_eq!(payload["route"], "/v1/status");
    assert_eq!(payload["method"], "GET");
}

#[tokio::test]
async fn allowed_http_origin_succeeds_with_cors_header() {
    let mut config = test_config();
    config.security.http.allowed_origins = vec!["https://allowed.example".to_owned()];
    let harness = ServerHarness::spawn_with_config(config).await;

    let response = reqwest::Client::new()
        .get(format!("{}/v1/status", harness.base_url))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .header("Origin", "https://allowed.example")
        .send()
        .await
        .expect("send");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("access-control-allow-origin")
            .and_then(|value| value.to_str().ok()),
        Some("https://allowed.example")
    );
}

#[tokio::test]
async fn wildcard_origin_accepts_http_and_websocket_without_denial_events() {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let mut config = test_config();
    config.security.http.allowed_origins = vec!["*".to_owned()];
    let harness = ServerHarness::spawn_with_config(config).await;

    let response = reqwest::Client::new()
        .get(format!("{}/v1/status", harness.base_url))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .header("Origin", "https://any.example")
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("access-control-allow-origin")
            .and_then(|value| value.to_str().ok()),
        Some("*")
    );

    let ws_url = harness.base_url.replacen("http://", "ws://", 1) + "/v1/ws";
    let mut request = ws_url.into_client_request().expect("websocket request");
    request.headers_mut().insert(
        "Authorization",
        http::HeaderValue::from_str(&format!("Bearer {SESSION_KEY}")).expect("auth header"),
    );
    request.headers_mut().insert(
        "Origin",
        http::HeaderValue::from_static("https://any.example"),
    );
    let (mut stream, response) = tokio_tungstenite::connect_async(request)
        .await
        .expect("websocket connects");
    assert_eq!(response.status().as_u16(), 101);
    stream.close(None).await.expect("close websocket");

    let logs_response = reqwest::Client::new()
        .get(format!("{}/v1/logs/security", harness.base_url))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("send");
    assert_eq!(logs_response.status(), StatusCode::OK);
    let logs_body: Value = logs_response.json().await.expect("json");
    let events = logs_body["data"]["events"]
        .as_array()
        .expect("events array");
    assert!(
        events
            .iter()
            .all(|event| event["kind"] != "security.cors_origin_denied"
                && event["kind"] != "security.ws_origin_denied"),
        "wildcard origins should not create denial events: {events:?}",
    );
}

#[tokio::test]
async fn unauthenticated_rate_limit_returns_429_envelope_and_security_event() {
    // burst=8 → per-IP cap 8, unauth cap ceil(8/4)=2. Auth'd requests don't
    // tick the unauth bucket, so the test can issue many unauth probes (tied
    // to the per-IP cap of 8 from the same IP) and then still read the audit
    // trail with the session key.
    let mut config = test_config();
    config.security.http.burst = 8;
    config.security.http.rate_limit_per_minute = 60;
    let harness = ServerHarness::spawn_with_config(config).await;
    let status_url = format!("{}/v1/status", harness.base_url);

    let mut limited = false;
    let mut limited_body: Value = Value::Null;
    for _ in 0..6 {
        let response = reqwest::Client::new()
            .get(&status_url)
            .send()
            .await
            .expect("send");
        if response.status() == StatusCode::TOO_MANY_REQUESTS {
            limited_body = response.json().await.expect("json");
            limited = true;
            break;
        }
    }
    assert!(
        limited,
        "must hit 429 within 6 unauth requests at burst=8 (unauth cap=2)"
    );
    assert_eq!(limited_body["ok"], false);
    assert_eq!(limited_body["error"]["code"], "auth.rate_limited");

    // GET /v1/logs/security must surface a security.rate_limited event.
    let logs_response = reqwest::Client::new()
        .get(format!("{}/v1/logs/security", harness.base_url))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("send");
    assert_eq!(logs_response.status(), StatusCode::OK);
    let logs_body: Value = logs_response.json().await.expect("json");
    let events = logs_body["data"]["events"]
        .as_array()
        .expect("events array");
    let rate_limited = events
        .iter()
        .find(|e| e["kind"] == "security.rate_limited")
        .expect("expected security.rate_limited event");
    let payload: Value =
        serde_json::from_str(rate_limited["payload_json"].as_str().expect("payload_json"))
            .expect("payload is JSON");
    // Scope label is `unauthenticated` since the trip happened on a
    // bearer-less request. (per_ip would also be acceptable if the auth'd
    // probe used the same IP and exhausted that bucket first, but with
    // burst=8 we hit unauth first.)
    let scope = payload["scope"].as_str().unwrap_or("");
    assert!(
        scope == "unauthenticated" || scope == "per_ip",
        "unexpected scope: {scope}",
    );
    // The raw bearer must never appear in a security event payload.
    assert!(payload.get("bearer").is_none());
    assert!(payload.get("key").is_none());
}

#[tokio::test]
async fn per_key_rate_limit_returns_429_for_authd_burst() {
    // burst=3 → per-IP cap 3 AND per-key cap 3. Either bucket will trip
    // before 6 requests at 60/min refill. The point of this test is that
    // an authenticated burst is rate-limited (i.e., a valid key cannot
    // bypass the limiter), not which scope fires first. The fingerprint
    // round-trip and "no raw bearer in payload" guarantees are covered by
    // the unit tests in `http_hardening.rs`.
    let mut config = test_config();
    config.security.http.burst = 3;
    config.security.http.rate_limit_per_minute = 60;
    let harness = ServerHarness::spawn_with_config(config).await;
    let status_url = format!("{}/v1/status", harness.base_url);

    let mut limited = false;
    for _ in 0..6 {
        let response = reqwest::Client::new()
            .get(&status_url)
            .header("Authorization", format!("Bearer {SESSION_KEY}"))
            .send()
            .await
            .expect("send");
        if response.status() == StatusCode::TOO_MANY_REQUESTS {
            let body: Value = response.json().await.expect("json");
            assert_eq!(body["ok"], false);
            assert_eq!(body["error"]["code"], "auth.rate_limited");
            limited = true;
            break;
        }
    }
    assert!(limited, "auth'd burst must trip the limiter");
}

#[tokio::test]
async fn rate_limit_envelope_uses_standard_shape() {
    let mut config = test_config();
    config.security.http.burst = 1;
    config.security.http.rate_limit_per_minute = 60;
    let harness = ServerHarness::spawn_with_config(config).await;
    let url = format!("{}/v1/status", harness.base_url);
    let mut last: Option<reqwest::Response> = None;
    for _ in 0..3 {
        let response = reqwest::Client::new().get(&url).send().await.expect("send");
        if response.status() == StatusCode::TOO_MANY_REQUESTS {
            last = Some(response);
            break;
        }
    }
    let response = last.expect("must rate-limit within 3 requests at burst=1");
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["ok"], false);
    assert!(body["error"]["code"].is_string());
    assert!(body["error"]["message"].is_string());
    // `details` must always be present (object), even when empty, per envelope spec.
    assert!(body["error"]["details"].is_object());
}
