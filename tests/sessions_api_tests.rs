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
use acp_stack::config::{Config, load_config_from_str};
use acp_stack::state::{NewPromptRecord, NewSessionRecord, StateStore};
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
        let config_path = tempdir.path().join("acp-stack.toml");
        std::fs::write(
            &config_path,
            config.to_canonical_toml().expect("canonical test config"),
        )
        .expect("test config write");
        let effective_bind = config.api.bind.clone();
        let runtime_paths = RuntimePaths::new(config_path, path);
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
    assert_eq!(metadata["agent_meta"]["origin"], "placebo-agent");
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
                    agent_id: "fake".to_owned(),
                    cwd: "/tmp/old".to_owned(),
                    title: None,
                    updated_at: Some("2026-01-01T00:00:00Z".to_owned()),
                    metadata_json: "{}".to_owned(),
                },
                acp_stack::state::ListedSessionRecord {
                    id: "sess_mid".to_owned(),
                    agent_id: "fake".to_owned(),
                    cwd: "/tmp/mid".to_owned(),
                    title: None,
                    updated_at: Some("2026-02-01T00:00:00Z".to_owned()),
                    metadata_json: "{}".to_owned(),
                },
                acp_stack::state::ListedSessionRecord {
                    id: "sess_new".to_owned(),
                    agent_id: "fake".to_owned(),
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
                    agent_id: "fake".to_owned(),
                    cwd: "/tmp/first".to_owned(),
                    title: None,
                    updated_at: Some("2026-02-01T00:00:00Z".to_owned()),
                    metadata_json: "{}".to_owned(),
                },
                acp_stack::state::ListedSessionRecord {
                    id: "sess_latest".to_owned(),
                    agent_id: "fake".to_owned(),
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
                agent_id: "fake".to_owned(),
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
                agent_id: "fake".to_owned(),
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
    let session = &body["data"]["sessions"][0];
    assert_eq!(session["id"], "sess_active");
    assert_eq!(session["last_activity_from"], "agent");
    assert_eq!(session["recent"], true);
    assert!(session.get("metadata_json").is_none());
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
                agent_id: "fake".to_owned(),
                cwd: "/tmp/active".to_owned(),
                title: None,
                metadata_json: "{}".to_owned(),
            })
            .expect("session inserted");
        store
            .upsert_listed_sessions(vec![acp_stack::state::ListedSessionRecord {
                id: "sess_active".to_owned(),
                agent_id: "fake".to_owned(),
                cwd: "/tmp/active".to_owned(),
                title: None,
                updated_at: Some("2026-01-01T00:00:00Z".to_owned()),
                metadata_json: "{}".to_owned(),
            }])
            .expect("session updated");
    }

    let body: Value = http()
        .get(format!(
            "{}/v1/sessions/-/status?threshold=1s",
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
                agent_id: "fake".to_owned(),
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
                agent_id: "fake".to_owned(),
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
