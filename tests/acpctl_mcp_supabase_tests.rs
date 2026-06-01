//! Acceptance test for the Phase 3 criterion:
//!
//! > SQLite remains the local source of truth when external logging is
//! > enabled.
//!
//! We bring up an in-process daemon, attach a Supabase sink pointed at a
//! fake HTTPS server, drive an MCP tool call through `acpctl mcp serve`,
//! then assert (a) SQLite contains the durable `api.request` row with
//! `source = "local"` (canonical), and (b) the outbox sink mirrored that
//! row to Supabase without leaking secret material.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use acp_stack::api::AppState;
use acp_stack::config::{
    Config, SupabaseLoggingBackend, SupabaseLoggingConfig, load_config_from_str,
};
use acp_stack::events::EventHub;
use acp_stack::local_listener;
use acp_stack::runtime::logging::supabase_sink::{SupabaseSink, SupabaseSinkCredential};
use acp_stack::state::{EventFilter, StateStore};
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::{RunningService, ServiceExt};
use rmcp::transport::TokioChildProcess;
use serde_json::Value;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::Command as TokioCommand;
use tokio::sync::Mutex as TokioMutex;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

const SESSION_KEY: &str = "acps_session_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const ADMIN_KEY: &str = "acps_admin_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

// --------------------------- Fake Supabase server ---------------------------
//
// Re-implements the minimal subset of `tests/supabase_sink_tests.rs` needed
// to capture POSTs. Kept inline rather than shared so this acceptance test
// is independently readable.

#[derive(Debug)]
struct CapturedRequest {
    path: String,
    body: String,
}

async fn start_fake_supabase() -> (String, mpsc::Receiver<CapturedRequest>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let local = listener.local_addr().expect("local addr");
    let url = format!("http://{local}");
    let (tx, rx) = mpsc::channel::<CapturedRequest>(128);
    tokio::spawn(async move {
        loop {
            let (stream, _peer) = match listener.accept().await {
                Ok(pair) => pair,
                Err(_) => break,
            };
            let tx = tx.clone();
            tokio::spawn(async move {
                if let Some(captured) = handle_one_request(stream).await {
                    let _ = tx.send(captured).await;
                }
            });
        }
    });
    (url, rx)
}

async fn handle_one_request(mut stream: TcpStream) -> Option<CapturedRequest> {
    let mut buf = vec![0u8; 32 * 1024];
    let mut total = 0usize;
    let header_end: usize = loop {
        let n = stream.read(&mut buf[total..]).await.ok()?;
        if n == 0 {
            return None;
        }
        total += n;
        if let Some(end) = buf[..total].windows(4).position(|w| w == b"\r\n\r\n") {
            break end;
        }
        if total >= buf.len() {
            return None;
        }
    };
    let header_str = std::str::from_utf8(&buf[..header_end]).ok()?;
    let mut lines = header_str.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let _method = parts.next()?;
    let path = parts.next()?.to_owned();
    let mut content_length = 0usize;
    for line in lines {
        if let Some((name, value)) = line.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            content_length = value.trim().parse().unwrap_or(0);
        }
    }
    let body_start = header_end + 4;
    let mut body = Vec::new();
    if body_start < total {
        body.extend_from_slice(&buf[body_start..total]);
    }
    while body.len() < content_length {
        let n = stream.read(&mut buf).await.ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&buf[..n]);
    }
    let body_str = String::from_utf8_lossy(&body[..content_length.min(body.len())]).into_owned();
    let response = "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
    Some(CapturedRequest {
        path,
        body: body_str,
    })
}

// ----------------------------- Daemon harness -------------------------------

struct Harness {
    socket: PathBuf,
    state: Arc<TokioMutex<StateStore>>,
    join: JoinHandle<acp_stack::error::Result<()>>,
    _workspace_tempdir: TempDir,
    _state_tempdir: TempDir,
    _socket_tempdir: TempDir,
}

impl Harness {
    async fn spawn_with_external_logging() -> Self {
        let workspace_tempdir = tempfile::tempdir().expect("workspace tempdir");
        let workspace_root = workspace_tempdir.path().to_path_buf();
        let uploads_root = workspace_root.join("uploads");
        std::fs::create_dir(&uploads_root).expect("uploads dir");

        let mut config = test_config();
        config.workspace.root = workspace_root.to_string_lossy().into_owned();
        config.workspace.uploads = uploads_root.to_string_lossy().into_owned();

        let state_tempdir = tempfile::tempdir().expect("state tempdir");
        let state_path = state_tempdir.path().join("state.sqlite");
        let mut store = StateStore::open(&state_path).expect("state open");
        store.migrate().expect("migrate");
        // Enable BEFORE we hand the store to the daemon so every event the
        // tool call lands also lands in the outbox.
        store.set_external_logging_enabled(true);

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
            state,
            join,
            _workspace_tempdir: workspace_tempdir,
            _state_tempdir: state_tempdir,
            _socket_tempdir: socket_tempdir,
        }
    }

    async fn api_request_rows(&self) -> Vec<(String, String)> {
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
    let toml_text = include_str!("fixtures/valid-placebo-stack.toml");
    load_config_from_str(toml_text).expect("config parses")
}

fn supabase_config(url: &str) -> SupabaseLoggingConfig {
    SupabaseLoggingConfig {
        enabled: true,
        backend: SupabaseLoggingBackend::Postgrest,
        url: url.to_owned(),
        table_prefix: String::new(),
        db_url_ref: None,
        api_key_ref: "SUPABASE_SECRET_KEY".to_owned(),
        schema: "acp_stack".to_owned(),
    }
}

fn acpctl_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_acpctl"))
}

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

    let proc = TokioChildProcess::new(command).expect("spawn acpctl mcp serve");
    ().serve(proc).await.expect("rmcp init handshake")
}

async fn call_tool_value(
    client: &RunningService<rmcp::RoleClient, ()>,
    name: &str,
) -> CallToolResult {
    let params = CallToolRequestParams::new(name.to_owned());
    tokio::time::timeout(Duration::from_secs(10), client.peer().call_tool(params))
        .await
        .expect("tool call did not time out")
        .expect("tool call succeeded")
}

async fn drain_at_least(
    rx: &mut mpsc::Receiver<CapturedRequest>,
    target: usize,
    deadline: Instant,
) -> Vec<CapturedRequest> {
    let mut out = Vec::new();
    while out.len() < target {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(captured)) => out.push(captured),
            Ok(None) => break,
            Err(_) => break,
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_sqlite_remains_source_of_truth_when_supabase_is_enabled() {
    let (sb_url, mut sb_rx) = start_fake_supabase().await;
    let harness = Harness::spawn_with_external_logging().await;

    let sink = SupabaseSink::spawn(
        harness.state.clone(),
        supabase_config(&sb_url),
        SupabaseSinkCredential::PostgrestApiKey("test-supabase-api-key".to_owned()),
        EventHub::new(),
    )
    .expect("sink spawn");

    // Drive one MCP tool call through `acpctl mcp serve` → daemon UDS.
    let client = spawn_stdio_client(&harness.socket).await;
    let status = call_tool_value(&client, "status").await;
    assert!(
        status.structured_content.is_some(),
        "tool returned structured"
    );
    let _ = client.cancel().await;

    // Wait for the sink to surface the events POST. Generous deadline because
    // the daemon → outbox path involves the SQLite outbox table + sink poll.
    // We loop until we see an /events POST whose body actually carries an
    // `api.request` row sourced from `local`, or the deadline expires.
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut captured: Vec<CapturedRequest> = Vec::new();
    let mirrored_status_row: Option<Value> = loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break None;
        }
        let mut batch = drain_at_least(&mut sb_rx, 1, Instant::now() + remaining).await;
        if batch.is_empty() {
            continue;
        }
        captured.append(&mut batch);

        let found = captured
            .iter()
            .filter(|req| req.path.ends_with("/rest/v1/events"))
            .find_map(|req| extract_mirrored_api_request_row(&req.body));
        if let Some(row) = found {
            break Some(row);
        }
    };
    sink.shutdown().await;

    // Assertion 1 — SQLite is canonical: the api.request row exists with
    // source=local regardless of whether Supabase responded successfully.
    let rows = harness.api_request_rows().await;
    assert!(
        rows.iter().any(|(source, path)| {
            source == acp_stack::state::EVENT_SOURCE_LOCAL && path == "/v1/status"
        }),
        "no local-sourced /v1/status row in SQLite; saw {rows:?}"
    );

    // Assertion 2 — Supabase received the mirrored row. We deliberately do
    // NOT assert payload.path here: `runtime::logging::sink_redaction::EVENTS_PAYLOAD_KEEP`
    // strips `path` from the outbox row to prevent route-shape leakage.
    // That redaction *is* a Phase 3 requirement; this assertion confirms it
    // by inspecting `payload_json` directly below.
    let row = mirrored_status_row
        .expect("supabase outbox did not mirror an api.request row within the deadline");
    assert_eq!(row["kind"].as_str(), Some("api.request"));
    assert_eq!(
        row["source"].as_str(),
        Some(acp_stack::state::EVENT_SOURCE_LOCAL)
    );
    let payload_obj = row
        .get("payload_json")
        .and_then(Value::as_object)
        .expect("mirrored row has payload_json as object");
    assert!(
        payload_obj.get("path").is_none(),
        "redaction did not strip `path` from mirrored payload_json: {payload_obj:?}"
    );

    // Assertion 3 — no secret values leaked through the outbox.
    for req in &captured {
        assert!(
            !req.body.contains(SESSION_KEY) && !req.body.contains(ADMIN_KEY),
            "outbox POST at {} leaked raw API key material: {}",
            req.path,
            req.body
        );
    }
}

/// Search a captured `/rest/v1/events` body for an api.request row with
/// `source = local`, returning the row JSON if it appears. The sink batches
/// rows into one POST, so the body is a JSON array of event objects.
fn extract_mirrored_api_request_row(body: &str) -> Option<Value> {
    let parsed: Value = serde_json::from_str(body).ok()?;
    let rows = parsed.as_array()?;
    rows.iter()
        .find(|row| {
            let kind = row.get("kind").and_then(Value::as_str);
            let source = row.get("source").and_then(Value::as_str);
            kind == Some("api.request") && source == Some(acp_stack::state::EVENT_SOURCE_LOCAL)
        })
        .cloned()
}
