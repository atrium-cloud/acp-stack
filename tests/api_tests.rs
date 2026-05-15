use std::path::{Path, PathBuf};
use std::sync::Arc;

use acp_stack::api::{self, AppState};
use acp_stack::config::{AgentAdapterConfig, Config, load_config_from_str};
use acp_stack::state::{AuthFailureFilter, StateStore};
use reqwest::StatusCode;
use rusqlite::Connection;
use serde_json::Value;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::Mutex as TokioMutex;
use tokio::task::JoinHandle;

const SESSION_KEY: &str = "acps_session_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const ADMIN_KEY: &str = "acps_admin_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

struct ServerHarness {
    base_url: String,
    state: Arc<TokioMutex<StateStore>>,
    state_path: PathBuf,
    join: JoinHandle<acp_stack::error::Result<()>>,
    _tempdir: TempDir,
}

impl ServerHarness {
    async fn spawn() -> Self {
        Self::spawn_with_config(test_config()).await
    }

    async fn spawn_with_config(config: Config) -> Self {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = tempdir.path().join("state.sqlite");
        let store = StateStore::open(&path).expect("state open");
        store.migrate().expect("migrate");
        let app_state = AppState::new(config, store, SESSION_KEY.to_owned(), ADMIN_KEY.to_owned());
        let state = app_state.state.clone();
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let local = listener.local_addr().expect("local addr");
        let join = tokio::spawn(async move { api::serve(app_state, listener).await });
        Self {
            base_url: format!("http://{local}"),
            state,
            state_path: path,
            join,
            _tempdir: tempdir,
        }
    }

    async fn auth_failure_count(&self) -> usize {
        let guard = self.state.lock().await;
        guard
            .query_auth_failures(AuthFailureFilter { limit: 100 })
            .expect("query auth failures")
            .len()
    }

    async fn latest_auth_failure(&self) -> (String, String) {
        let guard = self.state.lock().await;
        let rows = guard
            .query_auth_failures(AuthFailureFilter { limit: 1 })
            .expect("query auth failures");
        let row = rows.into_iter().next().expect("at least one auth failure");
        (row.key_kind, row.reason)
    }

    async fn latest_auth_failure_client_ip(&self) -> Option<String> {
        let guard = self.state.lock().await;
        let rows = guard
            .query_auth_failures(AuthFailureFilter { limit: 1 })
            .expect("query auth failures");
        rows.into_iter()
            .next()
            .expect("at least one auth failure")
            .client_ip
    }
}

impl Drop for ServerHarness {
    fn drop(&mut self) {
        self.join.abort();
    }
}

fn test_config() -> Config {
    let toml_text = include_str!("fixtures/valid-acp-stack.toml");
    load_config_from_str(toml_text).expect("config parses")
}

fn codex_adapter() -> AgentAdapterConfig {
    AgentAdapterConfig {
        id: "codex-acp".to_owned(),
        name: "Codex ACP Adapter".to_owned(),
        upstream_agent: "codex-cli".to_owned(),
        source_url: Some("https://github.com/zed-industries/codex-acp".to_owned()),
    }
}

fn seed_session(path: &Path, id: &str, status: &str, created_at: &str, updated_at: &str) {
    let connection = Connection::open(path).expect("open sqlite for seed");
    connection
        .execute(
            r#"
            INSERT INTO sessions (id, created_at, updated_at, status)
            VALUES (?1, ?2, ?3, ?4)
            "#,
            (id, created_at, updated_at, status),
        )
        .expect("insert session");
}

fn seed_command(
    path: &Path,
    id: &str,
    status: &str,
    command: &str,
    exit_status: Option<i64>,
    created_at: &str,
    updated_at: &str,
) {
    let connection = Connection::open(path).expect("open sqlite for seed");
    connection
        .execute(
            r#"
            INSERT INTO commands (id, created_at, updated_at, status, command, exit_status)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            "#,
            (id, created_at, updated_at, status, command, exit_status),
        )
        .expect("insert command");
}

fn seed_auth_failure(path: &Path, id: &str, created_at: &str, reason: &str) {
    let connection = Connection::open(path).expect("open sqlite for seed");
    connection
        .execute(
            r#"
            INSERT INTO auth_failures
                (id, created_at, key_kind, reason, client_ip, route, payload_json)
            VALUES (?1, ?2, 'unknown', ?3, NULL, '/v1/status', '{}')
            "#,
            (id, created_at, reason),
        )
        .expect("insert auth failure");
}

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
async fn config_export_returns_canonical_toml() {
    let harness = ServerHarness::spawn().await;
    let response = reqwest::Client::new()
        .get(format!("{}/v1/config/export", harness.base_url))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    let toml = body["data"]["toml"].as_str().expect("toml string");
    assert!(toml.contains("[api]"));
    assert!(toml.contains("bind ="));
}

#[tokio::test]
async fn config_validate_accepts_valid_toml() {
    let harness = ServerHarness::spawn().await;
    let toml = include_str!("fixtures/valid-acp-stack.toml");
    let response = reqwest::Client::new()
        .post(format!("{}/v1/config/validate", harness.base_url))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .body(toml.to_owned())
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["data"]["valid"], Value::Bool(true));
}

#[tokio::test]
async fn config_validate_rejects_garbage_with_400() {
    let harness = ServerHarness::spawn().await;
    let response = reqwest::Client::new()
        .post(format!("{}/v1/config/validate", harness.base_url))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .body("this is not toml at all")
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["ok"], Value::Bool(false));
    // ConfigToml errors carry the dotted code "config.invalid".
    assert_eq!(body["error"]["code"], "config.invalid");
}

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
    assert_eq!(events[0]["kind"], "test.kind");
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
    assert_eq!(body["data"]["agent"]["id"], "opencode");
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
        "https://github.com/zed-industries/codex-acp"
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

    let response = reqwest::Client::new()
        .get(format!("{}/v1/metrics/summary", harness.base_url))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["data"]["sessions"], Value::Number(1.into()));
    assert_eq!(body["data"]["commands"], Value::Number(1.into()));
    assert_eq!(body["data"]["auth_failures"], Value::Number(1.into()));
    assert_eq!(body["data"]["agent_lifecycle"], Value::Number(1.into()));
    assert_eq!(body["data"]["events"], Value::Number(1.into()));
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
    assert!(
        body["data"]["findings"]
            .as_array()
            .expect("findings")
            .is_empty()
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
    let app_state = AppState::with_effective_bind(
        config,
        store,
        SESSION_KEY.to_owned(),
        ADMIN_KEY.to_owned(),
        "0.0.0.0:7700".to_owned(),
    );
    let state = app_state.state.clone();
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let local = listener.local_addr().expect("local addr");
    let join = tokio::spawn(async move { api::serve(app_state, listener).await });
    let harness = ServerHarness {
        base_url: format!("http://{local}"),
        state,
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
