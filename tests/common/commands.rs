//! Shared fixtures for the `commands_*_tests` binaries.
//!
//! `tests/common/api.rs` and `tests/common/sessions.rs` define same-named items
//! with different key values and signatures. The three sets are deliberately
//! separate — do not merge or cross-import them.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use acp_stack::api::{self, AppState};
use acp_stack::config::{CommandsConfig, Config, PermissionsConfig, load_config_from_str};
use acp_stack::state::StateStore;
use reqwest::StatusCode;
use serde_json::Value;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::Mutex as TokioMutex;
use tokio::task::JoinHandle;

pub const SESSION_KEY: &str = "acps_session_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
pub const ADMIN_KEY: &str = "acps_admin_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

pub struct Harness {
    pub base_url: String,
    pub workspace_root: PathBuf,
    pub _state: Arc<TokioMutex<StateStore>>,
    pub join: JoinHandle<acp_stack::error::Result<()>>,
    pub _workspace_tempdir: TempDir,
    pub _state_tempdir: TempDir,
}

pub struct HarnessOverrides {
    pub permissions: Option<PermissionsConfig>,
    pub commands: Option<CommandsConfig>,
}

impl HarnessOverrides {
    pub fn none() -> Self {
        Self {
            permissions: None,
            commands: None,
        }
    }
}

impl Harness {
    pub async fn spawn() -> Self {
        Self::spawn_with(HarnessOverrides::none()).await
    }

    pub async fn spawn_with(overrides: HarnessOverrides) -> Self {
        let workspace_tempdir = tempfile::tempdir().expect("workspace tempdir");
        let workspace_root = workspace_tempdir.path().to_path_buf();
        let uploads_root = workspace_root.join("uploads");
        std::fs::create_dir(&uploads_root).expect("uploads dir");

        let mut config = test_config();
        config.workspace.root = workspace_root.to_string_lossy().into_owned();
        config.workspace.uploads = uploads_root.to_string_lossy().into_owned();
        // /bin/sh is available on every Unix CI box; /bin/bash is not.
        config.workspace.default_shell = "/bin/sh".to_owned();
        if let Some(permissions) = overrides.permissions {
            config.permissions = permissions;
        }
        if let Some(commands) = overrides.commands {
            config.commands = commands;
        }

        let state_tempdir = tempfile::tempdir().expect("state tempdir");
        let state_path = state_tempdir.path().join("state.sqlite");
        let store = StateStore::open(&state_path).expect("state open");
        store.migrate().expect("migrate");

        let app_state = AppState::new(config, store, SESSION_KEY.to_owned(), ADMIN_KEY.to_owned());
        let state = app_state.state.clone();
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let local = listener.local_addr().expect("local addr");
        let join = tokio::spawn(async move { api::serve(app_state, listener).await });

        Self {
            base_url: format!("http://{local}"),
            workspace_root,
            _state: state,
            join,
            _workspace_tempdir: workspace_tempdir,
            _state_tempdir: state_tempdir,
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
    load_config_from_str(toml_text).expect("config parses")
}

pub fn session_client() -> reqwest::Client {
    reqwest::Client::new()
}

pub fn auth(req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    req.header(
        reqwest::header::AUTHORIZATION,
        format!("Bearer {SESSION_KEY}"),
    )
}

pub fn admin_auth(req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    req.header(
        reqwest::header::AUTHORIZATION,
        format!("Bearer {ADMIN_KEY}"),
    )
}

pub async fn submit(harness: &Harness, body: Value) -> reqwest::Response {
    auth(session_client().post(format!("{}/v1/commands", harness.base_url)))
        .json(&body)
        .send()
        .await
        .expect("send")
}

pub async fn pending_permission_for_command(harness: &Harness, command_id: &str) -> Value {
    let pending =
        auth(session_client().get(format!("{}/v1/permissions/pending", harness.base_url)))
            .send()
            .await
            .expect("send");
    assert_eq!(pending.status(), StatusCode::OK);
    let pending_body: Value = pending.json().await.expect("json");
    pending_body["data"]["permissions"]
        .as_array()
        .expect("permissions array")
        .iter()
        .find(|permission| permission["subject_id"].as_str() == Some(command_id))
        .expect("pending permission row")
        .clone()
}

pub async fn approve_pending_command(harness: &Harness, command_id: &str) {
    let permission = pending_permission_for_command(harness, command_id).await;
    let permission_id = permission["id"].as_str().expect("permission id");
    let approve_response = auth(session_client().post(format!(
        "{}/v1/permissions/{permission_id}/approve",
        harness.base_url
    )))
    .json(&serde_json::json!({}))
    .send()
    .await
    .expect("send");
    assert_eq!(approve_response.status(), StatusCode::OK);
}

/// Drive `GET /v1/commands/{id}` until the row reaches a terminal status,
/// bounded so a regression is a deterministic timeout rather than a hung test.
pub async fn wait_for_terminal(harness: &Harness, id: &str) -> Value {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let response =
            auth(session_client().get(format!("{}/v1/commands/{}", harness.base_url, id)))
                .send()
                .await
                .expect("send");
        if response.status() == StatusCode::TOO_MANY_REQUESTS {
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        }
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value = response.json().await.expect("json");
        let status = body["data"]["status"].as_str().unwrap_or("");
        if status != "pending" && status != "running" {
            return body;
        }
        if std::time::Instant::now() >= deadline {
            panic!("command did not finish in time: {body}");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

// ----- WebSocket -----------------------------------------------------------

pub async fn open_ws(
    base_url: &str,
    topics: &[&str],
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    use futures::SinkExt;
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let ws_url = base_url.replacen("http://", "ws://", 1) + "/v1/ws";
    let mut request = ws_url.as_str().into_client_request().expect("ws request");
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {SESSION_KEY}").parse().expect("header"),
    );

    let (mut stream, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("ws connect");

    let subscribe = serde_json::json!({"type": "subscribe", "topics": topics});
    stream
        .send(Message::Text(subscribe.to_string().into()))
        .await
        .expect("subscribe");
    stream
}

pub async fn collect_until<S>(stream: &mut S, predicate: impl Fn(&Value) -> bool) -> Vec<Value>
where
    S: futures::Stream<
            Item = std::result::Result<
                tokio_tungstenite::tungstenite::Message,
                tokio_tungstenite::tungstenite::Error,
            >,
        > + Unpin,
{
    use futures::StreamExt;
    let mut out = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        let next = tokio::time::timeout(Duration::from_millis(500), stream.next()).await;
        let Ok(Some(Ok(message))) = next else {
            continue;
        };
        let text = match message {
            tokio_tungstenite::tungstenite::Message::Text(t) => t,
            _ => continue,
        };
        let value: Value = serde_json::from_str(text.as_str()).expect("ws json");
        let matched = predicate(&value);
        out.push(value);
        if matched {
            return out;
        }
    }
    out
}
