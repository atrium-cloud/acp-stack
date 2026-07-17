#![cfg(feature = "test-fixtures")]

//! End-to-end coverage for the session HTTP routes: create, list, get,
//! prompt (fire-and-forget + polling), cancel, load, close, plus auth-tier
//! enforcement and `session/update` persistence.
//!
//! The placebo ACP fixture stands in for a real ACP agent;
//! `tests/acp_bridge_tests.rs` exercises the lower-level bridge layer.

use std::sync::Arc;
use std::time::Duration;

use acp_stack::api::{self, AppState, RuntimePaths};
use acp_stack::config::{ArrayTargetConfig, Config, load_config_from_str};
use acp_stack::state::{
    NewPermissionRequest, NewPromptRecord, NewSessionRecord, PromptStatus, StateStore,
};
use futures::{SinkExt, StreamExt};
use reqwest::StatusCode;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::Mutex as TokioMutex;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const SESSION_KEY: &str = "acps_session_cccccccccccccccccccccccccccccccccccccccccccc";
const ADMIN_KEY: &str = "acps_admin_dddddddddddddddddddddddddddddddddddddddddddd";

struct Harness {
    base_url: String,
    config_path: std::path::PathBuf,
    workspace_root: std::path::PathBuf,
    _tempdir: TempDir,
    state: Arc<TokioMutex<StateStore>>,
    join: JoinHandle<acp_stack::error::Result<()>>,
}

impl Harness {
    async fn spawn() -> Self {
        Self::spawn_with(|_| {}).await
    }

    async fn spawn_with(mutate: impl FnOnce(&mut Config)) -> Self {
        Self::spawn_inner(mutate, None).await
    }

    async fn spawn_with_models_cache(mutate: impl FnOnce(&mut Config), models: Value) -> Self {
        Self::spawn_inner(mutate, Some(models)).await
    }

    async fn spawn_inner(mutate: impl FnOnce(&mut Config), models: Option<Value>) -> Self {
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
        harness.start_agent().await;
        harness
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

fn test_config() -> Config {
    let toml_text = include_str!("fixtures/valid-placebo-stack.toml");
    let mut config = load_config_from_str(toml_text).expect("config parses");
    config.agent.command = env!("CARGO_BIN_EXE_placebo-agent").to_owned();
    config.agent.args = vec!["acp".into()];
    config.agent.env = vec![];
    config.agent.cwd = Some(std::env::temp_dir().to_string_lossy().into_owned());
    config.agent.expected_sha256 = None;
    config
}

fn http() -> reqwest::Client {
    reqwest::Client::builder().build().expect("client")
}

fn write_models_dev_cache(root: &std::path::Path, models: Value) {
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

fn admin_bearer() -> String {
    format!("Bearer {ADMIN_KEY}")
}

fn session_bearer() -> String {
    format!("Bearer {SESSION_KEY}")
}

fn websocket_request(harness: &Harness, bearer: String) -> http::Request<()> {
    let mut request = websocket_url(harness)
        .into_client_request()
        .expect("websocket request");
    request.headers_mut().insert(
        "Authorization",
        http::HeaderValue::from_str(&bearer).expect("bearer header"),
    );
    request
}

fn websocket_url(harness: &Harness) -> String {
    format!(
        "{}/v1/ws",
        harness
            .base_url
            .strip_prefix("http://")
            .map(|rest| format!("ws://{rest}"))
            .unwrap_or_else(|| harness.base_url.replace("http", "ws"))
    )
}

async fn create_session(harness: &Harness) -> String {
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

async fn prompt_count_for_session(harness: &Harness, session_id: &str) -> i64 {
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

#[tokio::test]
async fn create_session_accepts_existing_cwd_under_workspace() {
    let harness = Harness::spawn().await;
    let inner = harness.workspace_root.join("inner");
    std::fs::create_dir(&inner).expect("inner dir");
    let response = http()
        .post(format!("{}/v1/sessions", harness.base_url))
        .header("Authorization", session_bearer())
        .json(&json!({ "cwd": inner.to_string_lossy() }))
        .send()
        .await
        .expect("create");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    let canonical_inner = inner.canonicalize().expect("canonical inner");
    assert_eq!(
        body["data"]["cwd"],
        canonical_inner.to_string_lossy().as_ref()
    );
}

#[tokio::test]
async fn create_session_rejects_symlink_cwd_escape() {
    let harness = Harness::spawn().await;
    let outside = tempfile::tempdir().expect("outside");
    let link = harness.workspace_root.join("outside-link");
    std::os::unix::fs::symlink(outside.path(), &link).expect("symlink");
    let response = http()
        .post(format!("{}/v1/sessions", harness.base_url))
        .header("Authorization", session_bearer())
        .json(&json!({ "cwd": link.to_string_lossy() }))
        .send()
        .await
        .expect("create");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "prompt.body_invalid");
}

#[tokio::test]
async fn create_session_applies_model_with_custom_config_option_id() {
    let harness = Harness::spawn_with(|config| {
        config.agent.args.extend([
            "--model-config-option".to_owned(),
            "deepseek/deepseek-v4-flash".to_owned(),
            "--model-config-option-id".to_owned(),
            "agent-model".to_owned(),
            "--expect-model-config".to_owned(),
            "deepseek/deepseek-v4-flash".to_owned(),
        ]);
        config.agent.model = Some("deepseek/deepseek-v4-flash".to_owned());
    })
    .await;
    let session_id = create_session(&harness).await;
    let response = http()
        .post(format!(
            "{}/v1/sessions/{}/prompt",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .json(&json!({ "prompt": "model should already be set" }))
        .send()
        .await
        .expect("prompt");
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn prompt_gate_allows_text_prompt_for_known_text_model() {
    let model_id = "provider/text-only";
    let harness = Harness::spawn_with_models_cache(
        |config| {
            config.agent.model = Some(model_id.to_owned());
            config
                .agent
                .args
                .extend(["--model-config-option".to_owned(), model_id.to_owned()]);
        },
        json!({
            model_id: {
                "id": model_id,
                "modalities": { "input": ["text"] }
            }
        }),
    )
    .await;
    let session_id = create_session(&harness).await;

    let response = http()
        .post(format!(
            "{}/v1/sessions/{}/prompt",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .json(&json!({ "prompt": "text is fine" }))
        .send()
        .await
        .expect("prompt");

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn prompt_gate_rejects_image_for_known_text_model_without_prompt_row() {
    let model_id = "provider/text-only";
    let harness = Harness::spawn_with_models_cache(
        |config| {
            config.agent.model = Some(model_id.to_owned());
            config
                .agent
                .args
                .extend(["--model-config-option".to_owned(), model_id.to_owned()]);
        },
        json!({
            model_id: {
                "id": model_id,
                "modalities": { "input": ["text"] }
            }
        }),
    )
    .await;
    let session_id = create_session(&harness).await;

    let response = http()
        .post(format!(
            "{}/v1/sessions/{}/prompt",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .json(&json!({
            "prompt": [{
                "type": "image",
                "data": "aW1hZ2U=",
                "mimeType": "image/png"
            }]
        }))
        .send()
        .await
        .expect("prompt");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "prompt.unsupported_modality");
    assert_eq!(prompt_count_for_session(&harness, &session_id).await, 0);
}

#[tokio::test]
async fn prompt_gate_rejects_video_blob_for_known_text_model() {
    let model_id = "provider/text-only";
    let harness = Harness::spawn_with_models_cache(
        |config| {
            config.agent.model = Some(model_id.to_owned());
            config
                .agent
                .args
                .extend(["--model-config-option".to_owned(), model_id.to_owned()]);
        },
        json!({
            model_id: {
                "id": model_id,
                "modalities": { "input": ["text"] }
            }
        }),
    )
    .await;
    let session_id = create_session(&harness).await;

    let response = http()
        .post(format!(
            "{}/v1/sessions/{}/prompt",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .json(&json!({
            "prompt": [{
                "type": "resource",
                "resource": {
                    "blob": "dmlkZW8=",
                    "uri": "file:///clip.mp4",
                    "mimeType": "video/mp4"
                }
            }]
        }))
        .send()
        .await
        .expect("prompt");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "prompt.unsupported_modality");
}

#[tokio::test]
async fn prompt_gate_allows_pdf_blob_for_known_text_model() {
    let model_id = "provider/text-only";
    let harness = Harness::spawn_with_models_cache(
        |config| {
            config.agent.model = Some(model_id.to_owned());
            config
                .agent
                .args
                .extend(["--model-config-option".to_owned(), model_id.to_owned()]);
        },
        json!({
            model_id: {
                "id": model_id,
                "modalities": { "input": ["text"] }
            }
        }),
    )
    .await;
    let session_id = create_session(&harness).await;

    let response = http()
        .post(format!(
            "{}/v1/sessions/{}/prompt",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .json(&json!({
            "prompt": [{
                "type": "resource",
                "resource": {
                    "blob": "cGRm",
                    "uri": "file:///doc.pdf",
                    "mimeType": "application/pdf"
                }
            }]
        }))
        .send()
        .await
        .expect("prompt");

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn prompt_gate_allows_image_for_unknown_model() {
    let model_id = "provider/unlisted";
    let harness = Harness::spawn_with_models_cache(
        |config| {
            config.agent.model = Some(model_id.to_owned());
            config
                .agent
                .args
                .extend(["--model-config-option".to_owned(), model_id.to_owned()]);
        },
        json!({
            "provider/text-only": {
                "id": "provider/text-only",
                "modalities": { "input": ["text"] }
            }
        }),
    )
    .await;
    let session_id = create_session(&harness).await;

    let response = http()
        .post(format!(
            "{}/v1/sessions/{}/prompt",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .json(&json!({
            "prompt": [{
                "type": "image",
                "data": "aW1hZ2U=",
                "mimeType": "image/png"
            }]
        }))
        .send()
        .await
        .expect("prompt");

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn prompt_gate_uses_array_target_model_for_media_checks() {
    let primary_model = "provider/text-only";
    let secondary_model = "provider/vision";
    let harness = Harness::spawn_with_models_cache(
        |config| {
            config.array.enabled = true;
            config.agent.model = Some(primary_model.to_owned());
            config
                .agent
                .args
                .extend(["--model-config-option".to_owned(), primary_model.to_owned()]);
            let mut secondary = config.agent.clone();
            secondary.id = "codex".to_owned();
            secondary.name = "Codex".to_owned();
            secondary.model = Some(secondary_model.to_owned());
            secondary.args = vec![
                "acp".to_owned(),
                "--model-config-option".to_owned(),
                secondary_model.to_owned(),
            ];
            config.array.targets.push(ArrayTargetConfig {
                id: "codex".to_owned(),
                agent: secondary,
            });
        },
        json!({
            primary_model: {
                "id": primary_model,
                "modalities": { "input": ["text"] }
            },
            secondary_model: {
                "id": secondary_model,
                "modalities": { "input": ["text", "image"] }
            }
        }),
    )
    .await;
    let client = http();
    let start = client
        .post(format!("{}/v1/array/targets/codex/start", harness.base_url))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("start codex");
    assert_eq!(start.status(), StatusCode::OK);

    let create = client
        .post(format!("{}/v1/sessions", harness.base_url))
        .header("Authorization", session_bearer())
        .json(&json!({ "target": "codex" }))
        .send()
        .await
        .expect("create session");
    assert_eq!(create.status(), StatusCode::OK);
    let session_id = create.json::<Value>().await.expect("create json")["data"]["id"]
        .as_str()
        .expect("session id")
        .to_owned();

    let response = client
        .post(format!(
            "{}/v1/sessions/{}/prompt?target=codex",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .json(&json!({
            "prompt": [{
                "type": "image",
                "data": "aW1hZ2U=",
                "mimeType": "image/png"
            }]
        }))
        .send()
        .await
        .expect("prompt");

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn websocket_streams_live_session_update_events() {
    let harness = Harness::spawn().await;
    let session_id = create_session(&harness).await;
    let request = websocket_request(&harness, session_bearer());
    let (mut ws, response) = tokio_tungstenite::connect_async(request)
        .await
        .expect("websocket connects");
    assert_eq!(response.status().as_u16(), 101);

    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        json!({
            "type": "subscribe",
            "topics": [format!("sessions.{session_id}")]
        })
        .to_string()
        .into(),
    ))
    .await
    .expect("subscribe");

    let client = http();
    let submit = client
        .post(format!(
            "{}/v1/sessions/{}/prompt",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .json(&json!({ "prompt": "stream me" }))
        .send()
        .await
        .expect("submit");
    assert_eq!(submit.status(), StatusCode::OK);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut received = None;
    while tokio::time::Instant::now() < deadline {
        let Some(message) = tokio::time::timeout(Duration::from_secs(1), ws.next())
            .await
            .expect("ws message before timeout")
        else {
            break;
        };
        let message = message.expect("ws message ok");
        let tokio_tungstenite::tungstenite::Message::Text(text) = message else {
            continue;
        };
        let event: Value = serde_json::from_str(&text).expect("event json");
        if event["type"] == "event"
            && event["topic"] == format!("sessions.{session_id}")
            && event["payload"]["kind"] == "session.update"
        {
            received = Some(event);
            break;
        }
    }
    let event = received.expect("session.update websocket event");
    assert!(event["id"].as_str().unwrap_or("").starts_with("evt_"));
    assert!(
        event["createdAt"].as_str().unwrap_or("").contains('T'),
        "createdAt should be an RFC3339 timestamp"
    );
    assert!(
        event["payload"].to_string().contains("chunk-"),
        "event payload = {event}"
    );
}

#[tokio::test]
async fn websocket_rejects_admin_key() {
    let harness = Harness::spawn().await;
    let request = websocket_request(&harness, admin_bearer());
    let err = tokio_tungstenite::connect_async(request)
        .await
        .expect_err("admin key must not upgrade session websocket");
    match err {
        tokio_tungstenite::tungstenite::Error::Http(response) => {
            assert_eq!(
                response.status().as_u16(),
                StatusCode::UNAUTHORIZED.as_u16()
            );
        }
        other => panic!("expected HTTP 401, got {other:?}"),
    }
}

#[tokio::test]
async fn full_lifecycle_create_list_get_prompt_poll_close() {
    let harness = Harness::spawn().await;
    let client = http();

    let session_id = create_session(&harness).await;

    // List returns the just-created session at the top.
    let list: Value = client
        .get(format!("{}/v1/sessions", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("list")
        .json()
        .await
        .expect("list json");
    let ids: Vec<&str> = list["data"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["id"].as_str())
        .collect();
    assert!(ids.contains(&session_id.as_str()), "list = {ids:?}");

    // GET by id returns full session row.
    let got: Value = client
        .get(format!("{}/v1/sessions/{}", harness.base_url, session_id))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("get json");
    assert_eq!(got["data"]["id"], session_id);
    assert_eq!(got["data"]["status"], "active");

    // Submit a prompt. Fire-and-forget returns a prompt id.
    let submit: Value = client
        .post(format!(
            "{}/v1/sessions/{}/prompt",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .json(&json!({ "prompt": "hello agent" }))
        .send()
        .await
        .expect("submit")
        .json()
        .await
        .expect("submit json");
    let prompt_id = submit["data"]["prompt_id"]
        .as_str()
        .expect("prompt id")
        .to_owned();
    let message_id = submit["data"]["message_id"]
        .as_str()
        .expect("prompt message id")
        .to_owned();

    // Poll until terminal. Bounded so a hung agent fails the test instead
    // of hanging CI forever.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let final_status = loop {
        if std::time::Instant::now() > deadline {
            panic!("prompt never settled");
        }
        let poll: Value = client
            .get(format!(
                "{}/v1/sessions/{}/prompts/{}",
                harness.base_url, session_id, prompt_id
            ))
            .header("Authorization", session_bearer())
            .send()
            .await
            .expect("poll")
            .json()
            .await
            .expect("poll json");
        let status = poll["data"]["status"].as_str().unwrap_or("").to_owned();
        if matches!(status.as_str(), "completed" | "errored" | "cancelled") {
            break poll["data"].clone();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(final_status["status"], "completed");
    assert_eq!(final_status["stop_reason"], "end_turn");
    assert_eq!(final_status["message_id"], message_id);
    assert_eq!(final_status["message_id_acknowledged"], true);

    // The fake agent emits two `session/update` notifications per prompt.
    // The bridge persists them keyed by session_id, so the events endpoint
    // returns at least those two plus our lifecycle rows.
    let events: Value = client
        .get(format!(
            "{}/v1/sessions/{}/events",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("events")
        .json()
        .await
        .expect("events json");
    let kinds: Vec<&str> = events["data"]["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["kind"].as_str())
        .collect();
    assert!(
        kinds.iter().filter(|k| **k == "session.update").count() >= 2,
        "expected >=2 session.update events, saw {kinds:?}"
    );
    assert!(kinds.contains(&"session.created"), "kinds = {kinds:?}");

    // Close transitions the row to closed.
    let close = client
        .delete(format!("{}/v1/sessions/{}", harness.base_url, session_id))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("close");
    assert_eq!(close.status(), StatusCode::OK);
    let close_body: Value = close.json().await.expect("close json");
    assert_eq!(close_body["data"]["status"], "closed");
}

#[tokio::test]
async fn fork_session_records_parent_lineage() {
    let harness = Harness::spawn().await;
    let client = http();
    let session_id = create_session(&harness).await;

    let forked: Value = client
        .post(format!(
            "{}/v1/sessions/{}/fork",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .json(&json!({}))
        .send()
        .await
        .expect("fork")
        .json()
        .await
        .expect("fork json");
    let child_id = forked["data"]["id"].as_str().expect("child id");

    let state = harness.state.lock().await;
    let child = state
        .get_session(child_id)
        .expect("child lookup")
        .expect("child exists");
    let metadata: Value = serde_json::from_str(&child.metadata_json).expect("metadata json");
    assert_eq!(metadata["fork"]["parent_session_id"], session_id);
    assert_eq!(metadata["fork"]["strategy"], "acp_native");
    assert!(metadata["fork"]["message_id"].is_null());
}

#[tokio::test]
async fn fork_session_forwards_message_breakpoint_to_placebo() {
    const BREAKPOINT_MESSAGE_ID: &str = "00000000-0000-4000-8000-000000000001";

    let harness = Harness::spawn_with(|config| {
        config.agent.args.extend([
            "--expect-fork-message-id".to_owned(),
            BREAKPOINT_MESSAGE_ID.to_owned(),
        ]);
    })
    .await;
    let client = http();
    let session_id = create_session(&harness).await;

    {
        let state = harness.state.lock().await;
        state
            .insert_prompt_with_message_id(
                NewPromptRecord {
                    id: "prm_fork_breakpoint".to_owned(),
                    session_id: session_id.clone(),
                    prompt_json: r#"[{"type":"text","text":"fork breakpoint"}]"#.to_owned(),
                },
                Some(BREAKPOINT_MESSAGE_ID.to_owned()),
            )
            .expect("prompt inserted");
        state
            .acknowledge_prompt_message_id("prm_fork_breakpoint", BREAKPOINT_MESSAGE_ID)
            .expect("prompt message id acknowledged");
    }

    let forked: Value = client
        .post(format!(
            "{}/v1/sessions/{}/fork",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .json(&json!({ "message_id": BREAKPOINT_MESSAGE_ID }))
        .send()
        .await
        .expect("fork")
        .json()
        .await
        .expect("fork json");
    let child_id = forked["data"]["id"].as_str().expect("child id");

    let state = harness.state.lock().await;
    let child = state
        .get_session(child_id)
        .expect("child lookup")
        .expect("child exists");
    let metadata: Value = serde_json::from_str(&child.metadata_json).expect("metadata json");
    assert_eq!(metadata["fork"]["parent_session_id"], session_id);
    assert_eq!(metadata["fork"]["strategy"], "acp_native");
    assert_eq!(metadata["fork"]["message_id"], BREAKPOINT_MESSAGE_ID);
}

#[tokio::test]
async fn load_and_resume_reject_closed_sessions() {
    let harness = Harness::spawn().await;
    let session_id = create_session(&harness).await;
    let client = http();

    let close = client
        .delete(format!("{}/v1/sessions/{}", harness.base_url, session_id))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("close");
    assert_eq!(close.status(), StatusCode::OK);

    for route in ["load", "resume"] {
        let response = client
            .post(format!(
                "{}/v1/sessions/{}/{}",
                harness.base_url, session_id, route
            ))
            .header("Authorization", session_bearer())
            .json(&json!({}))
            .send()
            .await
            .expect("session lifecycle request");
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body: Value = response.json().await.expect("json");
        assert_eq!(body["error"]["code"], "session.closed");
    }
}

#[tokio::test]
async fn close_session_on_secondary_target_survives_array_off() {
    // Regression: a session opened against a non-primary target while Array was
    // ON must stay closable after `acps array off`. Terminal wind-down ops
    // (close/cancel) bypass the Array-enabled gate so toggling Array off never
    // strands a session with a live agent and no way to wind it down.
    let harness = Harness::spawn_with(|config| {
        config.array.enabled = true;
        let mut secondary = config.agent.clone();
        secondary.id = "codex".to_owned();
        secondary.name = "Codex".to_owned();
        config.array.targets.push(ArrayTargetConfig {
            id: "codex".to_owned(),
            agent: secondary,
        });
    })
    .await;
    let client = http();

    // Start the secondary target and open a session against it while Array is on.
    let start = client
        .post(format!("{}/v1/array/targets/codex/start", harness.base_url))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("start codex");
    assert_eq!(start.status(), StatusCode::OK);

    let create = client
        .post(format!("{}/v1/sessions?target=codex", harness.base_url))
        .header("Authorization", session_bearer())
        .header("Content-Type", "application/json")
        .body("{}")
        .send()
        .await
        .expect("create session");
    assert_eq!(create.status(), StatusCode::OK);
    let session_id = create.json::<Value>().await.expect("create json")["data"]["id"]
        .as_str()
        .expect("session id")
        .to_owned();

    // Toggle Array off by rewriting the on-disk config; handlers re-read it.
    let mut disabled = Config::load_from_path(&harness.config_path).expect("load config");
    disabled.array.enabled = false;
    std::fs::write(
        &harness.config_path,
        disabled.to_canonical_toml().expect("canonical config"),
    )
    .expect("rewrite config");

    // Close must still succeed even though `codex` is no longer the active
    // default target; cancel shares the same wind-down resolver.
    let close = client
        .delete(format!("{}/v1/sessions/{}", harness.base_url, session_id))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("close session");
    assert_eq!(close.status(), StatusCode::OK);
    let close_body: Value = close.json().await.expect("close json");
    assert_eq!(close_body["data"]["status"], "closed");
}

#[tokio::test]
async fn stored_session_cwd_must_remain_under_workspace_for_reuse() {
    let harness = Harness::spawn().await;
    let outside = tempfile::tempdir().expect("outside");
    {
        let state = harness.state.lock().await;
        state
            .insert_session(NewSessionRecord {
                id: "sess_bad_cwd".to_owned(),
                agent_id: "placebo".to_owned(),
                cwd: outside.path().to_string_lossy().into_owned(),
                title: None,
                metadata_json: "{}".to_owned(),
            })
            .expect("session inserted");
    }

    let response = http()
        .post(format!(
            "{}/v1/sessions/{}/load",
            harness.base_url, "sess_bad_cwd"
        ))
        .header("Authorization", session_bearer())
        .json(&json!({}))
        .send()
        .await
        .expect("load");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "prompt.body_invalid");
}

#[tokio::test]
async fn stored_inner_cwd_is_valid_for_load_resume_and_fork() {
    let harness = Harness::spawn().await;
    let inner = harness.workspace_root.join("stored-inner");
    std::fs::create_dir(&inner).expect("inner dir");
    {
        let state = harness.state.lock().await;
        state
            .insert_session(NewSessionRecord {
                id: "sess_valid_cwd".to_owned(),
                agent_id: "placebo".to_owned(),
                cwd: inner.to_string_lossy().into_owned(),
                title: None,
                metadata_json: "{}".to_owned(),
            })
            .expect("session inserted");
    }

    let client = http();
    for route in ["load", "resume", "fork"] {
        let response = client
            .post(format!(
                "{}/v1/sessions/{}/{}",
                harness.base_url, "sess_valid_cwd", route
            ))
            .header("Authorization", session_bearer())
            .json(&json!({}))
            .send()
            .await
            .expect("session lifecycle request");
        assert_eq!(response.status(), StatusCode::OK);
    }
}

#[tokio::test]
async fn explicit_load_and_resume_cwd_is_persisted_after_agent_success() {
    let harness = Harness::spawn().await;
    let session_id = create_session(&harness).await;
    let load_cwd = harness.workspace_root.join("load-cwd");
    let resume_cwd = harness.workspace_root.join("resume-cwd");
    std::fs::create_dir(&load_cwd).expect("load cwd");
    std::fs::create_dir(&resume_cwd).expect("resume cwd");
    let client = http();

    let load_body: Value = client
        .post(format!(
            "{}/v1/sessions/{}/load",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .json(&json!({ "cwd": load_cwd.to_string_lossy() }))
        .send()
        .await
        .expect("load")
        .json()
        .await
        .expect("load json");
    let canonical_load = load_cwd.canonicalize().expect("canonical load cwd");
    assert_eq!(
        load_body["data"]["cwd"],
        canonical_load.to_string_lossy().as_ref()
    );

    let resume_body: Value = client
        .post(format!(
            "{}/v1/sessions/{}/resume",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .json(&json!({ "cwd": resume_cwd.to_string_lossy() }))
        .send()
        .await
        .expect("resume")
        .json()
        .await
        .expect("resume json");
    let canonical_resume = resume_cwd.canonicalize().expect("canonical resume cwd");
    assert_eq!(
        resume_body["data"]["cwd"],
        canonical_resume.to_string_lossy().as_ref()
    );

    let state = harness.state.lock().await;
    let stored = state
        .get_session(&session_id)
        .expect("session lookup")
        .expect("session exists");
    assert_eq!(stored.cwd, canonical_resume.to_string_lossy());
}

#[cfg(unix)]
#[tokio::test]
async fn stored_session_cwd_symlink_escape_is_rejected_before_reuse() {
    let harness = Harness::spawn().await;
    let inner = harness.workspace_root.join("stored-cwd");
    std::fs::create_dir(&inner).expect("inner dir");
    {
        let state = harness.state.lock().await;
        state
            .insert_session(NewSessionRecord {
                id: "sess_changed_cwd".to_owned(),
                agent_id: "placebo".to_owned(),
                cwd: inner.to_string_lossy().into_owned(),
                title: None,
                metadata_json: "{}".to_owned(),
            })
            .expect("session inserted");
    }
    std::fs::remove_dir(&inner).expect("remove inner");
    let outside = tempfile::tempdir().expect("outside");
    std::os::unix::fs::symlink(outside.path(), &inner).expect("replace with symlink");

    let response = http()
        .post(format!(
            "{}/v1/sessions/{}/resume",
            harness.base_url, "sess_changed_cwd"
        ))
        .header("Authorization", session_bearer())
        .json(&json!({}))
        .send()
        .await
        .expect("resume");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "prompt.body_invalid");
}

#[tokio::test]
async fn sessions_list_syncs_agent_discovered_sessions() {
    let harness = Harness::spawn().await;
    let client = http();

    let list: Value = client
        .get(format!("{}/v1/sessions", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("list")
        .json()
        .await
        .expect("list json");

    assert_eq!(list["data"]["agent_sync"]["attempted"], true);
    assert_eq!(list["data"]["agent_sync"]["status"], "synced");
    assert_eq!(list["data"]["agent_sync"]["upserted"], 1);
    let listed = list["data"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["agent_session_id"] == "sess_listed_0")
        .expect("listed session present");
    assert!(listed["id"].as_str().is_some_and(|id| !id.is_empty()));
    assert_eq!(listed["status"], "available");
    assert_eq!(listed["title"], "listed session");
    let metadata: Value =
        serde_json::from_str(listed["metadata_json"].as_str().unwrap()).expect("metadata json");
    assert_eq!(metadata["agent_meta"]["origin"], "placebo-agent");
}

#[tokio::test]
async fn sessions_list_skips_agent_discovered_cwd_outside_workspace() {
    let outside = tempfile::tempdir().expect("outside");
    let harness = Harness::spawn_with(|config| {
        config.agent.args.extend([
            "--listed-cwd".to_owned(),
            outside.path().to_string_lossy().into_owned(),
        ]);
    })
    .await;

    let list: Value = http()
        .get(format!("{}/v1/sessions", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("list")
        .json()
        .await
        .expect("list json");

    assert_eq!(list["data"]["agent_sync"]["attempted"], true);
    assert_eq!(list["data"]["agent_sync"]["status"], "synced");
    assert_eq!(list["data"]["agent_sync"]["upserted"], 0);
    let listed = list["data"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["id"] == "sess_listed_0");
    assert!(listed.is_none(), "invalid listed cwd must be skipped");
}

#[tokio::test]
async fn sessions_list_preserves_active_local_sessions() {
    let harness = Harness::spawn().await;
    let client = http();
    let session_id = create_session(&harness).await;

    let list: Value = client
        .get(format!("{}/v1/sessions", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("list")
        .json()
        .await
        .expect("list json");

    let active = list["data"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["id"].as_str() == Some(session_id.as_str()))
        .expect("created session present");
    assert_eq!(active["status"], "active");
    assert_eq!(list["data"]["agent_sync"]["updated"], 1);
}

#[tokio::test]
async fn sessions_list_works_when_agent_list_is_unsupported() {
    let harness = Harness::spawn_with(|config| {
        config.agent.args.push("--no-cap-list-session".into());
    })
    .await;
    let client = http();

    let response = client
        .get(format!("{}/v1/sessions", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("list");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("list json");
    assert_eq!(body["data"]["agent_sync"]["attempted"], false);
    assert_eq!(body["data"]["agent_sync"]["status"], "unsupported");
    assert!(body["data"]["sessions"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn sessions_list_filters_by_since_and_until() {
    let harness = Harness::spawn_with(|config| {
        config.agent.args.push("--no-cap-list-session".into());
    })
    .await;
    {
        let store = harness.state.lock().await;
        store
            .upsert_listed_sessions(vec![
                acp_stack::state::ListedSessionRecord {
                    id: "sess_old".to_owned(),
                    agent_session_id: "sess_old".to_owned(),
                    agent_id: "placebo".to_owned(),
                    cwd: "/tmp/old".to_owned(),
                    title: None,
                    updated_at: Some("2026-01-01T00:00:00Z".to_owned()),
                    metadata_json: "{}".to_owned(),
                },
                acp_stack::state::ListedSessionRecord {
                    id: "sess_mid".to_owned(),
                    agent_session_id: "sess_mid".to_owned(),
                    agent_id: "placebo".to_owned(),
                    cwd: "/tmp/mid".to_owned(),
                    title: None,
                    updated_at: Some("2026-02-01T00:00:00Z".to_owned()),
                    metadata_json: "{}".to_owned(),
                },
                acp_stack::state::ListedSessionRecord {
                    id: "sess_new".to_owned(),
                    agent_session_id: "sess_new".to_owned(),
                    agent_id: "placebo".to_owned(),
                    cwd: "/tmp/new".to_owned(),
                    title: None,
                    updated_at: Some("2026-03-01T00:00:00Z".to_owned()),
                    metadata_json: "{}".to_owned(),
                },
            ])
            .expect("sessions inserted");
    }
    let client = http();
    let body: Value = client
        .get(format!(
            "{}/v1/sessions?since=2026-01-15T00%3A00%3A00Z&until=2026-02-15T00%3A00%3A00Z",
            harness.base_url
        ))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("list")
        .json()
        .await
        .expect("list json");

    let ids: Vec<&str> = body["data"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|session| session["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["sess_mid"]);
}

#[tokio::test]
async fn sessions_list_rejects_malformed_bounds() {
    let harness = Harness::spawn_with(|config| {
        config.agent.args.push("--no-cap-list-session".into());
    })
    .await;
    let response = http()
        .get(format!("{}/v1/sessions?since=not-a-time", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("list");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "request.invalid_param");
}

#[tokio::test]
async fn sessions_list_rejects_duration_before_unix_epoch() {
    let harness = Harness::spawn_with(|config| {
        config.agent.args.push("--no-cap-list-session".into());
    })
    .await;
    let response = http()
        .get(format!(
            "{}/v1/sessions?range=999999999999999999y",
            harness.base_url
        ))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("list");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "request.invalid_param");
}

#[tokio::test]
async fn sessions_list_resolves_missing_explicit_bound_to_session_span() {
    let harness = Harness::spawn_with(|config| {
        config.agent.args.push("--no-cap-list-session".into());
    })
    .await;
    {
        let store = harness.state.lock().await;
        store
            .upsert_listed_sessions(vec![
                acp_stack::state::ListedSessionRecord {
                    id: "sess_first".to_owned(),
                    agent_session_id: "sess_first".to_owned(),
                    agent_id: "placebo".to_owned(),
                    cwd: "/tmp/first".to_owned(),
                    title: None,
                    updated_at: Some("2026-02-01T00:00:00Z".to_owned()),
                    metadata_json: "{}".to_owned(),
                },
                acp_stack::state::ListedSessionRecord {
                    id: "sess_latest".to_owned(),
                    agent_session_id: "sess_latest".to_owned(),
                    agent_id: "placebo".to_owned(),
                    cwd: "/tmp/latest".to_owned(),
                    title: None,
                    updated_at: Some("2026-02-02T00:00:00Z".to_owned()),
                    metadata_json: "{}".to_owned(),
                },
            ])
            .expect("sessions inserted");
    }

    let body: Value = http()
        .get(format!(
            "{}/v1/sessions?resolve_bounds=true&until=2026-02-01T12%3A00%3A00Z",
            harness.base_url
        ))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("list")
        .json()
        .await
        .expect("list json");
    let ids: Vec<&str> = body["data"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|session| session["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["sess_first"]);

    let body: Value = http()
        .get(format!(
            "{}/v1/sessions?resolve_bounds=true&since=2026-02-01T12%3A00%3A00Z",
            harness.base_url
        ))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("list")
        .json()
        .await
        .expect("list json");
    let ids: Vec<&str> = body["data"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|session| session["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["sess_latest"]);
}

#[tokio::test]
async fn sessions_list_range_counts_from_request_time() {
    let harness = Harness::spawn_with(|config| {
        config.agent.args.push("--no-cap-list-session".into());
    })
    .await;
    {
        let store = harness.state.lock().await;
        store
            .insert_session(NewSessionRecord {
                id: "sess_active".to_owned(),
                agent_id: "placebo".to_owned(),
                cwd: "/tmp/active".to_owned(),
                title: None,
                metadata_json: "{}".to_owned(),
            })
            .expect("session inserted");
    }

    let body: Value = http()
        .get(format!("{}/v1/sessions?range=30m", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("list")
        .json()
        .await
        .expect("list json");

    let ids: Vec<&str> = body["data"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|session| session["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["sess_active"]);
}

#[tokio::test]
async fn sessions_status_returns_compact_active_summary() {
    let harness = Harness::spawn_with(|config| {
        config.agent.args.push("--no-cap-list-session".into());
    })
    .await;
    {
        let store = harness.state.lock().await;
        store
            .insert_session(NewSessionRecord {
                id: "sess_active".to_owned(),
                agent_id: "placebo".to_owned(),
                cwd: "/tmp/active".to_owned(),
                title: Some("active title".to_owned()),
                metadata_json: r#"{"secretish":"not returned"}"#.to_owned(),
            })
            .expect("session inserted");
        store
            .append_session_event_with_source(
                "sess_active",
                "info",
                "session.update",
                acp_stack::state::EVENT_SOURCE_ACP,
                "ACP session update",
                "{}",
            )
            .expect("event inserted");
    }

    let body: Value = http()
        .get(format!("{}/v1/sessions/-/status", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("status")
        .json()
        .await
        .expect("status json");

    assert_eq!(body["data"]["active_count"], 1);
    assert_eq!(body["data"]["session_count"], 1);
    assert_eq!(body["data"]["window"], "8h");
    assert!(body["data"]["window_start"].is_string());
    assert!(body["data"]["window_end"].is_string());
    let session = &body["data"]["sessions"][0];
    assert_eq!(session["id"], "sess_active");
    assert_eq!(session["state"], "idle");
    assert_eq!(session["last_activity_from"], "agent");
    assert_eq!(session["recent"], true);
    assert!(session.get("metadata_json").is_none());
}

#[tokio::test]
async fn sessions_status_defaults_to_primary_target() {
    let harness = Harness::spawn_with(|config| {
        config.agent.args.push("--no-cap-list-session".into());
    })
    .await;
    {
        let store = harness.state.lock().await;
        store
            .insert_session(NewSessionRecord {
                id: "sess_primary".to_owned(),
                agent_id: "placebo".to_owned(),
                cwd: "/tmp/primary".to_owned(),
                title: None,
                metadata_json: "{}".to_owned(),
            })
            .expect("primary session inserted");
        store
            .insert_session_for_target(
                "placebo-secondary",
                "acp_secondary".to_owned(),
                NewSessionRecord {
                    id: "sess_secondary".to_owned(),
                    agent_id: "placebo-secondary".to_owned(),
                    cwd: "/tmp/secondary".to_owned(),
                    title: None,
                    metadata_json: "{}".to_owned(),
                },
            )
            .expect("secondary session inserted");
    }

    let body: Value = http()
        .get(format!("{}/v1/sessions/-/status", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("status")
        .json()
        .await
        .expect("status json");

    let ids: Vec<&str> = body["data"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|session| session["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["sess_primary"]);
}

#[tokio::test]
async fn sessions_target_obeys_array_off_written_after_daemon_start() {
    let harness = Harness::spawn_with(|config| {
        config.array.enabled = true;
        let mut secondary = config.agent.clone();
        secondary.id = "placebo-secondary".to_owned();
        secondary.name = "Placebo Secondary".to_owned();
        config.array.targets.push(ArrayTargetConfig {
            id: "placebo-secondary".to_owned(),
            agent: secondary,
        });
    })
    .await;
    let mut updated =
        Config::load_from_path(&harness.config_path).expect("config should load from disk");
    updated.array.enabled = false;
    std::fs::write(
        &harness.config_path,
        updated.to_canonical_toml().expect("canonical config"),
    )
    .expect("config should be rewritten");

    let response = http()
        .get(format!(
            "{}/v1/sessions?target=placebo-secondary",
            harness.base_url
        ))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("list");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "request.invalid_param");
}

#[tokio::test]
async fn sessions_status_marks_old_activity_idle() {
    let harness = Harness::spawn_with(|config| {
        config.agent.args.push("--no-cap-list-session".into());
    })
    .await;
    {
        let store = harness.state.lock().await;
        store
            .insert_session(NewSessionRecord {
                id: "sess_active".to_owned(),
                agent_id: "placebo".to_owned(),
                cwd: "/tmp/active".to_owned(),
                title: None,
                metadata_json: "{}".to_owned(),
            })
            .expect("session inserted");
    }

    let body: Value = http()
        .get(format!(
            "{}/v1/sessions/-/status?threshold=0s",
            harness.base_url
        ))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("status")
        .json()
        .await
        .expect("status json");

    assert_eq!(body["data"]["sessions"][0]["recent"], false);
}

#[tokio::test]
async fn sessions_status_rejects_malformed_threshold() {
    let harness = Harness::spawn().await;
    let response = http()
        .get(format!(
            "{}/v1/sessions/-/status?threshold=not-a-duration",
            harness.base_url
        ))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("status");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "request.invalid_param");
}

#[tokio::test]
async fn sessions_status_rejects_window_outside_bounds() {
    let harness = Harness::spawn().await;
    let response = http()
        .get(format!(
            "{}/v1/sessions/-/status?window=1000h",
            harness.base_url
        ))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("status");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "request.invalid_param");
}

#[tokio::test]
async fn sessions_status_reports_turn_states() {
    let harness = Harness::spawn_with(|config| {
        config.agent.args.push("--no-cap-list-session".into());
    })
    .await;
    {
        let store = harness.state.lock().await;
        for session_id in [
            "sess_prompt_sent",
            "sess_working",
            "sess_done",
            "sess_error",
            "sess_permission",
        ] {
            store
                .insert_session(NewSessionRecord {
                    id: session_id.to_owned(),
                    agent_id: "placebo".to_owned(),
                    cwd: format!("/tmp/{session_id}"),
                    title: None,
                    metadata_json: "{}".to_owned(),
                })
                .expect("session inserted");
            let prompt_id = format!("prm_{session_id}");
            store
                .insert_prompt(NewPromptRecord {
                    id: prompt_id.clone(),
                    session_id: session_id.to_owned(),
                    prompt_json: "[]".to_owned(),
                })
                .expect("prompt inserted");
            store
                .update_prompt_status(
                    &prompt_id,
                    PromptStatus::Running,
                    None,
                    None,
                    None,
                    None,
                    None,
                )
                .expect("prompt running");
        }

        std::thread::sleep(Duration::from_millis(2));
        for session_id in ["sess_working", "sess_permission"] {
            store
                .append_session_event_with_source(
                    session_id,
                    "info",
                    "session.update",
                    acp_stack::state::EVENT_SOURCE_ACP,
                    "ACP session update",
                    "{}",
                )
                .expect("session update");
        }
        store
            .update_prompt_status(
                "prm_sess_done",
                PromptStatus::Completed,
                Some("end_turn"),
                None,
                None,
                None,
                None,
            )
            .expect("prompt completed");
        store
            .update_prompt_status(
                "prm_sess_error",
                PromptStatus::Errored,
                None,
                Some("agent.request_failed"),
                Some("failed"),
                None,
                None,
            )
            .expect("prompt errored");
        store
            .append_permission_request(NewPermissionRequest {
                source: "acp",
                requester: Some("agent"),
                subject_id: Some("sess_permission"),
                detail_json: "{}",
                expires_at: None,
            })
            .expect("permission inserted");
    }

    let body: Value = http()
        .get(format!(
            "{}/v1/sessions/-/status?window=1h",
            harness.base_url
        ))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("status")
        .json()
        .await
        .expect("status json");
    let sessions = body["data"]["sessions"].as_array().expect("sessions array");
    let state_for = |id: &str| {
        sessions
            .iter()
            .find(|session| session["id"] == id)
            .and_then(|session| session["state"].as_str())
            .unwrap_or_else(|| panic!("missing state for {id}; body={body}"))
    };

    assert_eq!(state_for("sess_prompt_sent"), "prompt_sent");
    assert_eq!(state_for("sess_working"), "working");
    assert_eq!(state_for("sess_done"), "done");
    assert_eq!(state_for("sess_error"), "error");
    assert_eq!(state_for("sess_permission"), "permission_required");
    let permission_session = sessions
        .iter()
        .find(|session| session["id"] == "sess_permission")
        .expect("permission session");
    assert!(permission_session["permission"]["id"].is_string());
}

#[tokio::test]
async fn available_session_must_be_loaded_before_prompting() {
    let harness = Harness::spawn().await;
    let client = http();
    let list: Value = client
        .get(format!("{}/v1/sessions", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("list")
        .json()
        .await
        .expect("list json");
    let session_id = list["data"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["agent_session_id"] == "sess_listed_0")
        .and_then(|session| session["id"].as_str())
        .expect("listed session local id");

    let response = client
        .post(format!(
            "{}/v1/sessions/{}/prompt",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .json(&json!({ "prompt": "hello agent" }))
        .send()
        .await
        .expect("prompt");
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body: Value = response.json().await.expect("prompt json");
    assert_eq!(body["error"]["code"], "session.not_active");
}

#[tokio::test]
async fn unsupported_capability_load_returns_501() {
    let harness = Harness::spawn_with(|config| {
        config.agent.args.push("--no-cap-load-session".into());
    })
    .await;
    let session_id = create_session(&harness).await;
    let client = http();

    let response = client
        .post(format!(
            "{}/v1/sessions/{}/load",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .header("Content-Type", "application/json")
        .body("{}")
        .send()
        .await
        .expect("load");
    assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "agent.unsupported_capability");
}

#[tokio::test]
async fn unsupported_capability_resume_returns_501() {
    let harness = Harness::spawn_with(|config| {
        config.agent.args.push("--no-cap-resume-session".into());
    })
    .await;
    let session_id = create_session(&harness).await;
    let client = http();

    let response = client
        .post(format!(
            "{}/v1/sessions/{}/resume",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .header("Content-Type", "application/json")
        .body("{}")
        .send()
        .await
        .expect("resume");
    assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "agent.unsupported_capability");
}

#[tokio::test]
async fn unsupported_capability_close_returns_501_and_leaves_session_active() {
    let harness = Harness::spawn_with(|config| {
        config.agent.args.push("--no-cap-close-session".into());
    })
    .await;
    let session_id = create_session(&harness).await;
    let client = http();

    let response = client
        .delete(format!("{}/v1/sessions/{}", harness.base_url, session_id))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("close");
    assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "agent.unsupported_capability");

    let session: Value = client
        .get(format!("{}/v1/sessions/{}", harness.base_url, session_id))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("get json");
    assert_eq!(session["data"]["status"], "active");
}

#[tokio::test]
async fn session_routes_reject_admin_keys() {
    let harness = Harness::spawn().await;
    let client = http();
    let response = client
        .get(format!("{}/v1/sessions", harness.base_url))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("list");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "auth.wrong_kind");
}

#[tokio::test]
async fn unknown_session_returns_404() {
    let harness = Harness::spawn().await;
    let client = http();
    let response = client
        .get(format!(
            "{}/v1/sessions/sess_does_not_exist",
            harness.base_url
        ))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("get");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "session.not_found");
}

#[tokio::test]
async fn unknown_prompt_returns_404() {
    let harness = Harness::spawn().await;
    let session_id = create_session(&harness).await;
    let client = http();
    let response = client
        .get(format!(
            "{}/v1/sessions/{}/prompts/prm_missing",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("get");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "prompt.not_found");
}

#[tokio::test]
async fn session_update_notifications_land_in_events_table() {
    let harness = Harness::spawn().await;
    let session_id = create_session(&harness).await;
    let client = http();
    let submit: Value = client
        .post(format!(
            "{}/v1/sessions/{}/prompt",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .json(&json!({ "prompt": "ping" }))
        .send()
        .await
        .expect("submit")
        .json()
        .await
        .expect("submit json");
    let prompt_id = submit["data"]["prompt_id"]
        .as_str()
        .expect("prompt id")
        .to_owned();

    // Wait for terminal status before querying state — the writer task
    // settles the prompt row, and only then are all the session.update rows
    // guaranteed to have flushed.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if std::time::Instant::now() > deadline {
            panic!("prompt did not settle");
        }
        let poll: Value = client
            .get(format!(
                "{}/v1/sessions/{}/prompts/{}",
                harness.base_url, session_id, prompt_id
            ))
            .header("Authorization", session_bearer())
            .send()
            .await
            .expect("poll")
            .json()
            .await
            .expect("poll json");
        if matches!(
            poll["data"]["status"].as_str().unwrap_or(""),
            "completed" | "errored" | "cancelled"
        ) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Query the state store directly to assert events landed.
    let store = harness.state.lock().await;
    let events = store
        .query_session_events(&session_id, None, 100)
        .expect("session events");
    drop(store);
    let updates = events.iter().filter(|e| e.kind == "session.update").count();
    assert!(
        updates >= 2,
        "expected >=2 session.update rows, saw {updates}"
    );
}

#[tokio::test]
async fn sessions_snapshot_returns_session_in_flight_prompts_and_recent_events() {
    let harness = Harness::spawn_with(|config| {
        // Disable the bridge's `session/list` capability so the placebo agent
        // path leaves the state untouched after start; we want a clean slate
        // to seed deterministic snapshot fixtures.
        config.agent.args.push("--no-cap-list-session".into());
    })
    .await;
    let session_id = "sess_snapshot".to_owned();
    {
        let store = harness.state.lock().await;
        store
            .insert_session(NewSessionRecord {
                id: session_id.clone(),
                agent_id: "placebo".to_owned(),
                cwd: "/tmp/snap".to_owned(),
                title: Some("snap".to_owned()),
                metadata_json: "{}".to_owned(),
            })
            .expect("session inserted");
        store
            .insert_prompt(NewPromptRecord {
                id: "prm_inflight".to_owned(),
                session_id: session_id.clone(),
                prompt_json: r#"[{"type":"text","text":"hi"}]"#.to_owned(),
            })
            .expect("prompt inserted");
        for index in 0..3 {
            store
                .append_session_event_with_source(
                    &session_id,
                    "info",
                    "session.update",
                    acp_stack::state::EVENT_SOURCE_ACP,
                    "ACP session update",
                    &format!(r#"{{"seq":{index}}}"#),
                )
                .expect("event inserted");
        }
    }

    let body: Value = http()
        .get(format!(
            "{}/v1/sessions/{}/snapshot",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("snapshot")
        .json()
        .await
        .expect("snapshot json");

    assert_eq!(body["data"]["session"]["id"], session_id);
    assert_eq!(body["data"]["session"]["status"], "active");
    let in_flight = body["data"]["in_flight_prompts"]
        .as_array()
        .expect("in_flight_prompts array");
    assert_eq!(in_flight.len(), 1);
    assert_eq!(in_flight[0]["id"], "prm_inflight");
    assert_eq!(in_flight[0]["status"], "pending");

    let events = body["data"]["recent_events"]
        .as_array()
        .expect("recent_events array");
    assert_eq!(events.len(), 3);
    // Newest-first: the third event we appended carries `"seq":2`.
    let head_payload: Value = serde_json::from_str(events[0]["payload_json"].as_str().unwrap())
        .expect("head payload json");
    assert_eq!(head_payload["seq"], 2);
    let tail_payload: Value = serde_json::from_str(events[2]["payload_json"].as_str().unwrap())
        .expect("tail payload json");
    assert_eq!(tail_payload["seq"], 0);

    let last_event_id = body["data"]["last_event_id"]
        .as_str()
        .expect("last_event_id present");
    assert_eq!(last_event_id, events[0]["id"].as_str().unwrap());
}

#[tokio::test]
async fn sessions_snapshot_returns_404_for_unknown_session() {
    let harness = Harness::spawn().await;
    let response = http()
        .get(format!(
            "{}/v1/sessions/sess_does_not_exist/snapshot",
            harness.base_url
        ))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("snapshot");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn sessions_changes_returns_an_empty_ephemeral_snapshot_and_validates_target() {
    let harness = Harness::spawn_with(|config| {
        config.agent.args.push("--no-cap-list-session".into());
    })
    .await;
    let session_id = "sess_changes_empty".to_owned();
    {
        let store = harness.state.lock().await;
        store
            .insert_session(NewSessionRecord {
                id: session_id.clone(),
                agent_id: "placebo".to_owned(),
                cwd: "/tmp/changes".to_owned(),
                title: None,
                metadata_json: "{}".to_owned(),
            })
            .expect("session inserted");
    }

    let response = http()
        .get(format!(
            "{}/v1/sessions/{}/changes",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("changes request");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("changes JSON");
    assert_eq!(body["data"]["session_id"], session_id);
    assert_eq!(body["data"]["revision"], 0);
    assert_eq!(body["data"]["truncated"], false);
    assert_eq!(body["data"]["tool_calls"], json!([]));
    assert_eq!(
        body["data"]["generation"]
            .as_str()
            .expect("generation")
            .len(),
        32
    );

    let wrong_target = http()
        .get(format!(
            "{}/v1/sessions/{}/changes?target_id=wrong",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("wrong target request");
    assert_eq!(wrong_target.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn sessions_changes_returns_404_for_unknown_session() {
    let harness = Harness::spawn().await;
    let response = http()
        .get(format!(
            "{}/v1/sessions/sess_does_not_exist/changes",
            harness.base_url
        ))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("changes request");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn sessions_snapshot_caps_recent_events_at_50() {
    let harness = Harness::spawn_with(|config| {
        config.agent.args.push("--no-cap-list-session".into());
    })
    .await;
    let session_id = "sess_snapshot_cap".to_owned();
    {
        let store = harness.state.lock().await;
        store
            .insert_session(NewSessionRecord {
                id: session_id.clone(),
                agent_id: "placebo".to_owned(),
                cwd: "/tmp/cap".to_owned(),
                title: None,
                metadata_json: "{}".to_owned(),
            })
            .expect("session inserted");
        for index in 0..75 {
            store
                .append_session_event_with_source(
                    &session_id,
                    "info",
                    "session.update",
                    acp_stack::state::EVENT_SOURCE_ACP,
                    "ACP session update",
                    &format!(r#"{{"seq":{index}}}"#),
                )
                .expect("event inserted");
        }
    }

    let body: Value = http()
        .get(format!(
            "{}/v1/sessions/{}/snapshot",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("snapshot")
        .json()
        .await
        .expect("snapshot json");

    let events = body["data"]["recent_events"]
        .as_array()
        .expect("recent_events array");
    assert_eq!(events.len(), 50);
    // The cap should keep the newest 50, so the head still carries `"seq":74`.
    let head_payload: Value = serde_json::from_str(events[0]["payload_json"].as_str().unwrap())
        .expect("head payload json");
    assert_eq!(head_payload["seq"], 74);
}

#[tokio::test]
async fn append_session_event_fans_out_to_session_and_logs_topics() {
    let harness = Harness::spawn().await;
    let session_id = create_session(&harness).await;

    // One subscriber per topic; the bug we are guarding against silently
    // dropped session-topic delivery while logs-topic delivery still worked.
    let session_request = websocket_request(&harness, session_bearer());
    let (mut session_ws, _) = tokio_tungstenite::connect_async(session_request)
        .await
        .expect("session websocket connects");
    session_ws
        .send(tokio_tungstenite::tungstenite::Message::Text(
            json!({
                "type": "subscribe",
                "topics": [format!("sessions.{session_id}")]
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("session subscribe");

    let logs_request = websocket_request(&harness, session_bearer());
    let (mut logs_ws, _) = tokio_tungstenite::connect_async(logs_request)
        .await
        .expect("logs websocket connects");
    logs_ws
        .send(tokio_tungstenite::tungstenite::Message::Text(
            json!({
                "type": "subscribe",
                "topics": ["logs"]
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("logs subscribe");

    // The WS server processes subscribe frames inside the same select! arm as
    // event fanout, so a state write that happens before the server has
    // observed the subscribe frame is silently dropped on the broadcast end.
    // Poll the connections endpoint until both topics show as subscribed.
    let subscribe_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if tokio::time::Instant::now() > subscribe_deadline {
            panic!("ws subscriptions never registered");
        }
        let connections: Value = http()
            .get(format!("{}/v1/ws/connections", harness.base_url))
            .header("Authorization", session_bearer())
            .send()
            .await
            .expect("ws connections")
            .json()
            .await
            .expect("ws connections json");
        let topics_present: Vec<String> = connections["data"]["connections"]
            .as_array()
            .unwrap_or(&Vec::new())
            .iter()
            .flat_map(|connection| {
                connection["topics"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|topic| topic.as_str().map(str::to_owned))
            })
            .collect();
        if topics_present
            .iter()
            .any(|topic| topic == &format!("sessions.{session_id}"))
            && topics_present.iter().any(|topic| topic == "logs")
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Direct state write so the assertion targets the publish site, not the
    // bridge plumbing.
    {
        let store = harness.state.lock().await;
        store
            .append_session_event_with_source(
                &session_id,
                "info",
                "session.update",
                acp_stack::state::EVENT_SOURCE_ACP,
                "ACP session update",
                r#"{"seq":42}"#,
            )
            .expect("event inserted");
    }

    let session_event = recv_matching_event(
        &mut session_ws,
        &format!("sessions.{session_id}"),
        "session.update",
    )
    .await
    .expect("session.update on sessions.{id} topic");
    let session_payload: Value =
        serde_json::from_value(session_event["payload"]["data"].clone()).expect("session data");
    assert_eq!(session_payload["seq"], 42);

    let logs_event = recv_matching_event(&mut logs_ws, "logs", "session.update")
        .await
        .expect("session.update on logs topic");
    assert_eq!(logs_event["payload"]["data"]["kind"], "session.update");
}

/// Phase 2: when the agent's `session/prompt` JSON-RPC failure carries an
/// embedded HTTP status (e.g. `503 Service Unavailable`), the supervisor
/// classifies it as an inference-5xx failure, persists the structured detail
/// envelope, and emits a `prompt.inference_failed` session event. The raw
/// upstream message — including the URL and secret-looking token below — must
/// never reach the persisted `error_message`, `failure_detail_json`, or event
/// payload.
#[tokio::test]
async fn prompt_inference_5xx_persists_taxonomy_and_emits_event() {
    let injected_message = "upstream call to https://api.openai.com/v1/chat?key=sk-secret returned 503 Service Unavailable";
    let harness = Harness::spawn_with(|config| {
        config
            .agent
            .args
            .extend(["--prompt-inference-error".into(), injected_message.into()]);
    })
    .await;
    let session_id = create_session(&harness).await;

    let client = http();
    let submit: Value = client
        .post(format!(
            "{}/v1/sessions/{}/prompt",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .json(&json!({ "prompt": "ping the upstream" }))
        .send()
        .await
        .expect("submit")
        .json()
        .await
        .expect("submit json");
    let prompt_id = submit["data"]["prompt_id"]
        .as_str()
        .expect("prompt id")
        .to_owned();

    // Poll the prompt row until it lands in a terminal status; the inference
    // failure path settles as `errored`.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let terminal = loop {
        if std::time::Instant::now() > deadline {
            panic!("prompt never settled");
        }
        let state = harness.state.lock().await;
        let prompt = state.get_prompt(&prompt_id).expect("prompt lookup");
        drop(state);
        if let Some(record) = prompt
            && matches!(record.status.as_str(), "errored" | "stalled" | "cancelled")
        {
            break record;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    assert_eq!(terminal.status, "errored");
    assert_eq!(
        terminal.error_code.as_deref(),
        Some("agent.inference_5xx"),
        "expected inference_5xx error_code, got {:?}",
        terminal.error_code,
    );
    assert_eq!(
        terminal.failure_class.as_deref(),
        Some("inference_5xx"),
        "expected failure_class inference_5xx, got {:?}",
        terminal.failure_class,
    );

    let detail = terminal
        .failure_detail_json
        .as_deref()
        .expect("failure_detail_json present");
    let detail_value: Value = serde_json::from_str(detail).expect("detail json");
    assert_eq!(detail_value["status_code"], 503);
    assert_eq!(detail_value["reason_category"], "service_unavailable");

    // The persisted error_message must NOT contain any portion of the raw
    // upstream string (URL substring, secret-looking token, raw status text).
    let error_message = terminal
        .error_message
        .as_deref()
        .expect("public message present");
    assert!(
        !error_message.contains("503 Service Unavailable"),
        "raw status text leaked into error_message: {error_message}"
    );
    assert!(
        !error_message.contains("api.openai.com"),
        "url leaked into error_message: {error_message}"
    );
    assert!(
        !error_message.contains("sk-secret"),
        "secret-looking token leaked into error_message: {error_message}"
    );

    // Same invariant applied to `failure_detail_json` and `error_code` — a
    // future refactor that pipes raw upstream text into the JSON detail or the
    // error code must be caught here.
    assert!(
        !detail.contains("503 Service Unavailable")
            && !detail.contains("api.openai.com")
            && !detail.contains("sk-secret"),
        "raw upstream text leaked into failure_detail_json: {detail}"
    );
    let error_code = terminal.error_code.as_deref().expect("error_code present");
    assert!(
        !error_code.contains("api.openai.com") && !error_code.contains("sk-secret"),
        "raw upstream text leaked into error_code: {error_code}"
    );

    // A session-scoped event with kind `prompt.inference_failed` must exist
    // for this session and carry the structured payload.
    let state = harness.state.lock().await;
    let events = state
        .query_session_events(&session_id, None, 100)
        .expect("session events");
    drop(state);
    let inference_event = events
        .iter()
        .find(|event| event.kind == "prompt.inference_failed")
        .expect("prompt.inference_failed event present");
    let payload_value: Value =
        serde_json::from_str(&inference_event.payload_json).expect("event payload json");
    assert_eq!(payload_value["status_code"], 503);
    assert_eq!(payload_value["reason_category"], "service_unavailable");
    assert_eq!(payload_value["prompt_id"], prompt_id);
    // And neither the message nor the payload should leak the URL/secret.
    assert!(!inference_event.message.contains("openai"));
    assert!(!inference_event.message.contains("sk-secret"));
    assert!(!inference_event.payload_json.contains("openai"));
    assert!(!inference_event.payload_json.contains("sk-secret"));
}

#[tokio::test]
async fn stalled_prompt_suppresses_late_terminal_failure_event() {
    const DELAY_MS: u64 = 1000;
    let injected_message = "upstream returned 503 Service Unavailable";
    let harness = Harness::spawn_with(|config| {
        config.agent.args.extend([
            "--prompt-inference-error-after-update".into(),
            injected_message.into(),
            "--prompt-response-delay-ms".into(),
            DELAY_MS.to_string(),
        ]);
    })
    .await;
    let session_id = create_session(&harness).await;

    let submit: Value = http()
        .post(format!(
            "{}/v1/sessions/{}/prompt",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .json(&json!({ "prompt": "race a stalled prompt" }))
        .send()
        .await
        .expect("submit")
        .json()
        .await
        .expect("submit json");
    let prompt_id = submit["data"]["prompt_id"]
        .as_str()
        .expect("prompt id")
        .to_owned();

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if std::time::Instant::now() > deadline {
            panic!("prompt never reached running");
        }
        let state = harness.state.lock().await;
        let status = state
            .get_prompt(&prompt_id)
            .expect("prompt lookup")
            .map(|record| record.status);
        drop(state);
        if status.as_deref() == Some("running") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    {
        let state = harness.state.lock().await;
        let stalled = state
            .mark_stalled_prompts(Duration::from_secs(0), "test forced stall")
            .expect("mark stalled");
        assert!(
            stalled.iter().any(|(id, _)| id == &prompt_id),
            "forced stall should include submitted prompt, got {stalled:?}"
        );
    }

    tokio::time::sleep(Duration::from_millis(DELAY_MS + 250)).await;

    let state = harness.state.lock().await;
    let prompt = state
        .get_prompt(&prompt_id)
        .expect("prompt lookup")
        .expect("prompt exists");
    assert_eq!(prompt.status, "stalled");
    assert_eq!(
        prompt.failure_class.as_deref(),
        Some(acp_stack::state::FailureClass::Stalled.as_str())
    );
    let events = state
        .query_session_events(&session_id, None, 100)
        .expect("session events");
    drop(state);
    assert!(
        events
            .iter()
            .all(|event| event.kind != "prompt.inference_failed" && event.kind != "prompt.errored"),
        "late terminal failure event should be suppressed after stalled transition, got {events:?}"
    );
}

async fn recv_matching_event(
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
