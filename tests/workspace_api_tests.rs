use std::path::PathBuf;
use std::sync::Arc;

use acp_stack::api::{self, AppState};
use acp_stack::config::{Config, load_config_from_str};
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
    uploads_root: PathBuf,
    _state: Arc<TokioMutex<StateStore>>,
    join: JoinHandle<acp_stack::error::Result<()>>,
    _workspace_tempdir: TempDir,
    _state_tempdir: TempDir,
}

impl Harness {
    async fn spawn() -> Self {
        let workspace_tempdir = tempfile::tempdir().expect("workspace tempdir");
        let workspace_root = workspace_tempdir.path().to_path_buf();
        let uploads_root = workspace_root.join("uploads");
        std::fs::create_dir(&uploads_root).expect("uploads dir");

        let mut config = test_config();
        config.workspace.root = workspace_root.to_string_lossy().into_owned();
        config.workspace.uploads = uploads_root.to_string_lossy().into_owned();

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
            uploads_root,
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

#[tokio::test]
async fn workspace_metadata_returns_configured_roots() {
    let harness = Harness::spawn().await;
    let client = session_client();

    let response = auth(client.get(format!("{}/v1/workspace", harness.base_url)))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["ok"], true);
    assert_eq!(
        body["data"]["root"],
        harness.workspace_root.to_string_lossy().as_ref()
    );
    assert_eq!(body["data"]["uploads_path"], "uploads");
    assert_eq!(body["data"]["default_shell"], "/bin/bash");
    assert_eq!(body["data"]["max_file_bytes"], 8_388_608);
}

#[tokio::test]
async fn lists_directory_contents() {
    let harness = Harness::spawn().await;
    std::fs::write(harness.workspace_root.join("zzz.txt"), b"").expect("write zzz");
    std::fs::write(harness.workspace_root.join("aaa.txt"), b"hi").expect("write aaa");
    std::fs::create_dir(harness.workspace_root.join("subdir")).expect("mkdir");

    let response = auth(session_client().get(format!("{}/v1/files?path=", harness.base_url)))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    let names: Vec<&str> = body["data"]["entries"]
        .as_array()
        .expect("entries array")
        .iter()
        .map(|entry| entry["name"].as_str().expect("name"))
        .collect();
    // uploads dir is created by the harness, plus the three new entries.
    assert_eq!(names, vec!["subdir", "uploads", "aaa.txt", "zzz.txt"]);
}

#[tokio::test]
async fn reads_text_file_as_utf8() {
    let harness = Harness::spawn().await;
    std::fs::write(harness.workspace_root.join("hello.md"), b"# Hi").expect("write");

    let response = auth(session_client().get(format!(
        "{}/v1/files/content?path=hello.md",
        harness.base_url
    )))
    .send()
    .await
    .expect("send");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["data"]["encoding"], "utf8");
    assert_eq!(body["data"]["content"], "# Hi");
    assert_eq!(body["data"]["size"], 4);
}

#[tokio::test]
async fn reads_binary_file_as_base64() {
    let harness = Harness::spawn().await;
    // 0xFF 0xFE is not valid UTF-8.
    std::fs::write(
        harness.workspace_root.join("blob.bin"),
        [0xFFu8, 0xFE, 0x00],
    )
    .expect("write");

    let response = auth(session_client().get(format!(
        "{}/v1/files/content?path=blob.bin",
        harness.base_url
    )))
    .send()
    .await
    .expect("send");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["data"]["encoding"], "base64");
    use base64::Engine as _;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(body["data"]["content"].as_str().expect("content str"))
        .expect("decode");
    assert_eq!(decoded, vec![0xFFu8, 0xFE, 0x00]);
}

#[tokio::test]
async fn reads_above_limit_returns_too_large() {
    let harness = Harness::spawn().await;
    let big_path = harness.workspace_root.join("big.bin");
    // 8 MiB + 1 byte
    std::fs::write(&big_path, vec![0u8; 8 * 1024 * 1024 + 1]).expect("write big");

    let response = auth(session_client().get(format!(
        "{}/v1/files/content?path=big.bin",
        harness.base_url
    )))
    .send()
    .await
    .expect("send");
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "workspace.too_large");
}

#[tokio::test]
async fn read_missing_returns_not_found() {
    let harness = Harness::spawn().await;
    let response = auth(session_client().get(format!(
        "{}/v1/files/content?path=absent.md",
        harness.base_url
    )))
    .send()
    .await
    .expect("send");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "workspace.not_found");
}

#[tokio::test]
async fn download_streams_bytes_with_disposition_header() {
    let harness = Harness::spawn().await;
    let bytes = vec![1u8, 2, 3, 4, 5];
    std::fs::write(harness.workspace_root.join("data.bin"), &bytes).expect("write");

    let response = auth(session_client().get(format!(
        "{}/v1/files/download?path=data.bin",
        harness.base_url
    )))
    .send()
    .await
    .expect("send");
    assert_eq!(response.status(), StatusCode::OK);
    let disposition = response
        .headers()
        .get(reqwest::header::CONTENT_DISPOSITION)
        .expect("content-disposition")
        .to_str()
        .expect("ascii")
        .to_owned();
    assert!(
        disposition.contains("attachment") && disposition.contains("data.bin"),
        "got: {disposition}"
    );
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .expect("content-type")
        .to_str()
        .expect("ascii")
        .to_owned();
    assert_eq!(content_type, "application/octet-stream");
    let length: u64 = response
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)
        .expect("content-length")
        .to_str()
        .expect("ascii")
        .parse()
        .expect("parse");
    assert_eq!(length, 5);
    let body = response.bytes().await.expect("body");
    assert_eq!(body.as_ref(), bytes.as_slice());
}

#[tokio::test]
async fn download_above_limit_returns_too_large_before_any_body() {
    let harness = Harness::spawn().await;
    std::fs::write(
        harness.workspace_root.join("over.bin"),
        vec![0u8; 8 * 1024 * 1024 + 1],
    )
    .expect("write");

    let response = auth(session_client().get(format!(
        "{}/v1/files/download?path=over.bin",
        harness.base_url
    )))
    .send()
    .await
    .expect("send");
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "workspace.too_large");
}

#[tokio::test]
async fn download_missing_returns_not_found() {
    let harness = Harness::spawn().await;
    let response = auth(session_client().get(format!(
        "{}/v1/files/download?path=absent.bin",
        harness.base_url
    )))
    .send()
    .await
    .expect("send");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "workspace.not_found");
}

#[tokio::test]
async fn rejects_path_traversal_on_read() {
    let harness = Harness::spawn().await;
    let response = auth(session_client().get(format!(
        "{}/v1/files/content?path=../etc/passwd",
        harness.base_url
    )))
    .send()
    .await
    .expect("send");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "workspace.path_invalid");
}

#[tokio::test]
async fn rejects_absolute_path_outside_root() {
    let harness = Harness::spawn().await;
    let response = auth(session_client().get(format!(
        "{}/v1/files/content?path=/etc/passwd",
        harness.base_url
    )))
    .send()
    .await
    .expect("send");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "workspace.path_invalid");
}

#[cfg(unix)]
#[tokio::test]
async fn rejects_symlink_escape_on_read() {
    use std::os::unix::fs::symlink;

    let harness = Harness::spawn().await;
    let outside = tempfile::tempdir().expect("outside tempdir");
    let outside_target = outside.path().join("leak");
    std::fs::write(&outside_target, b"leak").expect("write outside");
    symlink(&outside_target, harness.workspace_root.join("escape")).expect("symlink");

    let response =
        auth(session_client().get(format!("{}/v1/files/content?path=escape", harness.base_url)))
            .send()
            .await
            .expect("send");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "workspace.symlink_escape");
}

#[tokio::test]
async fn rejects_admin_key_on_workspace_routes() {
    let harness = Harness::spawn().await;
    let response = session_client()
        .get(format!("{}/v1/workspace", harness.base_url))
        .header(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {ADMIN_KEY}"),
        )
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "auth.wrong_kind");
}

#[tokio::test]
async fn writes_file_atomically_with_utf8_content() {
    let harness = Harness::spawn().await;
    let body = serde_json::json!({
        "path": "note.md",
        "encoding": "utf8",
        "content": "# Hello"
    });

    let response = auth(session_client().put(format!("{}/v1/files/content", harness.base_url)))
        .json(&body)
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK);
    let envelope: Value = response.json().await.expect("json");
    assert_eq!(envelope["data"]["path"], "note.md");
    assert_eq!(envelope["data"]["size"], 7);

    let on_disk = std::fs::read(harness.workspace_root.join("note.md")).expect("read");
    assert_eq!(on_disk, b"# Hello");

    let response = auth(session_client().put(format!("{}/v1/files/content", harness.base_url)))
        .json(&serde_json::json!({
            "path": "note.md",
            "encoding": "utf8",
            "content": "# Updated content"
        }))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK);
    let on_disk = std::fs::read(harness.workspace_root.join("note.md")).expect("read");
    assert_eq!(on_disk, b"# Updated content");

    let leftover: Vec<_> = std::fs::read_dir(&harness.workspace_root)
        .expect("read_dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name() != "note.md" && e.file_name() != "uploads")
        .collect();
    assert!(leftover.is_empty(), "leftover: {leftover:?}");
}

#[tokio::test]
async fn writes_with_base64_encoding() {
    let harness = Harness::spawn().await;
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode([0xDEu8, 0xAD, 0xBE, 0xEF]);

    let response = auth(session_client().put(format!("{}/v1/files/content", harness.base_url)))
        .json(&serde_json::json!({
            "path": "blob.bin",
            "encoding": "base64",
            "content": encoded
        }))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK);
    let on_disk = std::fs::read(harness.workspace_root.join("blob.bin")).expect("read");
    assert_eq!(on_disk, vec![0xDEu8, 0xAD, 0xBE, 0xEF]);
}

#[tokio::test]
async fn write_above_limit_returns_too_large() {
    let harness = Harness::spawn().await;
    let big_text = "x".repeat(8 * 1024 * 1024 + 1);
    let response = auth(session_client().put(format!("{}/v1/files/content", harness.base_url)))
        .json(&serde_json::json!({
            "path": "big.txt",
            "encoding": "utf8",
            "content": big_text
        }))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "workspace.too_large");
}

#[tokio::test]
async fn write_rejects_invalid_encoding() {
    let harness = Harness::spawn().await;
    let response = auth(session_client().put(format!("{}/v1/files/content", harness.base_url)))
        .json(&serde_json::json!({
            "path": "note.md",
            "encoding": "rot13",
            "content": "abc"
        }))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "workspace.encoding_invalid");
}

#[tokio::test]
async fn uploads_multipart_lands_at_resolved_path() {
    let harness = Harness::spawn().await;
    let bytes = vec![1u8, 2, 3, 4];
    let form = reqwest::multipart::Form::new()
        .text("path", "demo.bin")
        .part(
            "file",
            reqwest::multipart::Part::bytes(bytes.clone()).file_name("demo.bin"),
        );

    let response = auth(session_client().post(format!("{}/v1/files/upload", harness.base_url)))
        .multipart(form)
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK);
    let envelope: Value = response.json().await.expect("json");
    assert_eq!(envelope["data"]["path"], "uploads/demo.bin");
    assert_eq!(envelope["data"]["filename"], "demo.bin");
    assert_eq!(envelope["data"]["size"], 4);
    let on_disk = std::fs::read(harness.uploads_root.join("demo.bin")).expect("read");
    assert_eq!(on_disk, bytes);
}

#[tokio::test]
async fn upload_outside_uploads_root_is_rejected() {
    let harness = Harness::spawn().await;
    let form = reqwest::multipart::Form::new()
        .text("path", "../outside.bin")
        .part(
            "file",
            reqwest::multipart::Part::bytes(vec![0u8]).file_name("outside.bin"),
        );

    let response = auth(session_client().post(format!("{}/v1/files/upload", harness.base_url)))
        .multipart(form)
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("json");
    let code = body["error"]["code"].as_str().expect("code");
    assert!(
        code == "workspace.path_invalid" || code == "workspace.symlink_escape",
        "got {code}"
    );
}

#[tokio::test]
async fn upload_with_missing_path_field_returns_400() {
    let harness = Harness::spawn().await;
    let form = reqwest::multipart::Form::new().part(
        "file",
        reqwest::multipart::Part::bytes(vec![0u8]).file_name("noop"),
    );

    let response = auth(session_client().post(format!("{}/v1/files/upload", harness.base_url)))
        .multipart(form)
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "workspace.upload_invalid");
}

#[tokio::test]
async fn upload_above_limit_returns_too_large() {
    let harness = Harness::spawn().await;
    let form = reqwest::multipart::Form::new()
        .text("path", "big.bin")
        .part(
            "file",
            reqwest::multipart::Part::bytes(vec![0u8; 8 * 1024 * 1024 + 1]).file_name("big.bin"),
        );

    let response = auth(session_client().post(format!("{}/v1/files/upload", harness.base_url)))
        .multipart(form)
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "workspace.too_large");
}

#[tokio::test]
async fn upload_to_missing_parent_returns_not_found() {
    let harness = Harness::spawn().await;
    let form = reqwest::multipart::Form::new()
        .text("path", "nested/dir/demo.bin")
        .part(
            "file",
            reqwest::multipart::Part::bytes(vec![0u8]).file_name("demo.bin"),
        );

    let response = auth(session_client().post(format!("{}/v1/files/upload", harness.base_url)))
        .multipart(form)
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "workspace.not_found");
}

#[tokio::test]
async fn delete_removes_file() {
    let harness = Harness::spawn().await;
    std::fs::write(harness.workspace_root.join("temp.txt"), b"bye").expect("write");

    let response =
        auth(session_client().delete(format!("{}/v1/files?path=temp.txt", harness.base_url)))
            .send()
            .await
            .expect("send");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["data"]["deleted"], true);
    assert!(!harness.workspace_root.join("temp.txt").exists());
}

#[tokio::test]
async fn delete_refuses_directory() {
    let harness = Harness::spawn().await;
    std::fs::create_dir(harness.workspace_root.join("scratch")).expect("mkdir");

    let response =
        auth(session_client().delete(format!("{}/v1/files?path=scratch", harness.base_url)))
            .send()
            .await
            .expect("send");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "workspace.path_invalid");
}

#[tokio::test]
async fn delete_missing_returns_not_found() {
    let harness = Harness::spawn().await;
    let response =
        auth(session_client().delete(format!("{}/v1/files?path=absent", harness.base_url)))
            .send()
            .await
            .expect("send");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "workspace.not_found");
}

// ----- WebSocket -------------------------------------------------------------

async fn open_ws(
    base_url: &str,
    topics: &[&str],
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
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

    use futures::SinkExt;
    let subscribe = serde_json::json!({
        "type": "subscribe",
        "topics": topics
    });
    stream
        .send(tokio_tungstenite::tungstenite::Message::Text(
            subscribe.to_string().into(),
        ))
        .await
        .expect("subscribe");
    stream
}

async fn next_ws_event<S>(stream: &mut S) -> Value
where
    S: futures::Stream<
            Item = std::result::Result<
                tokio_tungstenite::tungstenite::Message,
                tokio_tungstenite::tungstenite::Error,
            >,
        > + Unpin,
{
    use futures::StreamExt;
    let message = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
        .await
        .expect("ws timeout")
        .expect("ws closed")
        .expect("ws message");
    let text = match message {
        tokio_tungstenite::tungstenite::Message::Text(text) => text,
        other => panic!("expected text frame, got {other:?}"),
    };
    serde_json::from_str(text.as_str()).expect("ws json")
}

#[tokio::test]
async fn workspace_websocket_publishes_on_write() {
    let harness = Harness::spawn().await;
    let mut stream = open_ws(&harness.base_url, &["workspace"]).await;

    // Subscribe is async; give the server a moment to record the topic before
    // the producer side fires. 50ms is plenty for a localhost loopback.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let response = auth(session_client().put(format!("{}/v1/files/content", harness.base_url)))
        .json(&serde_json::json!({
            "path": "ws-test.txt",
            "encoding": "utf8",
            "content": "watching"
        }))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK);

    let event = next_ws_event(&mut stream).await;
    assert_eq!(event["type"], "event");
    assert_eq!(event["topic"], "workspace");
    assert_eq!(event["payload"]["kind"], "workspace.write");
    assert_eq!(event["payload"]["data"]["path"], "ws-test.txt");
    assert_eq!(event["payload"]["data"]["size"], 8);
}

#[tokio::test]
async fn workspace_websocket_publishes_on_upload_and_delete() {
    let harness = Harness::spawn().await;
    let mut stream = open_ws(&harness.base_url, &["workspace"]).await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let form = reqwest::multipart::Form::new()
        .text("path", "evt.bin")
        .part(
            "file",
            reqwest::multipart::Part::bytes(vec![1u8, 2, 3]).file_name("evt.bin"),
        );
    let upload = auth(session_client().post(format!("{}/v1/files/upload", harness.base_url)))
        .multipart(form)
        .send()
        .await
        .expect("send");
    assert_eq!(upload.status(), StatusCode::OK);

    let event = next_ws_event(&mut stream).await;
    assert_eq!(event["payload"]["kind"], "workspace.upload");
    assert_eq!(event["payload"]["data"]["path"], "uploads/evt.bin");

    let delete = auth(session_client().delete(format!(
        "{}/v1/files?path=uploads/evt.bin",
        harness.base_url
    )))
    .send()
    .await
    .expect("send");
    assert_eq!(delete.status(), StatusCode::OK);

    let event = next_ws_event(&mut stream).await;
    assert_eq!(event["payload"]["kind"], "workspace.delete");
    assert_eq!(event["payload"]["data"]["path"], "uploads/evt.bin");
}

#[tokio::test]
async fn workspace_websocket_does_not_publish_on_read() {
    let harness = Harness::spawn().await;
    std::fs::write(harness.workspace_root.join("seen.txt"), b"hi").expect("write");
    let mut stream = open_ws(&harness.base_url, &["workspace"]).await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let response = auth(session_client().get(format!(
        "{}/v1/files/content?path=seen.txt",
        harness.base_url
    )))
    .send()
    .await
    .expect("send");
    assert_eq!(response.status(), StatusCode::OK);

    use futures::StreamExt;
    let result = tokio::time::timeout(std::time::Duration::from_millis(200), stream.next()).await;
    assert!(
        result.is_err(),
        "read should not publish; got event: {result:?}"
    );
}
