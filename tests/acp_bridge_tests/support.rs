use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use acp_stack::config::{AgentConfig, AgentInstallConfig};
use acp_stack::runtime::agent::acp_bridge::{AcpBridge, AcpPermissionPolicy, SessionEventSink};

#[derive(Default)]
pub(crate) struct CapturedEvent {
    pub(crate) session_id: String,
    pub(crate) kind: String,
    pub(crate) payload: String,
}

#[derive(Default)]
pub(crate) struct InMemorySink {
    pub(crate) events: Mutex<Vec<CapturedEvent>>,
}

impl SessionEventSink for InMemorySink {
    fn append<'a>(
        &'a self,
        session_id: &'a str,
        kind: &'a str,
        payload_json: &'a str,
    ) -> futures::future::BoxFuture<'a, ()> {
        Box::pin(async move {
            self.events.lock().expect("sink lock").push(CapturedEvent {
                session_id: session_id.to_owned(),
                kind: kind.to_owned(),
                payload: payload_json.to_owned(),
            });
        })
    }
}

pub(crate) fn null_sink() -> Arc<dyn SessionEventSink> {
    Arc::new(InMemorySink::default())
}

pub(crate) fn fake_agent_config() -> AgentConfig {
    AgentConfig {
        id: "fake".into(),
        name: "fake".into(),
        command: env!("CARGO_BIN_EXE_placebo-agent").into(),
        args: vec!["acp".into()],
        cwd: None,
        env: vec![],
        expected_sha256: None,
        restart: "never".into(),
        mode: None,
        model: None,
        harness_version: None,
        adapter: None,
        provider: None,
        providers: None,
        subagent: None,
        auto_update: None,
        install: Some(AgentInstallConfig {
            install_type: "shell".into(),
            creates: "true".into(),
            shell: Some("true".into()),
        }),
    }
}

pub(crate) fn fake_env() -> HashMap<String, String> {
    HashMap::new()
}

pub(crate) const RESOURCE_NOT_FOUND_CODE: i64 = -32002;
pub(crate) const INVALID_PARAMS_CODE: i64 = -32602;

/// Run a prompt against a placebo configured with `terminal_flags` and return
/// (report, bridge, sink). The bridge is still running so callers can assert
/// shutdown behavior; most tests just shut it down.
pub(crate) async fn run_terminal_probe(
    terminal_flags: &[&str],
    command_log: Option<acp_stack::runtime::agent::acp_bridge::TerminalCommandLog>,
) -> (serde_json::Value, AcpBridge, Arc<InMemorySink>) {
    use agent_client_protocol::schema::v1::{ContentBlock, PromptRequest, TextContent};
    let mut config = fake_agent_config();
    config
        .args
        .extend(terminal_flags.iter().map(|s| s.to_string()));
    let sink = Arc::new(InMemorySink::default());
    let sink_dyn: Arc<dyn SessionEventSink> = sink.clone();
    let bridge = AcpBridge::spawn(
        &config,
        fake_env(),
        std::env::temp_dir(),
        sink_dyn,
        AcpPermissionPolicy::Cancel,
        &Default::default(),
        None,
        command_log,
    )
    .await
    .expect("spawn");
    let session = bridge
        .new_session(std::env::temp_dir(), vec![])
        .await
        .expect("session/new");
    let prompt = PromptRequest::new(
        session.session_id.clone(),
        vec![ContentBlock::Text(TextContent::new("run terminal probe"))],
    );
    bridge.prompt_session(prompt).await.expect("session/prompt");

    // Notification persistence goes through a spawned task inside the sink;
    // poll briefly for the report chunk instead of assuming ordering.
    let mut report = None;
    for _ in 0..100 {
        {
            let events = sink.events.lock().expect("sink lock");
            for event in events.iter() {
                if let Some(index) = event.payload.find("terminal-report:") {
                    let tail = &event.payload[index + "terminal-report:".len()..];
                    // The report JSON is embedded inside a JSON string field;
                    // decode by re-parsing the payload and extracting the text.
                    let payload: serde_json::Value =
                        serde_json::from_str(&event.payload).expect("payload parses");
                    let text = find_terminal_report_text(&payload)
                        .unwrap_or_else(|| panic!("report text missing in {tail}"));
                    let json = text
                        .strip_prefix("terminal-report:")
                        .expect("report prefix");
                    report = Some(serde_json::from_str(json).expect("report parses"));
                    break;
                }
            }
        }
        if report.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let report = report.expect("placebo emitted a terminal-report chunk");
    (report, bridge, sink)
}

/// Recursively find the string value carrying the terminal report.
fn find_terminal_report_text(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(text) if text.starts_with("terminal-report:") => {
            Some(text.clone())
        }
        serde_json::Value::Object(map) => map.values().find_map(find_terminal_report_text),
        serde_json::Value::Array(items) => items.iter().find_map(find_terminal_report_text),
        _ => None,
    }
}

pub(crate) fn open_test_state() -> (
    tempfile::TempDir,
    Arc<tokio::sync::Mutex<acp_stack::state::StateStore>>,
) {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let store =
        acp_stack::state::StateStore::open(tempdir.path().join("state.sqlite")).expect("open");
    store.migrate().expect("migrate");
    (tempdir, Arc::new(tokio::sync::Mutex::new(store)))
}
