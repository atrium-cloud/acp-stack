//! Shared fixtures for the `sessions_*_tests` binaries: an in-process server
//! harness wired to the placebo ACP agent, plus the auth/websocket/state
//! helpers its tests assert against.
//!
//! `tests/common/api.rs` defines same-named items (`SESSION_KEY`, `ADMIN_KEY`,
//! `test_config`, ...) with different key values and different signatures.
//! The two sets are deliberately separate — do not merge or cross-import them.

use std::sync::Arc;
use std::time::Duration;

use acp_stack::api::{self, AppState, RuntimePaths};
use acp_stack::config::{Config, load_config_from_str};
use acp_stack::state::StateStore;
use futures::StreamExt;
use reqwest::StatusCode;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::Mutex as TokioMutex;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

pub const SESSION_KEY: &str = "acps_session_cccccccccccccccccccccccccccccccccccccccccccc";
pub const ADMIN_KEY: &str = "acps_admin_dddddddddddddddddddddddddddddddddddddddddddd";

pub struct Harness {
    pub base_url: String,
    pub config_path: std::path::PathBuf,
    pub workspace_root: std::path::PathBuf,
    pub _tempdir: TempDir,
    pub state: Arc<TokioMutex<StateStore>>,
    pub join: JoinHandle<acp_stack::error::Result<()>>,
}

impl Harness {
    pub async fn spawn() -> Self {
        Self::spawn_with(|_| {}).await
    }

    pub async fn spawn_with(mutate: impl FnOnce(&mut Config)) -> Self {
        Self::spawn_inner(mutate, None, true).await
    }

    /// Harness that never calls `POST /v1/agent/start`, mirroring a freshly
    /// initialized host where the process manager owns `acps serve` only.
    pub async fn spawn_without_agent_start(mutate: impl FnOnce(&mut Config)) -> Self {
        Self::spawn_inner(mutate, None, false).await
    }

    pub async fn spawn_with_models_cache(mutate: impl FnOnce(&mut Config), models: Value) -> Self {
        Self::spawn_inner(mutate, Some(models), true).await
    }

    async fn spawn_inner(
        mutate: impl FnOnce(&mut Config),
        models: Option<Value>,
        start_agent: bool,
    ) -> Self {
        let tempdir = TempDir::new().expect("tempdir");
        let path = tempdir.path().join("state.sqlite");
        let store = StateStore::open(&path).expect("state open");
        store.migrate().expect("migrate");
        if let Some(models) = models {
            write_models_dev_cache(tempdir.path(), models);
        }
        let mut config = test_config();
        let workspace_root = tempdir.path().join("workspace");
        let uploads_root = workspace_root.join("uploads");
        std::fs::create_dir_all(&uploads_root).expect("workspace dirs");
        config.workspace.root = workspace_root.to_string_lossy().into_owned();
        config.workspace.uploads = uploads_root.to_string_lossy().into_owned();
        mutate(&mut config);
        if !config.agent.args.iter().any(|arg| arg == "--listed-cwd") {
            config.agent.args.extend([
                "--listed-cwd".to_owned(),
                workspace_root.to_string_lossy().into_owned(),
            ]);
        }
        let config_path = tempdir.path().join("acps-config.toml");
        std::fs::write(
            &config_path,
            config.to_canonical_toml().expect("canonical test config"),
        )
        .expect("test config write");
        let effective_bind = config.api.bind.clone();
        let runtime_paths = RuntimePaths::new(config_path.clone(), path);
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
        let base_url = format!("http://{}", listener.local_addr().expect("local"));
        let join = tokio::spawn(async move { api::serve(app_state, listener).await });
        let harness = Self {
            base_url,
            config_path,
            workspace_root,
            _tempdir: tempdir,
            state,
            join,
        };
        if start_agent {
            harness.start_agent().await;
        }
        harness
    }

    pub async fn agent_process_state(&self) -> String {
        let body: Value = http()
            .get(format!("{}/v1/status/agent", self.base_url))
            .header("Authorization", session_bearer())
            .send()
            .await
            .expect("agent status")
            .json()
            .await
            .expect("agent status json");
        body["data"]["process_state"]
            .as_str()
            .unwrap_or_else(|| panic!("process_state present in {body}"))
            .to_owned()
    }

    pub async fn stop_agent(&self) {
        let response = http()
            .post(format!("{}/v1/agent/stop", self.base_url))
            .header("Authorization", admin_bearer())
            .send()
            .await
            .expect("stop request");
        assert_eq!(response.status(), StatusCode::OK, "agent stop failed");
    }

    async fn start_agent(&self) {
        let client = http();
        let response = client
            .post(format!("{}/v1/agent/start", self.base_url))
            .header("Authorization", admin_bearer())
            .send()
            .await
            .expect("start request");
        if response.status() != StatusCode::OK {
            let body = response.text().await.unwrap_or_default();
            panic!("agent start failed: {body}");
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.join.abort();
    }
}

pub fn test_config() -> Config {
    let toml_text = include_str!("../fixtures/valid-placebo-stack.toml");
    let mut config = load_config_from_str(toml_text).expect("config parses");
    config.agent.command = env!("CARGO_BIN_EXE_placebo-agent").to_owned();
    config.agent.args = vec!["acp".into()];
    config.agent.env = vec![];
    config.agent.cwd = Some(std::env::temp_dir().to_string_lossy().into_owned());
    config.agent.expected_sha256 = None;
    config
}

pub fn http() -> reqwest::Client {
    reqwest::Client::builder().build().expect("client")
}

pub fn write_models_dev_cache(root: &std::path::Path, models: Value) {
    let payload = json!({
        "version": 1,
        "source_url": "https://models.dev/models.json",
        "fetched_at": 9_999_999_999u64,
        "last_failed_refresh_attempt_at": null,
        "models": models,
    });
    std::fs::write(
        root.join("models-dev-models.json"),
        serde_json::to_vec_pretty(&payload).expect("cache json"),
    )
    .expect("write models.dev cache");
}

pub fn admin_bearer() -> String {
    format!("Bearer {ADMIN_KEY}")
}

pub fn session_bearer() -> String {
    format!("Bearer {SESSION_KEY}")
}

pub fn websocket_request(harness: &Harness, bearer: String) -> http::Request<()> {
    let mut request = websocket_url(harness)
        .into_client_request()
        .expect("websocket request");
    request.headers_mut().insert(
        "Authorization",
        http::HeaderValue::from_str(&bearer).expect("bearer header"),
    );
    request
}

pub fn websocket_url(harness: &Harness) -> String {
    format!(
        "{}/v1/ws",
        harness
            .base_url
            .strip_prefix("http://")
            .map(|rest| format!("ws://{rest}"))
            .unwrap_or_else(|| harness.base_url.replace("http", "ws"))
    )
}

pub async fn create_session(harness: &Harness) -> String {
    let client = http();
    let response = client
        .post(format!("{}/v1/sessions", harness.base_url))
        .header("Authorization", session_bearer())
        .header("Content-Type", "application/json")
        .body("{}")
        .send()
        .await
        .expect("create");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("create json");
    body["data"]["id"]
        .as_str()
        .expect("session id present")
        .to_owned()
}

pub async fn prompt_count_for_session(harness: &Harness, session_id: &str) -> i64 {
    let state_path = {
        let state = harness.state.lock().await;
        state.path().to_path_buf()
    };
    let connection = rusqlite::Connection::open(state_path).expect("open state db");
    connection
        .query_row(
            "SELECT COUNT(*) FROM prompts WHERE session_id = ?1",
            [session_id],
            |row| row.get(0),
        )
        .expect("prompt count")
}

pub async fn recv_matching_event(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    expected_topic: &str,
    expected_kind: &str,
) -> Option<Value> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        let message = tokio::time::timeout(Duration::from_secs(1), ws.next())
            .await
            .expect("ws message before timeout")?;
        let message = message.expect("ws message ok");
        let tokio_tungstenite::tungstenite::Message::Text(text) = message else {
            continue;
        };
        let event: Value = serde_json::from_str(&text).expect("event json");
        if event["type"] == "event"
            && event["topic"] == expected_topic
            && event["payload"]["kind"] == expected_kind
        {
            return Some(event);
        }
    }
    None
}
