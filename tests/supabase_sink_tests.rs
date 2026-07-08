//! Integration test for the Supabase logging sink. Brings up a hand-rolled
//! HTTP/1.1 server on a random local port, points the sink at it, enqueues
//! source rows, and asserts batching, header shape, redaction (no secrets
//! escape), and the merge-duplicates header used for idempotent replay.

use std::sync::Arc;
use std::time::{Duration, Instant};

use acp_stack::config::{SupabaseLoggingBackend, SupabaseLoggingConfig};
use acp_stack::events::EventHub;
use acp_stack::runtime::logging::supabase_sink::{SupabaseSink, SupabaseSinkCredential};
use acp_stack::state::{NewCommandRecord, NewSessionRecord, StateStore};
use tempfile::tempdir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex as TokioMutex;
use tokio::sync::mpsc;

/// One captured request observed by the fake Supabase server. Used by the
/// assertions to verify headers, body, and per-table grouping without
/// re-parsing HTTP on the test side.
#[derive(Debug, Clone)]
struct CapturedRequest {
    path: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl CapturedRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Scripted response sequence: returns the configured status for the Nth
/// request, then sticks on the last entry. Lets a test simulate
/// 503-then-200 or 401 (permanent failure) etc.
#[derive(Clone)]
struct ResponsePlan {
    statuses: Vec<u16>,
}

impl ResponsePlan {
    fn fixed_200() -> Self {
        Self {
            statuses: vec![200],
        }
    }

    fn status_for(&self, request_index: usize) -> u16 {
        if request_index < self.statuses.len() {
            self.statuses[request_index]
        } else {
            *self.statuses.last().expect("plan must be non-empty")
        }
    }
}

async fn start_fake_supabase(plan: ResponsePlan) -> (String, mpsc::Receiver<CapturedRequest>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let local = listener.local_addr().expect("local addr");
    let url = format!("http://{local}");
    let (tx, rx) = mpsc::channel::<CapturedRequest>(128);
    tokio::spawn(async move {
        let mut request_index = 0usize;
        loop {
            let (stream, _peer) = match listener.accept().await {
                Ok(pair) => pair,
                Err(_) => break,
            };
            let plan = plan.clone();
            let tx = tx.clone();
            let idx = request_index;
            request_index += 1;
            tokio::spawn(async move {
                if let Some(captured) = handle_one_request(stream, plan.status_for(idx)).await {
                    let _ = tx.send(captured).await;
                }
            });
        }
    });
    (url, rx)
}

async fn handle_one_request(mut stream: TcpStream, status: u16) -> Option<CapturedRequest> {
    // Buffer up to a generous fixed limit; tests never send more than a few
    // KB of JSON. Parse headers, then read Content-Length bytes for the body.
    let mut buf = vec![0u8; 16 * 1024];
    let mut total = 0usize;
    let header_end: usize = loop {
        let n = stream.read(&mut buf[total..]).await.ok()?;
        if n == 0 {
            return None;
        }
        total += n;
        if let Some(end) = find_double_crlf(&buf[..total]) {
            break end;
        }
        if total >= buf.len() {
            return None;
        }
    };
    let header_str = std::str::from_utf8(&buf[..header_end]).ok()?;
    let mut lines = header_str.split("\r\n");
    let request_line = lines.next()?;
    let mut request_parts = request_line.split_whitespace();
    let _method = request_parts.next()?;
    let path = request_parts.next()?.to_owned();
    let mut headers: Vec<(String, String)> = Vec::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_owned(), value.trim().to_owned()));
        }
    }
    let content_length: usize = headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
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
    let response =
        format!("HTTP/1.1 {status} OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
    Some(CapturedRequest {
        path,
        headers,
        body: body_str,
    })
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

async fn fresh_store_with_external_logging() -> (tempfile::TempDir, Arc<TokioMutex<StateStore>>) {
    let dir = tempdir().expect("tempdir");
    let mut store = StateStore::open(dir.path().join("state.sqlite")).expect("open");
    store.migrate().expect("migrate");
    store.set_external_logging_enabled(true);
    (dir, Arc::new(TokioMutex::new(store)))
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

async fn drain_at_least(
    rx: &mut mpsc::Receiver<CapturedRequest>,
    target: usize,
) -> Vec<CapturedRequest> {
    let deadline = Instant::now() + Duration::from_secs(10);
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
async fn happy_path_uploads_grouped_batches_and_marks_sent() {
    let (url, mut rx) = start_fake_supabase(ResponsePlan::fixed_200()).await;
    let (_dir, state) = fresh_store_with_external_logging().await;
    let event_hub = EventHub::new();
    let sink = SupabaseSink::spawn(
        state.clone(),
        supabase_config(&url),
        SupabaseSinkCredential::PostgrestApiKey("test-supabase-api-key".to_owned()),
        event_hub,
    )
    .expect("sink spawn");

    // Enqueue rows across three source tables. The sink groups by table and
    // issues one POST per group.
    {
        let guard = state.lock().await;
        guard
            .insert_session(NewSessionRecord {
                id: "sess_1".to_owned(),
                agent_id: "agent_a".to_owned(),
                cwd: "/var/secrets/repo".to_owned(),
                title: Some("secret meeting notes".to_owned()),
                metadata_json: r#"{"agent_id":"agent_a","internal":"sk-meta"}"#.to_owned(),
            })
            .expect("insert session");
        guard
            .append_event("info", "test.kind", "hello", "{}")
            .expect("append event");
        guard
            .append_event("info", "test.kind", "world", "{}")
            .expect("append event 2");
        guard
            .append_auth_failure(
                "session",
                "bad-key",
                Some("127.0.0.1"),
                Some("/v1/secrets/sk-route-secret"),
                "{}",
            )
            .expect("append auth failure");
        guard
            .append_command(NewCommandRecord {
                command: "printf hello",
                cwd: Some("/var/secrets/repo"),
                env_json: Some(r#"{"TOKEN":"sk-command"}"#),
                origin: acp_stack::state::CommandOrigin::Acp,
                session_id: Some("sess_1"),
            })
            .expect("append command");
    }

    let captured = drain_at_least(&mut rx, 4).await;
    sink.shutdown().await;

    assert!(
        captured.len() >= 4,
        "expected at least 4 batched POSTs, got {}",
        captured.len()
    );

    // Every request must include the merge-duplicates header.
    for req in &captured {
        let prefer = req.header("Prefer").expect("prefer header present");
        assert!(prefer.contains("merge-duplicates"), "prefer was {prefer}");
        assert_eq!(req.header("Content-Profile"), Some("acp_stack"));
        let apikey = req.header("Apikey").expect("apikey header present");
        assert_eq!(apikey, "test-supabase-api-key");
        assert_eq!(req.header("Authorization"), None);
    }

    // The events POST should target /rest/v1/events and contain both event rows
    // in a single JSON array (batched).
    let events_req = captured
        .iter()
        .find(|r| r.path.ends_with("/rest/v1/events"))
        .expect("events POST present");
    let parsed: serde_json::Value =
        serde_json::from_str(&events_req.body).expect("body parses as JSON");
    let arr = parsed.as_array().expect("events body is array");
    assert_eq!(arr.len(), 2, "expected 2 events in one batched POST");

    let auth_req = captured
        .iter()
        .find(|r| r.path.ends_with("/rest/v1/auth_failures"))
        .expect("auth_failures POST present");
    let auth_parsed: serde_json::Value = serde_json::from_str(&auth_req.body).expect("body parses");
    assert_eq!(auth_parsed.as_array().map(|a| a.len()), Some(1));
    assert!(
        !auth_req.body.contains("sk-route-secret"),
        "auth failure route leaked: {body}",
        body = auth_req.body
    );
    let auth_row = auth_parsed
        .as_array()
        .and_then(|rows| rows.first())
        .expect("auth failure row present");
    assert!(auth_row.get("route").map(|v| v.is_null()).unwrap_or(false));

    let sessions_req = captured
        .iter()
        .find(|r| r.path.ends_with("/rest/v1/sessions"))
        .expect("sessions POST present");
    let sessions_parsed: serde_json::Value =
        serde_json::from_str(&sessions_req.body).expect("body parses");
    let session = sessions_parsed
        .as_array()
        .and_then(|rows| rows.first())
        .expect("session row present");
    assert_eq!(
        session.get("target_id").and_then(|v| v.as_str()),
        Some("agent_a")
    );
    assert_eq!(
        session.get("agent_session_id").and_then(|v| v.as_str()),
        Some("sess_1")
    );
    assert_eq!(session.get("cwd").and_then(|v| v.as_str()), Some(""));
    assert!(session.get("title").and_then(|v| v.as_str()).is_none());
    assert!(
        !sessions_req.body.contains("/var/secrets/repo"),
        "session cwd leaked: {body}",
        body = sessions_req.body
    );
    assert!(
        !sessions_req.body.contains("secret meeting notes"),
        "session title leaked: {body}",
        body = sessions_req.body
    );
    let commands_req = captured
        .iter()
        .find(|r| r.path.ends_with("/rest/v1/commands"))
        .expect("commands POST present");
    let commands_parsed: serde_json::Value =
        serde_json::from_str(&commands_req.body).expect("body parses");
    let command = commands_parsed
        .as_array()
        .and_then(|rows| rows.first())
        .expect("command row present");
    assert_eq!(
        command.get("output_bytes").and_then(|v| v.as_i64()),
        Some(0)
    );
    assert!(
        command
            .get("last_output_event_id")
            .map(|v| v.is_null())
            .unwrap_or(false)
    );
    // The ACP-vs-operator provenance must survive into the external mirror.
    assert_eq!(command.get("origin").and_then(|v| v.as_str()), Some("acp"));
    assert_eq!(
        command.get("session_id").and_then(|v| v.as_str()),
        Some("sess_1")
    );

    // Local outbox state: all rows must be marked sent.
    let pending_after = state
        .lock()
        .await
        .next_sink_outbox_batch(100, "2099-01-01T00:00:00Z")
        .expect("query");
    assert!(
        pending_after.is_empty(),
        "outbox must be drained, got {pending_after:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn payload_never_contains_plaintext_secret() {
    let (url, mut rx) = start_fake_supabase(ResponsePlan::fixed_200()).await;
    let (_dir, state) = fresh_store_with_external_logging().await;
    let event_hub = EventHub::new();
    let sink = SupabaseSink::spawn(
        state.clone(),
        supabase_config(&url),
        SupabaseSinkCredential::PostgrestApiKey("test-supabase-api-key".to_owned()),
        event_hub,
    )
    .expect("sink spawn");

    // The payload here mimics a buggy upstream that stuffed a token into an
    // events row. The redactor must drop the offending keys before the body
    // ever leaves the daemon.
    {
        let guard = state.lock().await;
        guard
            .append_event(
                "info",
                "test.kind",
                "leaky",
                r#"{"api_key":"sk-leak","Authorization":"Bearer leak","session_id":"sess_1"}"#,
            )
            .expect("append leaky event");
    }

    let captured = drain_at_least(&mut rx, 1).await;
    sink.shutdown().await;

    assert!(!captured.is_empty(), "expected at least one POST");
    for req in &captured {
        assert!(
            !req.body.contains("sk-leak"),
            "secret leaked: {body}",
            body = req.body
        );
        assert!(
            !req.body.contains("api_key"),
            "field name leaked: {body}",
            body = req.body
        );
        assert!(
            !req.body.contains("Bearer leak"),
            "bearer leaked: {body}",
            body = req.body
        );
        // session_id is on the allowlist and must survive.
        assert!(
            req.body.contains("sess_1"),
            "session_id should pass through: {body}",
            body = req.body
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn permanent_4xx_failure_does_not_retry_immediately() {
    let (url, mut rx) = start_fake_supabase(ResponsePlan {
        statuses: vec![401],
    })
    .await;
    let (_dir, state) = fresh_store_with_external_logging().await;
    let event_hub = EventHub::new();
    let sink = SupabaseSink::spawn(
        state.clone(),
        supabase_config(&url),
        SupabaseSinkCredential::PostgrestApiKey("test-supabase-api-key".to_owned()),
        event_hub,
    )
    .expect("sink spawn");

    {
        let guard = state.lock().await;
        guard
            .append_event("info", "test.kind", "hello", "{}")
            .expect("append");
    }

    // Wait for the first failure to be observed.
    let captured = drain_at_least(&mut rx, 1).await;
    assert_eq!(captured.len(), 1, "401 must not trigger immediate retries");

    // Give the sink a couple of poll cycles; assert no further requests land.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let post_failure: Vec<CapturedRequest> = drain_at_least(&mut rx, 1).await;
    assert!(
        post_failure.is_empty(),
        "401 should park retry far in the future, got {post_failure:?}"
    );
    sink.shutdown().await;

    let count = state.lock().await.sink_open_failure_count().expect("count");
    assert_eq!(count, 1, "outbox should hold one failed row");
}
