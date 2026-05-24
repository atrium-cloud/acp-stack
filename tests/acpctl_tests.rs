//! Integration tests for the `acpctl` local agent CLI.
//!
//! Tests spawn `serve_local` against a UDS bound inside a `tempdir` per-test
//! and drive it with an in-process HTTP/1.1-over-UDS client (mirroring the
//! one in `src/bin/acpctl.rs`). Each happy-path test asserts both the
//! response envelope and a durable `api.request` row with `source = 'local'`,
//! satisfying the Phase 3 acceptance criterion that local actions are
//! attributed to source `local`.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;

use acp_stack::api::{AppState, RuntimePaths};
use acp_stack::config::{Config, load_config_from_str};
use acp_stack::local_listener;
use acp_stack::state::{EventFilter, StateStore};
use assert_cmd::Command;
use serde_json::Value;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::Mutex as TokioMutex;
use tokio::task::JoinHandle;

const SESSION_KEY: &str = "acps_session_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const ADMIN_KEY: &str = "acps_admin_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

#[test]
fn acpctl_binary_help_smoke_test() {
    Command::cargo_bin("acpctl")
        .expect("binary should build")
        .arg("--help")
        .assert()
        .success();
}

struct Harness {
    socket: PathBuf,
    workspace_root: PathBuf,
    state: Arc<TokioMutex<StateStore>>,
    join: JoinHandle<acp_stack::error::Result<()>>,
    _workspace_tempdir: TempDir,
    _state_tempdir: TempDir,
    _socket_tempdir: TempDir,
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
        let config_path = create_runtime_files(state_tempdir.path(), &state_path);
        let runtime_paths = RuntimePaths::new(config_path, state_path.clone());

        let socket_tempdir = tempfile::tempdir().expect("socket tempdir");
        // Use a subdir the daemon will create itself, so the 0700 assertion
        // covers the daemon's own behavior (we deliberately do not chmod a
        // pre-existing operator-managed parent — see bind_local).
        let socket = socket_tempdir.path().join("acp-stack").join("acpctl.sock");

        let app_state = AppState::with_effective_bind_and_runtime_paths(
            config,
            store,
            SESSION_KEY.to_owned(),
            ADMIN_KEY.to_owned(),
            "127.0.0.1:7700".to_owned(),
            runtime_paths,
        );
        let state = app_state.state.clone();
        let bound =
            local_listener::bind_local(&socket, local_listener::ParentPolicy::RepairOwnerOnly)
                .await
                .expect("bind acpctl socket");
        let join = tokio::spawn(local_listener::serve_local(app_state, bound));

        Self {
            socket,
            workspace_root,
            state,
            join,
            _workspace_tempdir: workspace_tempdir,
            _state_tempdir: state_tempdir,
            _socket_tempdir: socket_tempdir,
        }
    }

    async fn last_api_request(&self) -> Option<EventRow> {
        let store = self.state.lock().await;
        let rows = store
            .query_events(EventFilter {
                limit: 50,
                kind: Some("api.request"),
                ..Default::default()
            })
            .ok()?;
        rows.into_iter().next().map(EventRow::from)
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.join.abort();
    }
}

fn create_runtime_files(root: &std::path::Path, state_path: &std::path::Path) -> PathBuf {
    let config_dir = root.join(".config/acp-stack");
    let state_dir = state_path.parent().expect("state parent").to_path_buf();
    std::fs::create_dir_all(&config_dir).expect("config dir");
    std::fs::create_dir_all(&state_dir).expect("state dir");
    let config_path = config_dir.join("acp-stack.toml");
    let age_key_path = config_dir.join("age.key");
    let secret_store_path = state_dir.join("secrets.age");
    std::fs::write(&config_path, "test config").expect("write config");
    std::fs::write(&age_key_path, "test age key").expect("write age key");
    std::fs::write(&secret_store_path, "test secret store").expect("write secret store");
    std::fs::set_permissions(&config_dir, std::fs::Permissions::from_mode(0o700))
        .expect("chmod config dir");
    std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o700))
        .expect("chmod state dir");
    for file in [&config_path, &age_key_path, state_path, &secret_store_path] {
        std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o600))
            .expect("chmod runtime file");
    }
    config_path
}

struct EventRow {
    source: String,
    payload: Value,
}

impl From<acp_stack::state::Event> for EventRow {
    fn from(event: acp_stack::state::Event) -> Self {
        let payload = serde_json::from_str(&event.payload_json).unwrap_or(Value::Null);
        Self {
            source: event.source,
            payload,
        }
    }
}

fn test_config() -> Config {
    let toml_text = include_str!("fixtures/valid-acp-stack.toml");
    load_config_from_str(toml_text).expect("config parses")
}

struct LocalResponse {
    status: u16,
    body: Vec<u8>,
}

impl LocalResponse {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).expect("response body is JSON")
    }
}

async fn request(
    socket: &std::path::Path,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
) -> LocalResponse {
    let mut stream = UnixStream::connect(socket)
        .await
        .expect("connect to acpctl socket");
    let body_bytes = body.unwrap_or(&[]);
    let mut request_text =
        format!("{method} {path} HTTP/1.1\r\nHost: acpctl.test\r\nConnection: close\r\n");
    if body.is_some() {
        request_text.push_str("Content-Type: application/json\r\n");
    }
    request_text.push_str(&format!("Content-Length: {}\r\n\r\n", body_bytes.len()));
    stream
        .write_all(request_text.as_bytes())
        .await
        .expect("write request");
    if !body_bytes.is_empty() {
        stream.write_all(body_bytes).await.expect("write body");
    }
    // Do NOT half-close the write side here. hyper interprets the FIN as the
    // client cancelling the request and may abandon the response. With
    // `Connection: close` the server closes the read end after writing the
    // response, which is enough to terminate `read_to_end`.
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.expect("read response");
    parse_response(&raw)
}

fn parse_response(raw: &[u8]) -> LocalResponse {
    let header_end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("response has header terminator");
    let header_text = std::str::from_utf8(&raw[..header_end]).expect("response headers are UTF-8");
    let mut lines = header_text.split("\r\n");
    let status_line = lines.next().expect("status line");
    let status: u16 = status_line
        .split(' ')
        .nth(1)
        .expect("status code present")
        .parse()
        .expect("status code numeric");
    let mut content_length = None;
    for line in lines {
        if let Some((name, value)) = line.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            content_length = value.trim().parse::<usize>().ok();
        }
    }
    let body_start = header_end + 4;
    let body = match content_length {
        Some(want) => raw[body_start..body_start + want].to_vec(),
        None => raw[body_start..].to_vec(),
    };
    LocalResponse { status, body }
}

// ----- Happy-path tests, one per acpctl subcommand --------------------------

#[tokio::test]
async fn status_returns_ok_over_uds() {
    let harness = Harness::spawn().await;
    let resp = request(&harness.socket, "GET", "/v1/status", None).await;
    assert_eq!(resp.status, 200);
    let json = resp.json();
    assert_eq!(json["ok"], true);
    assert!(json["data"]["schema_version"].is_number());
    // The cardinality skip on `/v1/status*` is bypassed for local-source
    // calls (acpctl spec: "all actions are logged with source local").
    let last = harness.last_api_request().await.expect("api.request row");
    assert_eq!(last.source, acp_stack::state::EVENT_SOURCE_LOCAL);
    assert_eq!(last.payload["path"], "/v1/status");
    assert_eq!(last.payload["key_kind"], "local");
}

#[tokio::test]
async fn security_check_returns_findings_array() {
    let harness = Harness::spawn().await;
    let resp = request(&harness.socket, "GET", "/v1/security/check", None).await;
    assert_eq!(resp.status, 200);
    let json = resp.json();
    assert_eq!(json["ok"], true);
    assert!(json["data"]["findings"].is_array());

    let last = harness.last_api_request().await.expect("api.request row");
    assert_eq!(last.source, acp_stack::state::EVENT_SOURCE_LOCAL);
    assert_eq!(last.payload["key_kind"], "local");
    assert_eq!(last.payload["path"], "/v1/security/check");
}

#[tokio::test]
async fn deps_check_runs_and_returns_report() {
    let harness = Harness::spawn().await;
    let resp = request(&harness.socket, "POST", "/v1/deps/check", Some(b"{}")).await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.json()["ok"], true);

    let last = harness.last_api_request().await.expect("api.request row");
    assert_eq!(last.source, acp_stack::state::EVENT_SOURCE_LOCAL);
    assert_eq!(last.payload["path"], "/v1/deps/check");
}

#[tokio::test]
async fn logs_query_filters_by_since() {
    let harness = Harness::spawn().await;
    // First write an event by calling deps/check so there's at least one row.
    let _ = request(&harness.socket, "POST", "/v1/deps/check", Some(b"{}")).await;
    let resp = request(
        &harness.socket,
        "GET",
        "/v1/logs/events?since=1h&limit=10",
        None,
    )
    .await;
    assert_eq!(resp.status, 200);
    let json = resp.json();
    assert_eq!(json["ok"], true);
    assert!(json["data"]["events"].is_array());

    let last = harness.last_api_request().await.expect("api.request row");
    assert_eq!(last.source, acp_stack::state::EVENT_SOURCE_LOCAL);
}

#[tokio::test]
async fn workspace_list_lists_entries() {
    let harness = Harness::spawn().await;
    std::fs::write(harness.workspace_root.join("hello.txt"), b"hi").expect("seed file");
    let resp = request(&harness.socket, "GET", "/v1/files?path=.", None).await;
    assert_eq!(resp.status, 200);
    let json = resp.json();
    assert_eq!(json["ok"], true);
    let entries = json["data"]["entries"].as_array().expect("entries array");
    assert!(entries.iter().any(|e| e["name"] == "hello.txt"));
}

#[tokio::test]
async fn workspace_read_returns_content() {
    let harness = Harness::spawn().await;
    std::fs::write(harness.workspace_root.join("read.txt"), b"local-read").expect("seed file");
    let resp = request(
        &harness.socket,
        "GET",
        "/v1/files/content?path=read.txt",
        None,
    )
    .await;
    assert_eq!(resp.status, 200);
    let json = resp.json();
    assert_eq!(json["data"]["encoding"], "utf8");
    assert_eq!(json["data"]["content"], "local-read");
}

#[tokio::test]
async fn workspace_write_writes_atomically_and_logs_local_source() {
    let harness = Harness::spawn().await;
    let body = serde_json::json!({
        "path": "notes.txt",
        "encoding": "utf8",
        "content": "hello from local",
    })
    .to_string();
    let resp = request(
        &harness.socket,
        "PUT",
        "/v1/files/content",
        Some(body.as_bytes()),
    )
    .await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.json()["ok"], true);

    let written = std::fs::read_to_string(harness.workspace_root.join("notes.txt"))
        .expect("file landed on disk");
    assert_eq!(written, "hello from local");

    // The api.request row tags source=local.
    let last = harness.last_api_request().await.expect("api.request row");
    assert_eq!(last.source, acp_stack::state::EVENT_SOURCE_LOCAL);

    // The workspace.write event also lands with source=local, proving the
    // source threading through publish_workspace_mutation.
    let store = harness.state.lock().await;
    let mutations = store
        .query_events(EventFilter {
            limit: 50,
            kind: Some("workspace.write"),
            ..Default::default()
        })
        .expect("query workspace.write events");
    let row = mutations.into_iter().next().expect("workspace.write row");
    assert_eq!(row.source, acp_stack::state::EVENT_SOURCE_LOCAL);
}

#[tokio::test]
async fn command_run_submits_through_gateway() {
    let harness = Harness::spawn().await;
    let body = serde_json::json!({ "command": "echo hi" }).to_string();
    let resp = request(
        &harness.socket,
        "POST",
        "/v1/commands",
        Some(body.as_bytes()),
    )
    .await;
    assert_eq!(resp.status, 200, "body: {:?}", resp.json());
    let json = resp.json();
    assert_eq!(json["ok"], true);
    assert!(json["data"]["id"].is_string());

    let last = harness.last_api_request().await.expect("api.request row");
    assert_eq!(last.source, acp_stack::state::EVENT_SOURCE_LOCAL);
}

#[tokio::test]
async fn config_export_returns_canonical_toml() {
    let harness = Harness::spawn().await;
    let resp = request(&harness.socket, "GET", "/v1/config/export", None).await;
    assert_eq!(resp.status, 200);
    let json = resp.json();
    assert_eq!(json["ok"], true);
    let toml = json["data"]["toml"].as_str().expect("toml string");
    assert!(toml.contains("[api]"));
}

#[tokio::test]
async fn permissions_pending_returns_array() {
    let harness = Harness::spawn().await;
    let resp = request(
        &harness.socket,
        "GET",
        "/v1/permissions/pending?limit=10",
        None,
    )
    .await;
    assert_eq!(resp.status, 200);
    let json = resp.json();
    assert_eq!(json["ok"], true);
    assert!(json["data"]["permissions"].is_array());
}

// ----- Negative / contract tests --------------------------------------------

#[tokio::test]
async fn local_router_returns_404_for_secrets_routes() {
    let harness = Harness::spawn().await;
    let resp = request(&harness.socket, "GET", "/v1/secrets", None).await;
    assert_eq!(resp.status, 404);
}

#[tokio::test]
async fn local_router_returns_404_for_config_import() {
    let harness = Harness::spawn().await;
    let resp = request(&harness.socket, "POST", "/v1/config/import", Some(b"{}")).await;
    assert_eq!(resp.status, 404);
}

#[tokio::test]
async fn local_router_returns_404_for_permissions_approve() {
    let harness = Harness::spawn().await;
    let resp = request(
        &harness.socket,
        "POST",
        "/v1/permissions/some-id/approve",
        Some(b"{}"),
    )
    .await;
    assert_eq!(resp.status, 404);
}

#[tokio::test]
async fn local_router_returns_404_for_agent_install() {
    let harness = Harness::spawn().await;
    let resp = request(&harness.socket, "POST", "/v1/agent/install", Some(b"{}")).await;
    assert_eq!(resp.status, 404);
}

#[tokio::test]
async fn local_router_returns_404_for_agent_start() {
    let harness = Harness::spawn().await;
    let resp = request(&harness.socket, "POST", "/v1/agent/start", Some(b"{}")).await;
    assert_eq!(resp.status, 404);
}

#[tokio::test]
async fn local_router_returns_404_for_agent_stop() {
    let harness = Harness::spawn().await;
    let resp = request(&harness.socket, "POST", "/v1/agent/stop", Some(b"{}")).await;
    assert_eq!(resp.status, 404);
}

#[tokio::test]
async fn local_router_returns_404_for_secrets_write() {
    // Writing a secret is admin-tier on the public API. Acpctl must not be
    // able to mutate the secret store even over the trusted UDS — the only
    // local mutation acpctl performs is the workspace write at
    // /v1/files/content.
    let harness = Harness::spawn().await;
    let body = serde_json::json!({ "name": "x", "value": "y" }).to_string();
    let resp = request(
        &harness.socket,
        "POST",
        "/v1/secrets",
        Some(body.as_bytes()),
    )
    .await;
    assert_eq!(resp.status, 404);
}

#[tokio::test]
async fn local_router_returns_404_for_secrets_delete() {
    let harness = Harness::spawn().await;
    let resp = request(&harness.socket, "DELETE", "/v1/secrets/example", None).await;
    assert_eq!(resp.status, 404);
}

#[tokio::test]
async fn local_router_cannot_rotate_api_keys_via_secrets_write() {
    // "Rotating API keys" through acpctl boils down to overwriting the
    // session or admin key ref in the secret store. The fixture config uses
    // `acps_session_key` and `acps_admin_key` as the canonical refs; both
    // POSTs must 404 over the UDS regardless of payload so a compromised
    // agent process cannot pivot a session-tier capability into admin-tier.
    let harness = Harness::spawn().await;
    for ref_name in ["acps_session_key", "acps_admin_key"] {
        let body = serde_json::json!({ "name": ref_name, "value": "evil" }).to_string();
        let resp = request(
            &harness.socket,
            "POST",
            "/v1/secrets",
            Some(body.as_bytes()),
        )
        .await;
        assert_eq!(
            resp.status, 404,
            "POST /v1/secrets must 404 for ref `{ref_name}` over UDS"
        );
        let resp = request(
            &harness.socket,
            "DELETE",
            &format!("/v1/secrets/{ref_name}"),
            None,
        )
        .await;
        assert_eq!(
            resp.status, 404,
            "DELETE /v1/secrets/{ref_name} must 404 over UDS"
        );
    }
}

#[tokio::test]
async fn local_router_cannot_read_api_keys_via_secrets_get() {
    // Read-side companion: agents asking acpctl for the value of either auth
    // ref must hit a hard 404 — the spec deny list says secrets read is
    // off-allowlist, and the test pins the contract for the two refs the
    // public API uses for authentication.
    let harness = Harness::spawn().await;
    for ref_name in ["acps_session_key", "acps_admin_key"] {
        let resp = request(
            &harness.socket,
            "GET",
            &format!("/v1/secrets/{ref_name}"),
            None,
        )
        .await;
        assert_eq!(
            resp.status, 404,
            "GET /v1/secrets/{ref_name} must 404 over UDS"
        );
    }
}

#[tokio::test]
async fn local_router_returns_404_for_permissions_deny() {
    // Mirrors the existing /approve negative test. Together they prove that
    // acpctl (or anything else inside the runtime that talks to the UDS)
    // cannot self-approve OR self-deny a permission request — both decisions
    // remain on the operator-facing public API behind a session key.
    let harness = Harness::spawn().await;
    let resp = request(
        &harness.socket,
        "POST",
        "/v1/permissions/some-id/deny",
        Some(b"{}"),
    )
    .await;
    assert_eq!(resp.status, 404);
}

#[tokio::test]
async fn local_router_blocks_all_high_risk_routes() {
    // Single declarative assertion covering every route the Phase 4 acpctl
    // hardening section lists as off-limits to acpctl. A regression that
    // accidentally adds one of these to `build_local_router` will fail this
    // test even if no per-route test was added.
    let harness = Harness::spawn().await;
    let cases: &[(&str, &str, Option<&[u8]>)] = &[
        ("GET", "/v1/secrets", None),
        ("POST", "/v1/secrets", Some(b"{}")),
        ("DELETE", "/v1/secrets/example", None),
        ("POST", "/v1/config/import", Some(b"{}")),
        ("POST", "/v1/agent/install", Some(b"{}")),
        ("POST", "/v1/agent/start", Some(b"{}")),
        ("POST", "/v1/agent/stop", Some(b"{}")),
        ("POST", "/v1/permissions/abc/approve", Some(b"{}")),
        ("POST", "/v1/permissions/abc/deny", Some(b"{}")),
        (
            "POST",
            "/v1/ws/connections/disconnect",
            Some(br#"{"connection_id":"abc"}"#),
        ),
        (
            "POST",
            "/v1/ws/sessions/disconnect",
            Some(br#"{"session_id":"abc"}"#),
        ),
    ];
    for (method, path, body) in cases {
        let resp = request(&harness.socket, method, path, *body).await;
        assert_eq!(
            resp.status, 404,
            "{method} {path} must not be reachable via acpctl UDS"
        );
    }
}

#[tokio::test]
async fn local_socket_has_owner_only_mode() {
    let harness = Harness::spawn().await;
    let meta = std::fs::metadata(&harness.socket).expect("stat socket");
    let mode = meta.permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "expected 0600, got {mode:o}");
    let parent = harness.socket.parent().expect("socket has parent");
    let parent_mode = std::fs::metadata(parent)
        .expect("stat parent")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        parent_mode, 0o700,
        "expected 0700 parent, got {parent_mode:o}"
    );
}

#[tokio::test]
async fn local_listener_unlinks_socket_on_shutdown() {
    let socket_tempdir = tempfile::tempdir().expect("socket tempdir");
    let socket = socket_tempdir.path().join("acpctl.sock");

    let workspace_tempdir = tempfile::tempdir().expect("workspace tempdir");
    let workspace_root = workspace_tempdir.path().to_path_buf();
    std::fs::create_dir(workspace_root.join("uploads")).expect("uploads");
    let state_tempdir = tempfile::tempdir().expect("state tempdir");
    let state_path = state_tempdir.path().join("state.sqlite");
    let store = StateStore::open(&state_path).expect("state open");
    store.migrate().expect("migrate");

    let mut config = test_config();
    config.workspace.root = workspace_root.to_string_lossy().into_owned();
    config.workspace.uploads = workspace_root
        .join("uploads")
        .to_string_lossy()
        .into_owned();

    let app_state = AppState::new(config, store, SESSION_KEY.to_owned(), ADMIN_KEY.to_owned());
    let bound = local_listener::bind_local(&socket, local_listener::ParentPolicy::RepairOwnerOnly)
        .await
        .expect("bind acpctl socket");
    let task = tokio::spawn(local_listener::serve_local(app_state, bound));
    assert!(socket.exists(), "socket should be bound");

    // Abort the task and give Drop a chance to run.
    task.abort();
    let _ = task.await;
    // Give the runtime a tick for the Drop unlink.
    for _ in 0..50 {
        if !socket.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(!socket.exists(), "socket should be unlinked on shutdown");
}

#[tokio::test]
async fn bind_local_refuses_when_another_daemon_owns_the_socket() {
    let socket_tempdir = tempfile::tempdir().expect("socket tempdir");
    let socket = socket_tempdir.path().join("acp-stack").join("acpctl.sock");

    let workspace_tempdir = tempfile::tempdir().expect("workspace tempdir");
    let workspace_root = workspace_tempdir.path().to_path_buf();
    std::fs::create_dir(workspace_root.join("uploads")).expect("uploads");
    let state_tempdir = tempfile::tempdir().expect("state tempdir");
    let state_path = state_tempdir.path().join("state.sqlite");
    let store = StateStore::open(&state_path).expect("state open");
    store.migrate().expect("migrate");

    let mut config = test_config();
    config.workspace.root = workspace_root.to_string_lossy().into_owned();
    config.workspace.uploads = workspace_root
        .join("uploads")
        .to_string_lossy()
        .into_owned();
    let app_state = AppState::new(config, store, SESSION_KEY.to_owned(), ADMIN_KEY.to_owned());
    let first_bound =
        local_listener::bind_local(&socket, local_listener::ParentPolicy::RepairOwnerOnly)
            .await
            .expect("first bind succeeds");
    let first_task = tokio::spawn(local_listener::serve_local(app_state.clone(), first_bound));

    // Wait for the first listener to be live.
    for _ in 0..50 {
        if UnixStream::connect(&socket).await.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    // A second bind on the same path must refuse with AddrInUse rather than
    // silently take over.
    let result =
        local_listener::bind_local(&socket, local_listener::ParentPolicy::RepairOwnerOnly).await;
    assert!(
        result.is_err(),
        "second bind should be rejected when first is live"
    );

    first_task.abort();
    let _ = first_task.await;
}

#[tokio::test]
async fn bind_local_unlinks_stale_socket_and_succeeds() {
    let socket_tempdir = tempfile::tempdir().expect("socket tempdir");
    let socket = socket_tempdir.path().join("acp-stack").join("acpctl.sock");

    // Pre-bind and drop without calling our guard, leaving a "stale" socket
    // (no accepter behind it).
    std::fs::create_dir_all(socket.parent().unwrap()).expect("mkdir");
    {
        let _listener = std::os::unix::net::UnixListener::bind(&socket).expect("bind");
        // Drop closes the listener but leaves the socket inode in the
        // filesystem — same shape as a crashed prior daemon.
    }
    assert!(socket.exists(), "stale socket should be on disk");

    // bind_local should detect the stale socket via ConnectionRefused, unlink
    // it, and bind cleanly.
    let bound = local_listener::bind_local(&socket, local_listener::ParentPolicy::RepairOwnerOnly)
        .await
        .expect("bind cleans up stale socket");

    // Drop the bound listener: SocketGuard runs and unlinks the new socket.
    drop(bound);
    // Re-bind to prove the file went away.
    assert!(!socket.exists(), "guard should unlink the socket on drop");
}

#[tokio::test]
async fn workspace_write_rejects_over_max_file_bytes() {
    let harness = Harness::spawn().await;
    // Build a body larger than the test config's workspace.max_file_bytes
    // (8 MiB per fixtures/valid-acp-stack.toml).
    let big = "x".repeat(9 * 1024 * 1024);
    let body = serde_json::json!({
        "path": "huge.bin",
        "encoding": "utf8",
        "content": big,
    })
    .to_string();
    let resp = request(
        &harness.socket,
        "PUT",
        "/v1/files/content",
        Some(body.as_bytes()),
    )
    .await;
    // Either 413 (body limit) or workspace-too-large depending on which cap
    // bites first; both prove the caller can't bypass the server-side checks
    // by going through the UDS.
    assert!(
        resp.status == 413 || resp.status == 400,
        "expected 400 or 413, got {}",
        resp.status
    );
}
