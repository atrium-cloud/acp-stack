#![cfg(all(feature = "dev-tools", feature = "test-fixtures"))]

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use acp_stack::config::Config;
use acp_stack::secrets::SecretStore;
use assert_cmd::Command;
use futures::{SinkExt, StreamExt};
use reqwest::StatusCode;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const ACCEPTANCE_KEY_REF: &str = "ACCEPTANCE_API_KEY";
const ACCEPTANCE_KEY_VALUE: &str = "fixture-secret-value";
const SERVE_READY_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_TIMEOUT: Duration = Duration::from_secs(10);

struct ServeProcess {
    child: Child,
}

impl Drop for ServeProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn acps_command(home: &Path) -> Command {
    let mut command = Command::cargo_bin("acps").expect("acps binary");
    command
        .env("HOME", home)
        .env(
            "ACP_STACK_DEV_PLACEBO_REGISTRY",
            env!("CARGO_BIN_EXE_placebo-agent"),
        )
        .env("NO_COLOR", "1");
    command
}

fn config_path(home: &Path) -> PathBuf {
    home.join(".config/acp-stack/acps-config.toml")
}

fn run_fixture_init(home: &Path, workspace: &Path) {
    std::fs::create_dir_all(workspace.join("uploads")).expect("workspace dirs");
    acps_command(home)
        .args([
            "init",
            "--agent",
            "placebo",
            "--no-skills",
            "--workspace-root",
            workspace.to_str().expect("workspace path is UTF-8"),
            "--workspace-uploads",
            workspace
                .join("uploads")
                .to_str()
                .expect("uploads path is UTF-8"),
            "--skip-testflight",
        ])
        .assert()
        .success();
}

fn set_secret(home: &Path, name: &str, value: &str) {
    acps_command(home)
        .args(["secrets", "set", name])
        .write_stdin(format!("{value}\n"))
        .assert()
        .success();
}

fn assert_plaintext_not_written(root: &Path, needle: &str) {
    let needle = needle.as_bytes();
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        let metadata = std::fs::symlink_metadata(&path).expect("metadata");
        if metadata.is_dir() {
            for entry in std::fs::read_dir(&path).expect("read dir") {
                stack.push(entry.expect("dir entry").path());
            }
        } else if metadata.is_file() {
            let bytes = std::fs::read(&path).expect("read file");
            assert!(
                !bytes.windows(needle.len()).any(|window| window == needle),
                "plaintext secret was written to {}",
                path.display()
            );
        }
    }
}

fn configure_fixture_secret_assertion(home: &Path) {
    let path = config_path(home);
    let mut config = Config::load_from_path(&path).expect("config loads");
    config.agent.env = vec![ACCEPTANCE_KEY_REF.to_owned()];
    config.agent.args.push("--assert-env-present".to_owned());
    config.agent.args.push(ACCEPTANCE_KEY_REF.to_owned());
    config.permissions.mode = "supervised".to_owned();
    config.permissions.review = vec!["echo *".to_owned()];
    config.workspace.default_shell = "/bin/sh".to_owned();
    let canonical = config.to_canonical_toml().expect("canonical TOML");
    std::fs::write(&path, canonical).expect("write acceptance config");
}

fn export_config(home: &Path, output: &Path) -> String {
    acps_command(home)
        .args([
            "config",
            "export",
            "--output",
            output.to_str().expect("export path is UTF-8"),
        ])
        .assert()
        .success();
    std::fs::read_to_string(output).expect("read exported config")
}

fn import_config_into_home(home: &Path, input: &Path) {
    acps_command(home)
        .args([
            "config",
            "import",
            input.to_str().expect("import path is UTF-8"),
        ])
        .assert()
        .success();
}

fn generated_keys(home: &Path) -> (String, String) {
    let config = Config::load_from_path(config_path(home)).expect("config loads");
    let store = SecretStore::open(home).expect("secret store opens");
    let session_key = store
        .get(&config.auth.session_key_ref)
        .expect("session key")
        .to_owned();
    let admin_key = store
        .get(&config.auth.admin_key_ref)
        .expect("admin key")
        .to_owned();
    (session_key, admin_key)
}

fn start_serve(home: &Path) -> (ServeProcess, String) {
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_acps"))
        .env("HOME", home)
        .env(
            "ACP_STACK_DEV_PLACEBO_REGISTRY",
            env!("CARGO_BIN_EXE_placebo-agent"),
        )
        .env("NO_COLOR", "1")
        .args(["serve", "--bind", "127.0.0.1:0"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn acps serve");

    let stderr = child.stderr.take().expect("serve stderr");
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let reader = BufReader::new(stderr);
        let mut lines = Vec::new();
        for line in reader.lines() {
            let line = match line {
                Ok(line) => line,
                Err(error) => {
                    let _ = sender.send(Err(format!("read serve stderr failed: {error}")));
                    return;
                }
            };
            if let Some(addr) = line.strip_prefix("acps serve: listening on ") {
                let _ = sender.send(Ok(format!("http://{addr}")));
                return;
            }
            lines.push(line);
        }
        let _ = sender.send(Err(format!(
            "serve exited before ready; stderr: {}",
            lines.join("\n")
        )));
    });

    let base_url = receiver
        .recv_timeout(SERVE_READY_TIMEOUT)
        .expect("serve ready line before timeout")
        .expect("serve starts");
    (ServeProcess { child }, base_url)
}

fn session_bearer(session_key: &str) -> String {
    format!("Bearer {session_key}")
}

fn admin_bearer(admin_key: &str) -> String {
    format!("Bearer {admin_key}")
}

async fn get_json(client: &reqwest::Client, url: String, bearer: &str) -> Value {
    let response = client
        .get(url)
        .header("Authorization", bearer)
        .send()
        .await
        .expect("GET request");
    assert_eq!(response.status(), StatusCode::OK);
    response.json().await.expect("GET JSON")
}

async fn post_json(client: &reqwest::Client, url: String, bearer: &str, body: Value) -> Value {
    let request_url = url.clone();
    let response = client
        .post(url)
        .header("Authorization", bearer)
        .json(&body)
        .send()
        .await
        .expect("POST request");
    let status = response.status();
    let response_body = response.text().await.expect("POST response body");
    assert_eq!(
        status,
        StatusCode::OK,
        "POST {request_url} returned {status}: {response_body}"
    );
    serde_json::from_str(&response_body).expect("POST JSON")
}

async fn put_json(client: &reqwest::Client, url: String, bearer: &str, body: Value) -> Value {
    let request_url = url.clone();
    let response = client
        .put(url)
        .header("Authorization", bearer)
        .json(&body)
        .send()
        .await
        .expect("PUT request");
    let status = response.status();
    let response_body = response.text().await.expect("PUT response body");
    assert_eq!(
        status,
        StatusCode::OK,
        "PUT {request_url} returned {status}: {response_body}"
    );
    serde_json::from_str(&response_body).expect("PUT JSON")
}

async fn wait_for_prompt(
    client: &reqwest::Client,
    base_url: &str,
    bearer: &str,
    session_id: &str,
    prompt_id: &str,
) -> Value {
    let deadline = tokio::time::Instant::now() + POLL_TIMEOUT;
    loop {
        let body = get_json(
            client,
            format!("{base_url}/v1/sessions/{session_id}/prompts/{prompt_id}"),
            bearer,
        )
        .await;
        let status = body["data"]["status"].as_str().unwrap_or("");
        if matches!(status, "completed" | "errored" | "cancelled" | "stalled") {
            return body;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "prompt did not settle: {body}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_for_command(
    client: &reqwest::Client,
    base_url: &str,
    bearer: &str,
    command_id: &str,
) -> Value {
    let deadline = tokio::time::Instant::now() + POLL_TIMEOUT;
    loop {
        let body = get_json(
            client,
            format!("{base_url}/v1/commands/{command_id}"),
            bearer,
        )
        .await;
        let status = body["data"]["status"].as_str().unwrap_or("");
        if status != "pending" && status != "running" {
            return body;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "command did not settle: {body}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn open_ws(
    base_url: &str,
    session_key: &str,
    topics: &[String],
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let ws_url = base_url.replacen("http://", "ws://", 1) + "/v1/ws";
    let mut request = ws_url.as_str().into_client_request().expect("ws request");
    request.headers_mut().insert(
        "Authorization",
        http::HeaderValue::from_str(&session_bearer(session_key)).expect("auth header"),
    );
    let (mut stream, response) = tokio_tungstenite::connect_async(request)
        .await
        .expect("ws connect");
    assert_eq!(response.status().as_u16(), 101);
    stream
        .send(Message::Text(
            json!({ "type": "subscribe", "topics": topics })
                .to_string()
                .into(),
        ))
        .await
        .expect("ws subscribe");
    stream
}

async fn wait_for_ws_topic(client: &reqwest::Client, base_url: &str, bearer: &str, topic: &str) {
    let deadline = tokio::time::Instant::now() + POLL_TIMEOUT;
    loop {
        let connections = get_json(client, format!("{base_url}/v1/ws/connections"), bearer).await;
        let subscribed = connections["data"]["connections"]
            .as_array()
            .expect("ws connections")
            .iter()
            .flat_map(|connection| {
                connection["topics"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|topic| topic.as_str())
            })
            .any(|registered| registered == topic);
        if subscribed {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "websocket topic never registered: {topic}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn receive_session_update(
    stream: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    topic: &str,
) -> Value {
    let deadline = tokio::time::Instant::now() + POLL_TIMEOUT;
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "session update websocket event did not arrive"
        );
        let Some(message) = tokio::time::timeout(Duration::from_secs(1), stream.next())
            .await
            .expect("ws receive tick")
        else {
            continue;
        };
        let message = message.expect("ws message");
        let Message::Text(text) = message else {
            continue;
        };
        let event: Value = serde_json::from_str(text.as_str()).expect("ws event JSON");
        if event["type"] == "event"
            && event["topic"] == topic
            && event["payload"]["kind"] == "session.update"
        {
            return event;
        }
    }
}

#[tokio::test]
async fn release_acceptance_fixture_first_test() {
    let home = tempfile::tempdir().expect("home tempdir");
    let imported_home = tempfile::tempdir().expect("import home tempdir");
    let workspace = home.path().join("workspace");

    acps_command(home.path())
        .arg("--version")
        .assert()
        .success();

    run_fixture_init(home.path(), &workspace);
    set_secret(home.path(), ACCEPTANCE_KEY_REF, ACCEPTANCE_KEY_VALUE);
    configure_fixture_secret_assertion(home.path());

    let export_path = home.path().join("exported-acps-config.toml");
    let exported = export_config(home.path(), &export_path);
    let (session_key, admin_key) = generated_keys(home.path());
    assert!(exported.contains(ACCEPTANCE_KEY_REF));
    assert!(!exported.contains(ACCEPTANCE_KEY_VALUE));
    assert!(!exported.contains(&session_key));
    assert!(!exported.contains(&admin_key));
    import_config_into_home(imported_home.path(), &export_path);
    assert_plaintext_not_written(home.path(), ACCEPTANCE_KEY_VALUE);

    let (_serve, base_url) = start_serve(home.path());
    let client = reqwest::Client::builder().build().expect("HTTP client");
    let session_auth = session_bearer(&session_key);
    let admin_auth = admin_bearer(&admin_key);

    let status = get_json(&client, format!("{base_url}/v1/status"), &session_auth).await;
    assert_eq!(status["ok"], true);
    post_json(
        &client,
        format!("{base_url}/v1/agent/start"),
        &admin_auth,
        json!({}),
    )
    .await;

    let session = post_json(
        &client,
        format!("{base_url}/v1/sessions"),
        &session_auth,
        json!({}),
    )
    .await;
    let session_id = session["data"]["id"]
        .as_str()
        .expect("session id")
        .to_owned();

    let capabilities = get_json(
        &client,
        format!("{base_url}/v1/agent/capabilities"),
        &session_auth,
    )
    .await;
    assert_eq!(
        capabilities["data"]["capabilities"]["agent_title"],
        "env assertions passed"
    );

    let topic = format!("sessions.{session_id}");
    let mut ws = open_ws(&base_url, &session_key, std::slice::from_ref(&topic)).await;
    wait_for_ws_topic(&client, &base_url, &session_auth, &topic).await;
    let prompt = post_json(
        &client,
        format!("{base_url}/v1/sessions/{session_id}/prompt"),
        &session_auth,
        json!({ "prompt": "fixture acceptance prompt" }),
    )
    .await;
    let prompt_id = prompt["data"]["prompt_id"].as_str().expect("prompt id");
    let update = receive_session_update(&mut ws, &topic).await;
    assert!(
        update["payload"].to_string().contains("chunk-"),
        "session update payload: {update}"
    );
    let prompt_status =
        wait_for_prompt(&client, &base_url, &session_auth, &session_id, prompt_id).await;
    assert_eq!(prompt_status["data"]["status"], "completed");

    let workspace_meta = get_json(&client, format!("{base_url}/v1/workspace"), &session_auth).await;
    assert_eq!(
        workspace_meta["data"]["root"],
        workspace.to_string_lossy().as_ref()
    );
    std::fs::create_dir_all(workspace.join("notes")).expect("notes directory");
    put_json(
        &client,
        format!("{base_url}/v1/files/content"),
        &session_auth,
        json!({
            "path": "notes/acceptance.txt",
            "encoding": "utf8",
            "content": "workspace acceptance"
        }),
    )
    .await;
    let read_back = get_json(
        &client,
        format!("{base_url}/v1/files/content?path=notes/acceptance.txt"),
        &session_auth,
    )
    .await;
    assert_eq!(read_back["data"]["content"], "workspace acceptance");
    let listing = get_json(
        &client,
        format!("{base_url}/v1/files?path=notes"),
        &session_auth,
    )
    .await;
    assert!(
        listing["data"]["entries"]
            .as_array()
            .expect("entries")
            .iter()
            .any(|entry| entry["name"] == "acceptance.txt")
    );
    let upload = client
        .post(format!("{base_url}/v1/files/upload"))
        .header("Authorization", &session_auth)
        .multipart(
            reqwest::multipart::Form::new()
                .text("path", "uploaded.txt")
                .part(
                    "file",
                    reqwest::multipart::Part::bytes("uploaded acceptance".as_bytes().to_vec())
                        .file_name("uploaded.txt"),
                ),
        )
        .send()
        .await
        .expect("upload");
    assert_eq!(upload.status(), StatusCode::OK);
    let upload_body: Value = upload.json().await.expect("upload JSON");
    assert_eq!(upload_body["data"]["path"], "uploads/uploaded.txt");
    let download = client
        .get(format!(
            "{base_url}/v1/files/download?path=uploads/uploaded.txt"
        ))
        .header("Authorization", &session_auth)
        .send()
        .await
        .expect("download");
    assert_eq!(download.status(), StatusCode::OK);
    let downloaded = download.bytes().await.expect("download body");
    assert_eq!(downloaded.as_ref(), b"uploaded acceptance");

    let command = post_json(
        &client,
        format!("{base_url}/v1/commands"),
        &session_auth,
        json!({ "command": "pwd" }),
    )
    .await;
    let command_id = command["data"]["id"].as_str().expect("command id");
    let command_final = wait_for_command(&client, &base_url, &session_auth, command_id).await;
    assert_eq!(command_final["data"]["status"], "exited");
    assert_eq!(command_final["data"]["exit_status"], 0);

    let gated = post_json(
        &client,
        format!("{base_url}/v1/commands"),
        &session_auth,
        json!({ "command": "echo permission-gated" }),
    )
    .await;
    assert_eq!(gated["data"]["status"], "pending");
    let gated_id = gated["data"]["id"].as_str().expect("gated command id");
    let pending = get_json(
        &client,
        format!("{base_url}/v1/permissions/pending"),
        &session_auth,
    )
    .await;
    let permission_id = pending["data"]["permissions"]
        .as_array()
        .expect("permissions")
        .iter()
        .find(|permission| permission["subject_id"].as_str() == Some(gated_id))
        .expect("pending permission")["id"]
        .as_str()
        .expect("permission id")
        .to_owned();
    post_json(
        &client,
        format!("{base_url}/v1/permissions/{permission_id}/approve"),
        &session_auth,
        json!({}),
    )
    .await;
    let gated_final = wait_for_command(&client, &base_url, &session_auth, gated_id).await;
    assert_eq!(gated_final["data"]["status"], "exited");

    for (route, key) in [
        ("/v1/logs/sessions?limit=20", "sessions"),
        ("/v1/logs/events?limit=20", "events"),
        ("/v1/logs/commands?limit=20", "commands"),
        ("/v1/logs/permissions?limit=20", "events"),
    ] {
        let logs = get_json(&client, format!("{base_url}{route}"), &session_auth).await;
        assert!(
            !logs["data"][key].as_array().expect("log rows").is_empty(),
            "expected durable rows from {route}: {logs}"
        );
    }
    let permission_logs = get_json(
        &client,
        format!("{base_url}/v1/logs/permissions?permission_id={permission_id}&limit=20"),
        &session_auth,
    )
    .await;
    let permission_kinds: Vec<&str> = permission_logs["data"]["events"]
        .as_array()
        .expect("permission log rows")
        .iter()
        .filter_map(|event| event["kind"].as_str())
        .collect();
    assert!(
        permission_kinds.contains(&"permission.approved"),
        "expected durable approval for {permission_id}: {permission_logs}"
    );

    let security = get_json(
        &client,
        format!("{base_url}/v1/security/check"),
        &admin_auth,
    )
    .await;
    assert!(security["data"]["run_id"].as_str().is_some());
}
