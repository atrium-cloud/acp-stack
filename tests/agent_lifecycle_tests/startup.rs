use std::time::Duration;

use futures::{SinkExt, StreamExt};
use reqwest::StatusCode;
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::common::agent::{
    AgentHarness, admin_bearer, http, session_bearer, shell_quote_path, test_config,
    websocket_request,
};

#[tokio::test]
async fn registry_install_does_not_require_runtime_secret_store() {
    let mut config = test_config();
    let command = config.agent.command.clone();
    config.agent.install = None;
    config.agent.env = vec!["OPENCODE_API_KEY".to_owned()];
    let tempdir = TempDir::new().expect("tempdir");
    let workspace_root = tempdir.path().join("workspace");
    std::fs::create_dir(&workspace_root).expect("workspace dir");
    config.workspace.root = workspace_root.to_string_lossy().into_owned();
    config.workspace.uploads = workspace_root
        .join("uploads")
        .to_string_lossy()
        .into_owned();
    let binary_path = tempdir
        .path()
        .join(".local")
        .join("bin")
        .join("registry-agent");
    let script = format!(
        "mkdir -p {bin} && printf '#!/bin/sh\\n' > {binary} && chmod 755 {binary}",
        bin = shell_quote_path(binary_path.parent().expect("binary has parent")),
        binary = shell_quote_path(&binary_path),
    );
    config.agent.command = "registry-agent".to_owned();
    config.agent.args = Vec::new();
    let override_dir = tempdir.path().join(".config").join("acp-stack");
    std::fs::create_dir_all(&override_dir).expect("override dir");
    std::fs::write(
        override_dir.join("agents.toml"),
        format!(
            r#"
[[agents]]
id = "opencode"
name = "OpenCode Test"
kind = "native"
headless_compatible = true
support_doc = "docs/agents/opencode.md"

[agents.harness]
id = "opencode"

[agents.harness.install.shell]
script = {script:?}
creates = "registry-agent"
"#
        ),
    )
    .expect("override registry");
    let harness =
        AgentHarness::spawn_with_config_and_home(config, tempdir.path().to_path_buf()).await;
    let response = http()
        .await
        .post(format!("{}/v1/agent/install", harness.base_url))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("send install");
    let status = response.status();
    let body: Value = response.json().await.expect("install json");
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["outcome"], "installed");
    assert_eq!(body["data"]["path"], binary_path.to_string_lossy().as_ref());
    assert_eq!(command, env!("CARGO_BIN_EXE_placebo-agent"));
}

#[tokio::test]
async fn install_then_start_then_capabilities_then_stop() {
    // The version capture resolves the launch shape from the registry, so the test agent must be
    // adapter-kind there, matching the adapter metadata `test_config` hand-sets.
    let tempdir = TempDir::new().expect("tempdir");
    let override_dir = tempdir.path().join(".config").join("acp-stack");
    std::fs::create_dir_all(&override_dir).expect("override dir");
    std::fs::write(
        override_dir.join("agents.toml"),
        r#"
[[agents]]
id = "opencode"
name = "OpenCode Test"
kind = "adapter"
headless_compatible = true
support_doc = "docs/agents/opencode.md"

[agents.adapter]
id = "codex-acp"
github = "agentclientprotocol/codex-acp"

[agents.adapter.install.npm]
package = "@agentclientprotocol/codex-acp"
creates = "codex-acp"

[agents.harness]
id = "opencode"

[agents.harness.install.npm]
package = "opencode-ai"
creates = "opencode"
"#,
    )
    .expect("override registry");
    let harness =
        AgentHarness::spawn_with_config_and_home(test_config(), tempdir.path().to_path_buf()).await;
    let client = http().await;

    // The fake config's `shell`/`creates` both resolve to /usr/bin/true on
    // every test host, so precheck wins with `already_present`.
    let response = client
        .post(format!("{}/v1/agent/install", harness.base_url))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("send install");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("install json");
    assert_eq!(body["ok"], true);
    let outcome = body["data"]["outcome"].as_str().expect("outcome present");
    assert!(matches!(outcome, "installed" | "already_present"));

    // The fake install records no versions (shell recipe); seed versioned rows so the
    // capture at start has something to pair the handshake with.
    {
        let store = harness.state.lock().await;
        for (started_at, step, version) in [
            ("2026-05-21T00:00:00.000000000Z", "harness", "1.2.3"),
            ("2026-05-21T00:00:01.000000000Z", "adapter", "0.4.0"),
        ] {
            store
                .append_installer_run(acp_stack::state::InstallerRunInput {
                    agent_id: "opencode",
                    started_at,
                    finished_at: Some(started_at),
                    status: "ran",
                    stdout: "",
                    stderr: "",
                    exit_status: Some(0),
                    step,
                    version: Some(version),
                    operation: "install",
                    method: None,
                    log_dir: None,
                    apply_run_id: None,
                })
                .expect("seed installer run");
        }
    }

    let start = client
        .post(format!("{}/v1/agent/start", harness.base_url))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("send start");
    if start.status() != StatusCode::OK {
        let body = start.text().await.unwrap_or_default();
        panic!("start failed: {body}");
    }

    let caps = client
        .get(format!("{}/v1/agent/capabilities", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("send caps");
    assert_eq!(caps.status(), StatusCode::OK);
    let caps_body: Value = caps.json().await.expect("caps json");
    assert_eq!(caps_body["data"]["agent_id"], "opencode");
    assert_eq!(caps_body["data"]["adapter"]["id"], "codex-acp");
    assert_eq!(
        caps_body["data"]["adapter"]["source_url"],
        "https://github.com/agentclientprotocol/codex-acp"
    );
    assert_eq!(caps_body["data"]["capabilities"]["protocol_version"], 1);
    assert_eq!(caps_body["data"]["capabilities"]["agent_id"], "opencode");
    assert_eq!(
        caps_body["data"]["capabilities"]["harness_version"],
        "1.2.3"
    );
    assert_eq!(caps_body["data"]["capabilities"]["adapter_id"], "codex-acp");
    assert_eq!(
        caps_body["data"]["capabilities"]["adapter_version"],
        "0.4.0"
    );

    let stop = client
        .post(format!("{}/v1/agent/stop", harness.base_url))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("send stop");
    assert_eq!(stop.status(), StatusCode::OK);

    let store = harness.state.lock().await;
    let lifecycle = store.query_agent_lifecycle(20).expect("lifecycle query");
    drop(store);
    let kinds: Vec<&str> = lifecycle.iter().map(|r| r.event_kind.as_str()).collect();
    assert!(kinds.contains(&"agent.starting"), "kinds: {kinds:?}");
    assert!(kinds.contains(&"agent.started"), "kinds: {kinds:?}");
    assert!(kinds.contains(&"agent.stopped"), "kinds: {kinds:?}");
    let started = lifecycle
        .iter()
        .find(|r| r.event_kind == "agent.started")
        .expect("agent.started row");
    let payload: Value = serde_json::from_str(&started.payload_json).expect("started payload json");
    assert_eq!(payload["adapter"]["id"], "codex-acp");
    assert_eq!(
        payload["adapter"]["source_url"],
        "https://github.com/agentclientprotocol/codex-acp"
    );
}

#[tokio::test]
async fn websocket_streams_agent_lifecycle_topic() {
    let harness = AgentHarness::spawn().await;
    let (mut ws, response) = tokio_tungstenite::connect_async(websocket_request(&harness))
        .await
        .expect("websocket connects");
    assert_eq!(response.status().as_u16(), 101);
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        json!({
            "type": "subscribe",
            "topics": ["agent.lifecycle"]
        })
        .to_string()
        .into(),
    ))
    .await
    .expect("subscribe");

    let client = http().await;
    let start = client
        .post(format!("{}/v1/agent/start", harness.base_url))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("send start");
    assert_eq!(start.status(), StatusCode::OK);

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
            && event["topic"] == "agent.lifecycle"
            && event["payload"]["kind"] == "agent.started"
        {
            received = Some(event);
            break;
        }
    }
    let event = received.expect("agent.started lifecycle websocket event");
    assert!(event["id"].as_str().unwrap_or("").starts_with("agl_"));
}

#[tokio::test]
async fn session_key_rejected_on_admin_routes() {
    let harness = AgentHarness::spawn().await;
    let client = http().await;

    for path in ["/v1/agent/install", "/v1/agent/start", "/v1/agent/stop"] {
        let response = client
            .post(format!("{}{}", harness.base_url, path))
            .header("Authorization", session_bearer())
            .send()
            .await
            .expect("send");
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "{path} should reject session key"
        );
        let body: Value = response.json().await.expect("json");
        assert_eq!(body["error"]["code"], "auth.wrong_kind");
    }
}

#[tokio::test]
async fn capabilities_returns_404_until_first_start() {
    let harness = AgentHarness::spawn().await;
    let client = http().await;

    let response = client
        .get(format!("{}/v1/agent/capabilities", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "agent.not_initialized");
}
