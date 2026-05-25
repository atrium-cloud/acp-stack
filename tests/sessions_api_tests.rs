//! End-to-end coverage for the session HTTP routes: create, list, get,
//! prompt (fire-and-forget + polling), cancel, load, close, plus auth-tier
//! enforcement and `session/update` persistence.
//!
//! The fake-agent gate in the `acps` binary stands in for a real ACP agent;
//! `tests/acp_bridge_tests.rs` exercises the lower-level bridge layer.

use std::sync::Arc;
use std::time::Duration;

use acp_stack::api::{self, AppState};
use acp_stack::config::{Config, load_config_from_str};
use acp_stack::state::StateStore;
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
    _tempdir: TempDir,
    state: Arc<TokioMutex<StateStore>>,
    join: JoinHandle<acp_stack::error::Result<()>>,
}

impl Harness {
    async fn spawn() -> Self {
        Self::spawn_with(|_| {}).await
    }

    async fn spawn_with(mutate: impl FnOnce(&mut Config)) -> Self {
        let tempdir = TempDir::new().expect("tempdir");
        let path = tempdir.path().join("state.sqlite");
        let store = StateStore::open(&path).expect("state open");
        store.migrate().expect("migrate");
        let mut config = test_config();
        mutate(&mut config);
        let app_state = AppState::new(config, store, SESSION_KEY.to_owned(), ADMIN_KEY.to_owned());
        let state = app_state.state.clone();
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let base_url = format!("http://{}", listener.local_addr().expect("local"));
        let join = tokio::spawn(async move { api::serve(app_state, listener).await });
        let harness = Self {
            base_url,
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
    let toml_text = include_str!("fixtures/valid-acp-stack.toml");
    let mut config = load_config_from_str(toml_text).expect("config parses");
    config.agent.command = env!("CARGO_BIN_EXE_acps").to_owned();
    config.agent.args = vec!["__acps-test-fake-agent".into()];
    config.agent.env = vec![];
    config.agent.cwd = Some(std::env::temp_dir().to_string_lossy().into_owned());
    config.agent.expected_sha256 = None;
    config
}

fn http() -> reqwest::Client {
    reqwest::Client::builder().build().expect("client")
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
        .find(|session| session["id"] == "sess_listed_0")
        .expect("listed session present");
    assert_eq!(listed["status"], "available");
    assert_eq!(listed["title"], "listed session");
    let metadata: Value =
        serde_json::from_str(listed["metadata_json"].as_str().unwrap()).expect("metadata json");
    assert_eq!(metadata["agent_meta"]["origin"], "fake-agent");
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
async fn available_session_must_be_loaded_before_prompting() {
    let harness = Harness::spawn().await;
    let client = http();
    let _: Value = client
        .get(format!("{}/v1/sessions", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("list")
        .json()
        .await
        .expect("list json");

    let response = client
        .post(format!(
            "{}/v1/sessions/{}/prompt",
            harness.base_url, "sess_listed_0"
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
