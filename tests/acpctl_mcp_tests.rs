//! Integration tests for `acpctl mcp serve`.
//!
//! Each test boots an in-process daemon UDS listener (mirroring the pattern in
//! `tests/acpctl_tests.rs`), spawns `acpctl mcp serve` as a child process, and
//! drives it with the rmcp client over either stdio or http-uds. For each
//! tool we assert (a) the response shape returned by the MCP server, and (b)
//! that the underlying UDS hit landed in SQLite with `source = "local"` —
//! the durable Phase 3 acceptance criterion.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use acp_stack::api::AppState;
use acp_stack::config::{Config, load_config_from_str};
use acp_stack::local_listener;
use acp_stack::state::{EventFilter, StateStore};
use rmcp::model::{CallToolRequestParams, CallToolResult, Tool};
use rmcp::service::{RunningService, ServiceExt};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess, UnixSocketHttpClient};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::process::Command as TokioCommand;
use tokio::sync::Mutex as TokioMutex;
use tokio::task::JoinHandle;

const SESSION_KEY: &str = "acps_session_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const ADMIN_KEY: &str = "acps_admin_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

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

        let socket_tempdir = tempfile::tempdir().expect("socket tempdir");
        let socket = socket_tempdir.path().join("acp-stack").join("acpctl.sock");

        let app_state = AppState::new(config, store, SESSION_KEY.to_owned(), ADMIN_KEY.to_owned());
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

    async fn api_request_paths(&self) -> Vec<(String, String)> {
        let store = self.state.lock().await;
        let rows = store
            .query_events(EventFilter {
                limit: 200,
                kind: Some("api.request"),
                ..Default::default()
            })
            .expect("query events");
        rows.into_iter()
            .map(|row| {
                let payload: Value = serde_json::from_str(&row.payload_json).unwrap_or(Value::Null);
                let path = payload
                    .get("path")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                (row.source, path)
            })
            .collect()
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

fn acpctl_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_acpctl"))
}

/// Spawn `acpctl mcp serve --transport stdio --socket <daemon>` and wrap it
/// in an rmcp client.
async fn spawn_stdio_client(
    daemon_socket: &std::path::Path,
) -> RunningService<rmcp::RoleClient, ()> {
    let mut command = TokioCommand::new(acpctl_bin());
    command
        .arg("--socket")
        .arg(daemon_socket)
        .arg("mcp")
        .arg("serve")
        .arg("--transport")
        .arg("stdio")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());

    let child = TokioChildProcess::new(command).expect("spawn acpctl mcp serve");
    ().serve(child).await.expect("rmcp client init handshake")
}

async fn call_tool_value(
    client: &RunningService<rmcp::RoleClient, ()>,
    name: &str,
    arguments: Option<serde_json::Map<String, Value>>,
) -> CallToolResult {
    let mut params = CallToolRequestParams::new(name.to_owned());
    if let Some(args) = arguments {
        params = params.with_arguments(args);
    }
    tokio::time::timeout(Duration::from_secs(10), client.peer().call_tool(params))
        .await
        .expect("tool call did not time out")
        .expect("tool call succeeded")
}

fn structured(result: &CallToolResult) -> &Value {
    result
        .structured_content
        .as_ref()
        .expect("tool returned structured content")
}

#[tokio::test]
async fn mcp_stdio_lists_exactly_the_ten_tools() {
    let harness = Harness::spawn().await;
    let client = spawn_stdio_client(&harness.socket).await;
    let tools: Vec<Tool> = client.peer().list_all_tools().await.expect("list_tools");
    let mut got: Vec<String> = tools.iter().map(|t| t.name.to_string()).collect();
    got.sort();
    let mut expected: Vec<String> = [
        "command_run",
        "config_export",
        "deps_check",
        "logs_query",
        "permissions_pending",
        "security_check",
        "status",
        "workspace_list",
        "workspace_read",
        "workspace_write",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    expected.sort();
    assert_eq!(got, expected);

    // Negative check: none of the denied operations should ever appear, even
    // if a future commit accidentally tries to expose them.
    for forbidden in [
        "secrets_list",
        "secrets_get",
        "permissions_approve",
        "permissions_deny",
        "config_import",
        "agent_install",
    ] {
        assert!(
            !got.iter().any(|n| n == forbidden),
            "tool {forbidden} must not be reachable"
        );
    }
    let _ = client.cancel().await;
}

#[tokio::test]
async fn mcp_stdio_drives_every_tool_with_source_local() {
    let harness = Harness::spawn().await;
    // Seed a workspace file so workspace_read has something to return.
    std::fs::write(harness.workspace_root.join("hello.txt"), b"hi from mcp").expect("seed file");
    let client = spawn_stdio_client(&harness.socket).await;

    // status
    let status = call_tool_value(&client, "status", None).await;
    assert_eq!(structured(&status)["ok"], true);

    // security_check
    let sec = call_tool_value(&client, "security_check", None).await;
    assert_eq!(structured(&sec)["ok"], true);

    // deps_check
    let deps = call_tool_value(&client, "deps_check", None).await;
    assert_eq!(structured(&deps)["ok"], true);

    // logs_query
    let mut logs_args = serde_json::Map::new();
    logs_args.insert("limit".into(), json!(10));
    logs_args.insert("since".into(), json!("2026-01-01T00:00:00Z"));
    let logs = call_tool_value(&client, "logs_query", Some(logs_args)).await;
    assert_eq!(structured(&logs)["ok"], true);
    assert!(structured(&logs)["data"]["events"].is_array());

    // workspace_list
    let mut list_args = serde_json::Map::new();
    list_args.insert("path".into(), json!("."));
    let list = call_tool_value(&client, "workspace_list", Some(list_args)).await;
    assert_eq!(structured(&list)["ok"], true);
    let entries = structured(&list)["data"]["entries"]
        .as_array()
        .expect("entries array");
    assert!(entries.iter().any(|e| e["name"] == "hello.txt"));

    // workspace_read
    let mut read_args = serde_json::Map::new();
    read_args.insert("path".into(), json!("hello.txt"));
    let read = call_tool_value(&client, "workspace_read", Some(read_args)).await;
    assert_eq!(structured(&read)["data"]["content"], "hi from mcp");

    // workspace_write
    let mut write_args = serde_json::Map::new();
    write_args.insert("path".into(), json!("written.txt"));
    write_args.insert("content".into(), json!("written via mcp"));
    write_args.insert("encoding".into(), json!("utf8"));
    let write = call_tool_value(&client, "workspace_write", Some(write_args)).await;
    assert_eq!(structured(&write)["ok"], true);
    assert_eq!(
        std::fs::read_to_string(harness.workspace_root.join("written.txt")).expect("file"),
        "written via mcp"
    );

    // config_export
    let cfg = call_tool_value(&client, "config_export", None).await;
    assert_eq!(structured(&cfg)["ok"], true);
    // Spec: export carries refs only, never secret values. Cheap belt-and-
    // suspenders check on the canonical TOML body.
    let toml_text = structured(&cfg)["data"]["toml"]
        .as_str()
        .unwrap_or_default();
    assert!(
        !toml_text.contains("acps_session_") && !toml_text.contains("acps_admin_"),
        "config export must not leak raw keys"
    );

    // permissions_pending
    let perms = call_tool_value(&client, "permissions_pending", None).await;
    assert_eq!(structured(&perms)["ok"], true);
    assert!(structured(&perms)["data"]["permissions"].is_array());

    // command_run — kept last because it is the slowest tool. Use `true`
    // so we don't depend on shell builtins or working directories.
    let mut cmd_args = serde_json::Map::new();
    cmd_args.insert("command".into(), json!("true"));
    let cmd = call_tool_value(&client, "command_run", Some(cmd_args)).await;
    assert_eq!(structured(&cmd)["ok"], true);

    let _ = client.cancel().await;

    // Every tool call landed an `api.request` row sourced from the local UDS.
    // We do not assert exact ordering — the daemon may interleave its own
    // bookkeeping events — but we do require every tool's path to show up
    // tagged as source=local.
    let rows = harness.api_request_paths().await;
    let paths: Vec<&str> = rows.iter().map(|(_, path)| path.as_str()).collect();
    let sources: Vec<&str> = rows.iter().map(|(source, _)| source.as_str()).collect();
    for expected in [
        "/v1/status",
        "/v1/security/check",
        "/v1/deps/check",
        "/v1/logs/events",
        "/v1/files",
        "/v1/files/content",
        "/v1/commands",
        "/v1/config/export",
        "/v1/permissions/pending",
    ] {
        assert!(
            paths.iter().any(|p| p.starts_with(expected)),
            "no api.request landed for {expected}: saw {paths:?}"
        );
    }
    assert!(
        sources
            .iter()
            .all(|s| *s == acp_stack::state::EVENT_SOURCE_LOCAL),
        "non-local source observed: {sources:?}"
    );
}

#[tokio::test]
async fn mcp_stdio_rejects_unknown_tool_with_invalid_params() {
    let harness = Harness::spawn().await;
    let client = spawn_stdio_client(&harness.socket).await;
    let params = CallToolRequestParams::new("secrets_get");
    let err = client
        .peer()
        .call_tool(params)
        .await
        .expect_err("unknown tool");
    // The dispatcher classifies unknown tools as invalid params; the rmcp
    // layer might also reject the call earlier via get_tool validation. Either
    // way it must NOT succeed.
    let msg = format!("{err:?}");
    assert!(
        msg.contains("unknown") || msg.contains("Unknown") || msg.contains("not found"),
        "unexpected error message: {msg}"
    );
    let _ = client.cancel().await;
}

#[tokio::test]
async fn mcp_http_uds_drives_status_with_source_local() {
    let harness = Harness::spawn().await;
    let bind_dir = tempfile::tempdir().expect("bind dir");
    let bind_path = bind_dir.path().join("acp-stack").join("acpctl-mcp.sock");

    let mut command = TokioCommand::new(acpctl_bin());
    command
        .arg("--socket")
        .arg(&harness.socket)
        .arg("mcp")
        .arg("serve")
        .arg("--transport")
        .arg("http-uds")
        .arg("--bind")
        .arg(&bind_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());

    let mut child = command.spawn().expect("spawn acpctl mcp http-uds");

    // Wait up to ~2s for the socket to materialize.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !bind_path.exists() {
        if std::time::Instant::now() > deadline {
            let _ = child.kill().await;
            panic!(
                "acpctl mcp http-uds socket never appeared at {}",
                bind_path.display()
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let bind_str = bind_path.to_string_lossy().into_owned();
    // The streamable HTTP server defaults to a loopback-only `Host` allowlist
    // (DNS-rebind protection). Talking via UDS, the Host header is otherwise
    // meaningless — using `localhost` keeps us inside the allowed set.
    let http_client = UnixSocketHttpClient::new(&bind_str, "http://localhost/");
    let transport = StreamableHttpClientTransport::with_client(
        http_client,
        StreamableHttpClientTransportConfig::with_uri("http://localhost/".to_string()),
    );
    let client = ().serve(transport).await.expect("rmcp http-uds init");

    let status = call_tool_value(&client, "status", None).await;
    assert_eq!(structured(&status)["ok"], true);

    let _ = client.cancel().await;
    let _ = child.kill().await;
    drop(bind_dir);

    let rows = harness.api_request_paths().await;
    assert!(
        rows.iter().any(
            |(source, path)| source == acp_stack::state::EVENT_SOURCE_LOCAL && path == "/v1/status"
        ),
        "no local-sourced /v1/status event landed; saw {rows:?}"
    );
}
