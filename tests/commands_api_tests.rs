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

const SESSION_KEY: &str = "acps_session_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const ADMIN_KEY: &str = "acps_admin_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

struct Harness {
    base_url: String,
    workspace_root: PathBuf,
    _state: Arc<TokioMutex<StateStore>>,
    join: JoinHandle<acp_stack::error::Result<()>>,
    _workspace_tempdir: TempDir,
    _state_tempdir: TempDir,
}

struct HarnessOverrides {
    permissions: Option<PermissionsConfig>,
    commands: Option<CommandsConfig>,
}

impl HarnessOverrides {
    fn none() -> Self {
        Self {
            permissions: None,
            commands: None,
        }
    }
}

impl Harness {
    async fn spawn() -> Self {
        Self::spawn_with(HarnessOverrides::none()).await
    }

    async fn spawn_with(overrides: HarnessOverrides) -> Self {
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

fn test_config() -> Config {
    let toml_text = include_str!("fixtures/valid-acp-stack.toml");
    load_config_from_str(toml_text).expect("config parses")
}

fn session_client() -> reqwest::Client {
    reqwest::Client::new()
}

fn auth(req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    req.header(
        reqwest::header::AUTHORIZATION,
        format!("Bearer {SESSION_KEY}"),
    )
}

fn admin_auth(req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    req.header(
        reqwest::header::AUTHORIZATION,
        format!("Bearer {ADMIN_KEY}"),
    )
}

async fn submit(harness: &Harness, body: Value) -> reqwest::Response {
    auth(session_client().post(format!("{}/v1/commands", harness.base_url)))
        .json(&body)
        .send()
        .await
        .expect("send")
}

/// Drive `GET /v1/commands/{id}` until the row reaches a terminal status
/// (anything other than `pending` / `running`). Bounded loop so a regression
/// surfaces as a deterministic timeout rather than a hung test.
async fn wait_for_terminal(harness: &Harness, id: &str) -> Value {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let response =
            auth(session_client().get(format!("{}/v1/commands/{}", harness.base_url, id)))
                .send()
                .await
                .expect("send");
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

#[tokio::test]
async fn submit_runs_command_and_records_exit_status() {
    let harness = Harness::spawn().await;
    let response = submit(&harness, serde_json::json!({"command": "echo hello"})).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    let id = body["data"]["id"].as_str().expect("id").to_owned();

    let final_body = wait_for_terminal(&harness, &id).await;
    assert_eq!(final_body["data"]["status"], "exited");
    assert_eq!(final_body["data"]["exit_status"], 0);
    assert!(final_body["data"]["duration_ms"].as_i64().unwrap() >= 0);
}

#[tokio::test]
async fn submit_records_failure_status_for_nonzero_exit() {
    let harness = Harness::spawn().await;
    let response = submit(&harness, serde_json::json!({"command": "exit 7"})).await;
    let body: Value = response.json().await.expect("json");
    let id = body["data"]["id"].as_str().unwrap().to_owned();
    let final_body = wait_for_terminal(&harness, &id).await;
    assert_eq!(final_body["data"]["status"], "failed");
    assert_eq!(final_body["data"]["exit_status"], 7);
}

#[tokio::test]
async fn deny_pattern_rejects_submission() {
    let permissions = PermissionsConfig {
        mode: "auto".to_owned(),
        review: vec![],
        deny: vec!["rm *".to_owned()],
        ..PermissionsConfig::default()
    };
    let harness = Harness::spawn_with(HarnessOverrides {
        permissions: Some(permissions),
        commands: None,
    })
    .await;
    let response = submit(&harness, serde_json::json!({"command": "rm -rf /"})).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "command.denied");
}

#[tokio::test]
async fn review_pattern_enqueues_permission_in_supervised_mode() {
    let permissions = PermissionsConfig {
        mode: "supervised".to_owned(),
        review: vec!["sudo *".to_owned()],
        deny: vec![],
        ..PermissionsConfig::default()
    };
    let harness = Harness::spawn_with(HarnessOverrides {
        permissions: Some(permissions),
        commands: None,
    })
    .await;
    let response = submit(&harness, serde_json::json!({"command": "sudo apt update"})).await;
    // Row is created in pending state; permission decision lands out-of-band.
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["data"]["status"], "pending");
    let cmd_id = body["data"]["id"].as_str().unwrap().to_owned();

    let pending =
        auth(session_client().get(format!("{}/v1/permissions/pending", harness.base_url)))
            .send()
            .await
            .expect("send");
    assert_eq!(pending.status(), StatusCode::OK);
    let pending_body: Value = pending.json().await.expect("json");
    let permissions_list = pending_body["data"]["permissions"]
        .as_array()
        .expect("permissions array");
    let entry = permissions_list
        .iter()
        .find(|p| p["subject_id"].as_str() == Some(&cmd_id))
        .expect("pending permission row for command");
    let perm_id = entry["id"].as_str().unwrap().to_owned();

    let deny_response = auth(session_client().post(format!(
        "{}/v1/permissions/{}/deny",
        harness.base_url, perm_id
    )))
    .json(&serde_json::json!({}))
    .send()
    .await
    .expect("send");
    assert_eq!(deny_response.status(), StatusCode::OK);

    let final_body = wait_for_terminal(&harness, &cmd_id).await;
    assert_eq!(final_body["data"]["status"], "failed");
    assert_eq!(final_body["data"]["exit_status"], Value::Null);
}

#[tokio::test]
async fn locked_mode_enqueues_permission_and_approval_runs() {
    let permissions = PermissionsConfig {
        mode: "locked".to_owned(),
        review: vec![],
        deny: vec![],
        ..PermissionsConfig::default()
    };
    let harness = Harness::spawn_with(HarnessOverrides {
        permissions: Some(permissions),
        commands: None,
    })
    .await;
    let response = submit(&harness, serde_json::json!({"command": "echo hi"})).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["data"]["status"], "pending");
    let cmd_id = body["data"]["id"].as_str().unwrap().to_owned();

    let pending =
        auth(session_client().get(format!("{}/v1/permissions/pending", harness.base_url)))
            .send()
            .await
            .expect("send");
    let pending_body: Value = pending.json().await.expect("json");
    let permissions_list = pending_body["data"]["permissions"]
        .as_array()
        .expect("permissions array");
    let perm_id = permissions_list
        .iter()
        .find(|p| p["subject_id"].as_str() == Some(&cmd_id))
        .expect("permission row")["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let approve_response = auth(session_client().post(format!(
        "{}/v1/permissions/{}/approve",
        harness.base_url, perm_id
    )))
    .json(&serde_json::json!({}))
    .send()
    .await
    .expect("send");
    assert_eq!(approve_response.status(), StatusCode::OK);

    let final_body = wait_for_terminal(&harness, &cmd_id).await;
    assert_eq!(final_body["data"]["status"], "exited");
    assert_eq!(final_body["data"]["exit_status"], 0);

    // GET /v1/logs/permissions must surface the durable permission.* events
    // generated for this command's lifecycle (created + approved). Without
    // this assertion, a regression in PermissionService event persistence
    // would silently leave the log route returning an empty array.
    let logs = auth(session_client().get(format!("{}/v1/logs/permissions", harness.base_url)))
        .send()
        .await
        .expect("send");
    assert_eq!(logs.status(), StatusCode::OK);
    let logs_body: Value = logs.json().await.expect("json");
    let kinds: Vec<&str> = logs_body["data"]["events"]
        .as_array()
        .expect("events array")
        .iter()
        .filter_map(|e| e["kind"].as_str())
        .collect();
    assert!(
        kinds.contains(&"permission.created"),
        "expected permission.created event, saw: {kinds:?}",
    );
    assert!(
        kinds.contains(&"permission.approved"),
        "expected permission.approved event, saw: {kinds:?}",
    );
}

#[tokio::test]
async fn review_supervised_mode_approve_runs_command() {
    // Quadrant: supervised + review-match + APPROVE → command transitions
    // to running and exits cleanly. Complements the existing supervised-deny
    // and locked-approve tests so all four review/locked outcomes are
    // covered end-to-end.
    let permissions = PermissionsConfig {
        mode: "supervised".to_owned(),
        review: vec!["echo *".to_owned()],
        deny: vec![],
        ..PermissionsConfig::default()
    };
    let harness = Harness::spawn_with(HarnessOverrides {
        permissions: Some(permissions),
        commands: None,
    })
    .await;
    let response = submit(&harness, serde_json::json!({"command": "echo allowed"})).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["data"]["status"], "pending");
    let cmd_id = body["data"]["id"].as_str().unwrap().to_owned();

    let pending =
        auth(session_client().get(format!("{}/v1/permissions/pending", harness.base_url)))
            .send()
            .await
            .expect("send");
    let pending_body: Value = pending.json().await.expect("json");
    let perm_id = pending_body["data"]["permissions"]
        .as_array()
        .expect("permissions array")
        .iter()
        .find(|p| p["subject_id"].as_str() == Some(&cmd_id))
        .expect("permission row")["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let approve_response = auth(session_client().post(format!(
        "{}/v1/permissions/{}/approve",
        harness.base_url, perm_id
    )))
    .json(&serde_json::json!({}))
    .send()
    .await
    .expect("send");
    assert_eq!(approve_response.status(), StatusCode::OK);

    let final_body = wait_for_terminal(&harness, &cmd_id).await;
    assert_eq!(final_body["data"]["status"], "exited");
    assert_eq!(final_body["data"]["exit_status"], 0);
}

#[tokio::test]
async fn locked_mode_deny_marks_command_failed() {
    // Quadrant: locked + DENY → command transitions to failed without ever
    // spawning a child. Completes the four-quadrant policy matrix.
    let permissions = PermissionsConfig {
        mode: "locked".to_owned(),
        review: vec![],
        deny: vec![],
        ..PermissionsConfig::default()
    };
    let harness = Harness::spawn_with(HarnessOverrides {
        permissions: Some(permissions),
        commands: None,
    })
    .await;
    let response = submit(&harness, serde_json::json!({"command": "echo blocked"})).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    let cmd_id = body["data"]["id"].as_str().unwrap().to_owned();

    let pending =
        auth(session_client().get(format!("{}/v1/permissions/pending", harness.base_url)))
            .send()
            .await
            .expect("send");
    let pending_body: Value = pending.json().await.expect("json");
    let perm_id = pending_body["data"]["permissions"]
        .as_array()
        .expect("permissions array")
        .iter()
        .find(|p| p["subject_id"].as_str() == Some(&cmd_id))
        .expect("permission row")["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let deny_response = auth(session_client().post(format!(
        "{}/v1/permissions/{}/deny",
        harness.base_url, perm_id
    )))
    .json(&serde_json::json!({}))
    .send()
    .await
    .expect("send");
    assert_eq!(deny_response.status(), StatusCode::OK);

    let final_body = wait_for_terminal(&harness, &cmd_id).await;
    assert_eq!(final_body["data"]["status"], "failed");
    // exit_status is null because the child never ran.
    assert_eq!(final_body["data"]["exit_status"], Value::Null);

    // GET /v1/logs/permissions must surface permission.denied for this row.
    let logs = auth(session_client().get(format!("{}/v1/logs/permissions", harness.base_url)))
        .send()
        .await
        .expect("send");
    let logs_body: Value = logs.json().await.expect("json");
    let kinds: Vec<&str> = logs_body["data"]["events"]
        .as_array()
        .expect("events array")
        .iter()
        .filter_map(|e| e["kind"].as_str())
        .collect();
    assert!(
        kinds.contains(&"permission.denied"),
        "expected permission.denied event, saw: {kinds:?}",
    );
}

#[tokio::test]
async fn review_pattern_allowed_in_auto_mode() {
    let permissions = PermissionsConfig {
        mode: "auto".to_owned(),
        review: vec!["echo *".to_owned()],
        deny: vec![],
        ..PermissionsConfig::default()
    };
    let harness = Harness::spawn_with(HarnessOverrides {
        permissions: Some(permissions),
        commands: None,
    })
    .await;
    let response = submit(&harness, serde_json::json!({"command": "echo flagged"})).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    let id = body["data"]["id"].as_str().unwrap().to_owned();
    let final_body = wait_for_terminal(&harness, &id).await;
    assert_eq!(final_body["data"]["status"], "exited");
}

#[tokio::test]
async fn env_not_on_allowlist_rejected() {
    let commands = CommandsConfig {
        default_timeout: "10m".to_owned(),
        cancel_grace: "5s".to_owned(),
        env_allowlist: vec!["FOO".to_owned()],
        max_output_bytes: 1_048_576,
    };
    let harness = Harness::spawn_with(HarnessOverrides {
        permissions: None,
        commands: Some(commands),
    })
    .await;
    let response = submit(
        &harness,
        serde_json::json!({"command": "echo $BAR", "env": {"BAR": "x"}}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "command.env_not_allowed");
}

#[tokio::test]
async fn env_on_allowlist_reaches_child() {
    let commands = CommandsConfig {
        default_timeout: "10m".to_owned(),
        cancel_grace: "5s".to_owned(),
        env_allowlist: vec!["GREETING".to_owned()],
        max_output_bytes: 1_048_576,
    };
    let harness = Harness::spawn_with(HarnessOverrides {
        permissions: None,
        commands: Some(commands),
    })
    .await;
    let response = submit(
        &harness,
        serde_json::json!({"command": "printf %s \"$GREETING\"", "env": {"GREETING": "hi"}}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    let id = body["data"]["id"].as_str().unwrap().to_owned();
    let final_body = wait_for_terminal(&harness, &id).await;
    assert_eq!(final_body["data"]["status"], "exited");
}

#[tokio::test]
async fn cwd_outside_workspace_rejected() {
    let harness = Harness::spawn().await;
    let response = submit(
        &harness,
        serde_json::json!({"command": "echo", "cwd": "/etc"}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "command.cwd_outside_workspace");
}

#[tokio::test]
async fn cwd_relative_under_workspace_accepted() {
    let harness = Harness::spawn().await;
    std::fs::create_dir(harness.workspace_root.join("inner")).expect("inner dir");
    let response = submit(
        &harness,
        serde_json::json!({"command": "pwd", "cwd": "inner"}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    let id = body["data"]["id"].as_str().unwrap().to_owned();
    let final_body = wait_for_terminal(&harness, &id).await;
    assert_eq!(final_body["data"]["status"], "exited");
}

#[tokio::test]
async fn cancel_transitions_running_command_to_canceled() {
    let harness = Harness::spawn().await;
    let response = submit(&harness, serde_json::json!({"command": "sleep 30"})).await;
    let body: Value = response.json().await.expect("json");
    let id = body["data"]["id"].as_str().unwrap().to_owned();

    // Give the supervisor a moment to mark the row running before we cancel.
    tokio::time::sleep(Duration::from_millis(150)).await;

    let cancel =
        auth(session_client().post(format!("{}/v1/commands/{}/cancel", harness.base_url, id)))
            .send()
            .await
            .expect("send");
    assert_eq!(cancel.status(), StatusCode::OK);

    let final_body = wait_for_terminal(&harness, &id).await;
    assert_eq!(final_body["data"]["status"], "canceled");
}

#[tokio::test]
async fn timeout_marks_failed_status() {
    let commands = CommandsConfig {
        default_timeout: "300ms".to_owned(),
        cancel_grace: "200ms".to_owned(),
        env_allowlist: vec![],
        max_output_bytes: 1_048_576,
    };
    let harness = Harness::spawn_with(HarnessOverrides {
        permissions: None,
        commands: Some(commands),
    })
    .await;
    let response = submit(&harness, serde_json::json!({"command": "sleep 30"})).await;
    let body: Value = response.json().await.expect("json");
    let id = body["data"]["id"].as_str().unwrap().to_owned();
    let final_body = wait_for_terminal(&harness, &id).await;
    assert_eq!(final_body["data"]["status"], "failed");
}

#[tokio::test]
async fn output_truncation_marks_truncated_flag() {
    let commands = CommandsConfig {
        default_timeout: "10m".to_owned(),
        cancel_grace: "5s".to_owned(),
        env_allowlist: vec![],
        max_output_bytes: 16,
    };
    let harness = Harness::spawn_with(HarnessOverrides {
        permissions: None,
        commands: Some(commands),
    })
    .await;
    let response = submit(
        &harness,
        serde_json::json!({"command": "printf 'ABCDEFGHIJ' && printf 'KLMNOPQRSTUVWXYZ12345'"}),
    )
    .await;
    let body: Value = response.json().await.expect("json");
    let id = body["data"]["id"].as_str().unwrap().to_owned();
    let final_body = wait_for_terminal(&harness, &id).await;
    assert_eq!(final_body["data"]["truncated"], true);
}

#[tokio::test]
async fn admin_key_rejected_on_session_route() {
    let harness = Harness::spawn().await;
    let response = admin_auth(session_client().post(format!("{}/v1/commands", harness.base_url)))
        .json(&serde_json::json!({"command": "echo"}))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn get_returns_not_found_for_unknown_id() {
    let harness = Harness::spawn().await;
    let response = auth(session_client().get(format!(
        "{}/v1/commands/cmd_does_not_exist",
        harness.base_url
    )))
    .send()
    .await
    .expect("send");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "command.not_found");
}

#[tokio::test]
async fn list_returns_recent_commands() {
    let harness = Harness::spawn().await;
    for command in ["echo a", "echo b", "echo c"] {
        let response = submit(&harness, serde_json::json!({"command": command})).await;
        assert_eq!(response.status(), StatusCode::OK);
    }
    // Wait for all three to finish so list order is stable.
    for _ in 0..30 {
        let response = auth(session_client().get(format!("{}/v1/commands", harness.base_url)))
            .send()
            .await
            .expect("send");
        let body: Value = response.json().await.expect("json");
        let items = body["data"]["items"].as_array().expect("items");
        if items.iter().all(|item| {
            let status = item["status"].as_str().unwrap_or("");
            status != "pending" && status != "running"
        }) && items.len() == 3
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("commands did not finish in time");
}

// ----- WebSocket -----------------------------------------------------------

async fn open_ws(
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

async fn collect_until<S>(stream: &mut S, predicate: impl Fn(&Value) -> bool) -> Vec<Value>
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

#[tokio::test]
async fn websocket_streams_command_stdout_and_exit() {
    let harness = Harness::spawn().await;
    // `commands.{id}` is per-row, so the id has to exist before subscribing.
    // We slow the command itself with `sleep 0.3` so the supervisor's events
    // (`command.started`, stdout chunk from `echo`, `command.exited`) fire
    // AFTER the WebSocket subscription is registered.
    let response = submit(
        &harness,
        serde_json::json!({"command": "sleep 0.3 && echo streamed"}),
    )
    .await;
    let body: Value = response.json().await.expect("json");
    let id = body["data"]["id"].as_str().unwrap().to_owned();
    let topic = format!("commands.{id}");
    let mut stream = open_ws(&harness.base_url, &[topic.as_str()]).await;
    // Subscribe is async; give the server a brief moment to register the
    // topic before the supervisor begins emitting.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let _final_body = wait_for_terminal(&harness, &id).await;
    let events = collect_until(&mut stream, |value| {
        value["payload"]["kind"]
            .as_str()
            .map(|kind| kind == "command.exited" || kind == "command.failed")
            .unwrap_or(false)
    })
    .await;
    let kinds: Vec<&str> = events
        .iter()
        .filter_map(|event| event["payload"]["kind"].as_str())
        .collect();
    assert!(
        kinds.contains(&"command.exited") && kinds.contains(&"command.stdout"),
        "expected both command.stdout and command.exited, got: {kinds:?}"
    );
}

#[tokio::test]
async fn websocket_logs_topic_receives_every_event() {
    let harness = Harness::spawn().await;
    let mut stream = open_ws(&harness.base_url, &["logs"]).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let response = submit(&harness, serde_json::json!({"command": "echo log"})).await;
    let body: Value = response.json().await.expect("json");
    let id = body["data"]["id"].as_str().unwrap().to_owned();
    let _final_body = wait_for_terminal(&harness, &id).await;
    let events = collect_until(&mut stream, |value| {
        value["payload"]["kind"]
            .as_str()
            .map(|kind| kind == "command.exited")
            .unwrap_or(false)
    })
    .await;
    let topics: Vec<&str> = events
        .iter()
        .filter_map(|event| event["topic"].as_str())
        .collect();
    assert!(
        topics.contains(&"logs"),
        "expected at least one logs event, saw {topics:?}"
    );
}
