#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use acp_stack::api::{self, AppState, RuntimePaths};
use acp_stack::config::{
    AgentAdapterConfig, Config, DependenciesConfig, DependencyEntry, HttpHeaderRef, McpConfig,
    McpHttpServer, McpServerConfig, McpStdioServer, load_config_from_str,
};
use acp_stack::secrets::SecretStore;
use acp_stack::state::{AuthFailureFilter, EventFilter, StateStore};
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
    config_path: PathBuf,
    state_path: PathBuf,
    join: JoinHandle<acp_stack::error::Result<()>>,
    _tempdir: TempDir,
}

impl ServerHarness {
    async fn spawn() -> Self {
        Self::spawn_with_config(test_config()).await
    }

    async fn spawn_with_config(mut config: Config) -> Self {
        let tempdir = tempfile::tempdir().expect("tempdir");
        // Repoint workspace.root at the tempdir so the security-check route's
        // workspace-writability probe (Phase 4: runtime.workspace_not_writable)
        // sees a real, writable directory rather than the fixture's
        // "/workspace" placeholder.
        let workspace_root = tempdir.path().join("workspace");
        std::fs::create_dir_all(&workspace_root).expect("create workspace");
        config.workspace.root = workspace_root.to_string_lossy().into_owned();
        config.workspace.uploads = workspace_root
            .join("uploads")
            .to_string_lossy()
            .into_owned();
        if let Some(user) = acp_stack::ownership::current_username()
            .expect("resolve current username for security fixture")
        {
            config.workspace.runtime_user = user;
        }
        std::fs::create_dir_all(workspace_root.join("uploads")).expect("create uploads");
        Self::spawn_with_prepared_config(config, tempdir).await
    }

    /// Like `spawn_with_config` but does not rewrite `workspace.root`. Use this
    /// when a test deliberately needs the workspace path to come from the
    /// passed-in `Config` — e.g. exercising the "workspace not writable" path
    /// in `/v1/health/ready`.
    async fn spawn_with_unmodified_workspace(config: Config) -> Self {
        let tempdir = tempfile::tempdir().expect("tempdir");
        Self::spawn_with_prepared_config(config, tempdir).await
    }

    async fn spawn_with_prepared_config(config: Config, tempdir: TempDir) -> Self {
        let path = tempdir.path().join("state.sqlite");
        let store = StateStore::open(&path).expect("state open");
        store.migrate().expect("migrate");
        let config_path = create_runtime_files(tempdir.path(), &path);
        std::fs::write(
            &config_path,
            config.to_canonical_toml().expect("canonical test config"),
        )
        .expect("write runtime config");
        let runtime_paths = RuntimePaths::new(config_path.clone(), path.clone());
        let effective_bind = config.api.bind.clone();
        let app_state = AppState::with_effective_bind_and_runtime_paths(
            config,
            store,
            SESSION_KEY.to_owned(),
            ADMIN_KEY.to_owned(),
            effective_bind,
            runtime_paths,
        );
        let state = app_state.state.clone();
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let local = listener.local_addr().expect("local addr");
        let join = tokio::spawn(async move { api::serve(app_state, listener).await });
        Self {
            base_url: format!("http://{local}"),
            state,
            config_path,
            state_path: path,
            join,
            _tempdir: tempdir,
        }
    }

    async fn auth_failure_count(&self) -> usize {
        let guard = self.state.lock().await;
        guard
            .query_auth_failures(AuthFailureFilter {
                limit: 100,
                ..AuthFailureFilter::default()
            })
            .expect("query auth failures")
            .len()
    }

    async fn latest_auth_failure(&self) -> (String, String) {
        let guard = self.state.lock().await;
        let rows = guard
            .query_auth_failures(AuthFailureFilter {
                limit: 1,
                ..AuthFailureFilter::default()
            })
            .expect("query auth failures");
        let row = rows.into_iter().next().expect("at least one auth failure");
        (row.key_kind, row.reason)
    }

    async fn latest_auth_failure_client_ip(&self) -> Option<String> {
        let guard = self.state.lock().await;
        let rows = guard
            .query_auth_failures(AuthFailureFilter {
                limit: 1,
                ..AuthFailureFilter::default()
            })
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

fn create_runtime_files(root: &Path, state_path: &Path) -> PathBuf {
    let config_dir = root.join(".config/acp-stack");
    let state_dir = state_path.parent().expect("state parent").to_path_buf();
    std::fs::create_dir_all(&config_dir).expect("create config dir");
    std::fs::create_dir_all(&state_dir).expect("create state dir");

    let config_path = config_dir.join("acps-config.toml");
    let age_key_path = config_dir.join("age.key");
    let secret_store_path = state_dir.join("secrets.age");
    std::fs::write(&config_path, "test config").expect("write config file");
    SecretStore::open_or_create_at_paths(&age_key_path, &secret_store_path)
        .expect("create secret store");

    #[cfg(unix)]
    {
        std::fs::set_permissions(&config_dir, std::fs::Permissions::from_mode(0o700))
            .expect("chmod config dir");
        std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o700))
            .expect("chmod state dir");
        for file in [&config_path, &age_key_path, state_path, &secret_store_path] {
            std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o600))
                .expect("chmod runtime file");
        }
    }

    config_path
}

fn test_config() -> Config {
    let toml_text = include_str!("fixtures/valid-placebo-stack.toml");
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
async fn config_export_reads_current_runtime_config_file() {
    let harness = ServerHarness::spawn().await;
    let current = std::fs::read_to_string(&harness.config_path).expect("read config");
    let updated = current.replace(
        r#"public_url = "https://agent.example.com""#,
        r#"public_url = "https://updated.example.com""#,
    );
    std::fs::write(&harness.config_path, updated).expect("write updated config");

    let response = reqwest::Client::new()
        .get(format!("{}/v1/config/export", harness.base_url))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    let toml = body["data"]["toml"].as_str().expect("toml string");
    assert!(toml.contains(r#"public_url = "https://updated.example.com""#));
}

#[tokio::test]
async fn config_validate_accepts_valid_toml() {
    let harness = ServerHarness::spawn().await;
    let toml = include_str!("fixtures/valid-placebo-stack.toml");
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
            headers: vec![HttpHeaderRef {
                name: "Authorization".to_owned(),
                value_ref: "LINEAR_API_KEY".to_owned(),
            }],
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
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let local = listener.local_addr().expect("local addr");
    let join = tokio::spawn(async move { api::serve(app_state, listener).await });
    let harness = ServerHarness {
        base_url: format!("http://{local}"),
        state,
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

#[tokio::test]
async fn config_import_dry_run_returns_metadata() {
    let harness = ServerHarness::spawn().await;
    let original_config = std::fs::read_to_string(&harness.config_path).expect("read config");
    let toml = include_str!("fixtures/valid-placebo-stack.toml");
    let response = reqwest::Client::new()
        .post(format!(
            "{}/v1/config/import?dry_run=true",
            harness.base_url
        ))
        .header("Authorization", format!("Bearer {ADMIN_KEY}"))
        .body(toml.to_owned())
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["ok"], Value::Bool(true));
    assert_eq!(body["data"]["dry_run"], Value::Bool(true));
    assert_eq!(body["data"]["config_version"], Value::Number(1.into()));
    assert!(body["data"]["canonical_toml_size"].is_number());
    assert!(body["data"]["input_size"].is_number());
    assert!(body["data"]["auth_refs_unchanged"].is_boolean());
    assert!(body["data"]["target"].is_string());
    assert!(body["data"]["target_exists"].is_boolean());

    let current_config = std::fs::read_to_string(&harness.config_path).expect("read config");
    assert_eq!(current_config, original_config);
    let guard = harness.state.lock().await;
    let events = guard
        .query_events(EventFilter {
            limit: 10,
            kind: Some("server.config_imported"),
            ..EventFilter::default()
        })
        .expect("query events");
    assert!(events.is_empty(), "dry-run must not audit config import");
}

#[tokio::test]
async fn config_import_dry_run_reports_auth_ref_mismatch_without_mutation() {
    let harness = ServerHarness::spawn().await;
    let original_config = std::fs::read_to_string(&harness.config_path).expect("read config");
    let toml = include_str!("fixtures/valid-placebo-stack.toml").replace(
        r#"admin_key_ref = "ACP_STACK_ADMIN_KEY""#,
        r#"admin_key_ref = "ROTATED_ADMIN_KEY""#,
    );
    let response = reqwest::Client::new()
        .post(format!(
            "{}/v1/config/import?dry_run=true",
            harness.base_url
        ))
        .header("Authorization", format!("Bearer {ADMIN_KEY}"))
        .body(toml)
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["ok"], Value::Bool(true));
    assert_eq!(body["data"]["auth_refs_unchanged"], Value::Bool(false));

    let current_config = std::fs::read_to_string(&harness.config_path).expect("read config");
    assert_eq!(current_config, original_config);
    let guard = harness.state.lock().await;
    let events = guard
        .query_events(EventFilter {
            limit: 10,
            kind: Some("server.config_imported"),
            ..EventFilter::default()
        })
        .expect("query events");
    assert!(events.is_empty(), "dry-run must not audit config import");
}

#[tokio::test]
async fn config_import_rejects_auth_ref_mismatch_before_write() {
    let harness = ServerHarness::spawn().await;
    let original_config = std::fs::read_to_string(&harness.config_path).expect("read config");
    let toml = include_str!("fixtures/valid-placebo-stack.toml").replace(
        r#"admin_key_ref = "ACP_STACK_ADMIN_KEY""#,
        r#"admin_key_ref = "ROTATED_ADMIN_KEY""#,
    );
    let response = reqwest::Client::new()
        .post(format!("{}/v1/config/import", harness.base_url))
        .header("Authorization", format!("Bearer {ADMIN_KEY}"))
        .body(toml)
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["ok"], Value::Bool(false));
    assert_eq!(body["error"]["code"], "config.import_changes_auth_ref");

    let current_config = std::fs::read_to_string(&harness.config_path).expect("read config");
    assert_eq!(current_config, original_config);
    let guard = harness.state.lock().await;
    let events = guard
        .query_events(EventFilter {
            limit: 10,
            kind: Some("server.config_imported"),
            ..EventFilter::default()
        })
        .expect("query events");
    assert!(
        events.is_empty(),
        "rejected import must not audit config import"
    );
}

#[tokio::test]
async fn config_import_oversized_body_returns_413() {
    let harness = ServerHarness::spawn().await;
    let body = "x".repeat(2 * 1024 * 1024); // 2 MiB
    let response = reqwest::Client::new()
        .post(format!("{}/v1/config/import", harness.base_url))
        .header("Authorization", format!("Bearer {ADMIN_KEY}"))
        .body(body)
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["ok"], Value::Bool(false));
    assert_eq!(body["error"]["code"], "import.too_large");
}

#[tokio::test]
async fn config_validate_secret_ref_value_error_does_not_echo_secret() {
    let harness = ServerHarness::spawn().await;
    let secret = "sk-proj-inline-secret-value";
    let toml = include_str!("fixtures/valid-placebo-stack.toml")
        .replace("env = []", &format!(r#"env = ["{secret}"]"#));
    let response = reqwest::Client::new()
        .post(format!("{}/v1/config/validate", harness.base_url))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .body(toml)
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["ok"], Value::Bool(false));
    assert_eq!(body["error"]["code"], "config.invalid");
    assert!(
        !body["error"]["message"]
            .as_str()
            .expect("message")
            .contains(secret)
    );
}
