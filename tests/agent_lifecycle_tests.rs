#![cfg(feature = "test-fixtures")]

//! End-to-end coverage for the agent lifecycle HTTP routes: install, start,
//! capabilities, restart, stop, provider/model discovery, the array target
//! routes, and the session/admin tier enforcement on those.
//!
//! All tests drive a real `acps` HTTP server against a `Config` whose
//! `[agent].command` is the standalone placebo ACP fixture.

use std::time::Duration;

use acp_stack::config::{ArrayTargetConfig, Config};
use futures::{SinkExt, StreamExt};
use reqwest::StatusCode;
use serde_json::{Value, json};
use tempfile::TempDir;

mod common;
use common::HomeEnvGuard;
use common::agent::{
    AgentHarness, EnvVarGuard, add_codex_placebo_target, admin_bearer, http, session_bearer,
    shell_quote_path, test_config, websocket_request,
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
        "mkdir -p {bin} && printf registry > {binary} && chmod 755 {binary}",
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
    let _home_guard = HomeEnvGuard::set(tempdir.path());
    let harness = AgentHarness::spawn_with_config(config).await;
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
    let harness = AgentHarness::spawn().await;
    let client = http().await;

    // Install — admin key required. The fake config uses `shell = "true"`
    // and `creates = "true"`, which both resolve in /usr/bin on every test
    // host; we expect `already_present` since precheck wins.
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

    // Start — agent process spawns and ACP `initialize` returns.
    let start = client
        .post(format!("{}/v1/agent/start", harness.base_url))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("send start");
    if start.status() != StatusCode::OK {
        // Surface the body to make CI failures actionable.
        let body = start.text().await.unwrap_or_default();
        panic!("start failed: {body}");
    }

    // Capabilities — session key, returns the persisted snapshot.
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

    // Stop.
    let stop = client
        .post(format!("{}/v1/agent/stop", harness.base_url))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("send stop");
    assert_eq!(stop.status(), StatusCode::OK);

    // Lifecycle rows captured the trail.
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

#[tokio::test]
async fn providers_lists_supported_providers_for_configured_agent() {
    // Test config uses agent id `opencode`. The embedded provider
    // mapping lists openai, anthropic, openrouter, etc. as supported
    // for opencode. The endpoint should return those without spawning
    // the agent — it's pure embedded-mapping lookup.
    let harness = AgentHarness::spawn().await;
    let client = http().await;

    let response = client
        .get(format!("{}/v1/providers", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("providers json");
    assert_eq!(body["ok"], true);
    assert_eq!(body["data"]["agent_id"], "opencode");
    let providers = body["data"]["providers"]
        .as_array()
        .expect("providers array");
    assert!(
        !providers.is_empty(),
        "embedded mapping lists providers for opencode",
    );
    // Each provider entry has at least an id and a name.
    for provider in providers {
        assert!(
            provider["id"].as_str().is_some(),
            "missing id on {provider:?}",
        );
        assert!(
            provider["name"].as_str().is_some(),
            "missing name on {provider:?}",
        );
    }
}

#[tokio::test]
async fn providers_follow_default_target_changed_on_disk() {
    let mut config = test_config();
    config.array.enabled = true;
    add_codex_placebo_target(&mut config);
    let harness = AgentHarness::spawn_with_config(config).await;

    let mut updated =
        Config::load_from_path(&harness.config_path).expect("config should load from disk");
    let codex_agent = updated
        .array
        .target("codex")
        .expect("codex target exists")
        .agent
        .clone();
    updated.array.primary_target = "codex".to_owned();
    updated.agent = codex_agent;
    std::fs::write(
        &harness.config_path,
        updated.to_canonical_toml().expect("canonical config"),
    )
    .expect("config should be rewritten");

    let response = http()
        .await
        .get(format!("{}/v1/providers", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("send providers");
    let status = response.status();
    let body: Value = response.json().await.expect("providers json");

    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["agent_id"], "codex");
}

#[tokio::test]
async fn providers_requires_session_key() {
    let harness = AgentHarness::spawn().await;
    let client = http().await;
    let response = client
        .get(format!("{}/v1/providers", harness.base_url))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn models_returns_fixture_advertised_values() {
    // Drive the model-discovery path entirely from a fixture file so
    // the test doesn't spawn the real agent binary. The
    // ACP_STACK_AGENT_CONFIG_OPTIONS_PATH env var is the same seam
    // the CLI uses — see runtime::model_discovery for details.
    let tempdir = TempDir::new().expect("tempdir");
    let fixture_path = tempdir.path().join("config-options.json");
    // Mirrors `tests/common/cli.rs::write_acp_config_options` shape so
    // the fixture round-trips through the same SessionConfigOption
    // deserializer the CLI tests rely on.
    let fixture_body = serde_json::json!([
        {
            "id": "model",
            "name": "Model",
            "category": "model",
            "type": "select",
            "currentValue": "openai/gpt-4o",
            "options": [
                { "value": "openai/gpt-4o", "name": "openai/gpt-4o" },
                { "value": "anthropic/claude-3-5-sonnet", "name": "anthropic/claude-3-5-sonnet" }
            ]
        },
        {
            "id": "mode",
            "name": "Mode",
            "category": "mode",
            "type": "select",
            "currentValue": "default",
            "options": [
                { "value": "default", "name": "default" },
                { "value": "yolo", "name": "yolo" }
            ]
        }
    ]);
    std::fs::write(&fixture_path, fixture_body.to_string()).expect("write fixture");

    let _fixture_guard = EnvVarGuard::set("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &fixture_path);

    let harness = AgentHarness::spawn().await;
    let client = http().await;
    // /v1/models is a session-tier discovery route.
    let response = client
        .get(format!("{}/v1/models", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("send");

    let status = response.status();
    let body_text = response.text().await.unwrap_or_default();
    assert_eq!(status, StatusCode::OK, "body: {body_text}");
    let body: Value = serde_json::from_str(&body_text).expect("models json");
    assert_eq!(body["ok"], true);
    assert_eq!(body["data"]["agent_id"], "opencode");
    let models = body["data"]["models"].as_array().expect("models array");
    assert!(
        models.iter().any(|m| m.as_str() == Some("openai/gpt-4o")),
        "advertised model values missing: {models:?}",
    );
    let modes = body["data"]["modes"].as_array().expect("modes array");
    assert!(
        modes.iter().any(|m| m.as_str() == Some("default")),
        "advertised mode values missing: {modes:?}",
    );
}

#[tokio::test]
async fn models_rejects_admin_key() {
    // Strict tiering has no admin-key superset behavior; session-tier
    // routes reject valid admin keys with auth.wrong_kind.
    let harness = AgentHarness::spawn().await;
    let client = http().await;
    let response = client
        .get(format!("{}/v1/models", harness.base_url))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "auth.wrong_kind");
}

#[cfg(unix)]
fn kill_process(pid: u32) {
    let result = unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
    let result = if result == 0 {
        result
    } else {
        unsafe { libc::kill(pid as i32, libc::SIGKILL) }
    };
    assert_eq!(result, 0, "failed to SIGKILL fake agent pid {pid}");
}

#[cfg(unix)]
fn read_fake_agent_pid(path: &std::path::Path) -> u32 {
    std::fs::read_to_string(path)
        .expect("fake agent pid file")
        .trim()
        .parse()
        .expect("fake agent pid parses")
}

#[cfg(unix)]
async fn wait_for_agent_status(
    client: &reqwest::Client,
    base_url: &str,
    predicate: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let body: Value = client
            .get(format!("{base_url}/v1/agent/status"))
            .header("Authorization", session_bearer())
            .send()
            .await
            .expect("status")
            .json()
            .await
            .expect("status json");
        if predicate(&body["data"]) {
            return body["data"].clone();
        }
        if std::time::Instant::now() > deadline {
            panic!("agent status did not reach expected state; last body: {body}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[cfg(unix)]
async fn start_agent_for_crash_test(client: &reqwest::Client, base_url: &str) -> u32 {
    let response = client
        .post(format!("{base_url}/v1/agent/start"))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("start");
    let status = response.status();
    let body: Value = response.json().await.expect("start json");
    assert_eq!(status, StatusCode::OK, "body: {body}");
    body["data"]["pid"]
        .as_u64()
        .and_then(|pid| u32::try_from(pid).ok())
        .expect("start response pid")
}

#[cfg(unix)]
async fn create_session_for_crash_test(client: &reqwest::Client, base_url: &str) -> String {
    let response = client
        .post(format!("{base_url}/v1/sessions"))
        .header("Authorization", session_bearer())
        .json(&json!({}))
        .send()
        .await
        .expect("create session");
    let status = response.status();
    let body: Value = response.json().await.expect("create session json");
    assert_eq!(status, StatusCode::OK, "body: {body}");
    body["data"]["id"].as_str().expect("session id").to_owned()
}

#[cfg(unix)]
#[tokio::test]
async fn on_crash_policy_restarts_agent_and_allows_session_resume() {
    let tempdir = TempDir::new().expect("tempdir");
    let pid_path = tempdir.path().join("placebo-agent.pid");
    let mut config = test_config();
    let workspace = tempdir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    config.workspace.root = workspace.to_string_lossy().into_owned();
    config.workspace.uploads = workspace.join("uploads").to_string_lossy().into_owned();
    config.agent.cwd = Some(config.workspace.root.clone());
    config.agent.restart = "on-crash".to_owned();
    config.agent.args.extend([
        "--write-pid".to_owned(),
        pid_path.to_string_lossy().into_owned(),
    ]);
    let harness = AgentHarness::spawn_with_config(config).await;
    let client = http().await;

    let reported_first_pid = start_agent_for_crash_test(&client, &harness.base_url).await;
    let first_pid = read_fake_agent_pid(&pid_path);
    assert_eq!(first_pid, reported_first_pid);
    let session_id = create_session_for_crash_test(&client, &harness.base_url).await;

    kill_process(first_pid);
    let status = wait_for_agent_status(&client, &harness.base_url, |data| {
        data["process_state"].as_str() == Some("running")
            && data["pid"]
                .as_u64()
                .is_some_and(|pid| pid != u64::from(first_pid))
    })
    .await;
    let restarted_pid = status["pid"].as_u64().expect("restarted pid");
    assert_ne!(restarted_pid, u64::from(first_pid));

    let response = client
        .post(format!(
            "{}/v1/sessions/{}/resume",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .json(&json!({}))
        .send()
        .await
        .expect("resume");
    let resume_status = response.status();
    let resume_body: Value = response.json().await.expect("resume json");
    assert_eq!(resume_status, StatusCode::OK, "body: {resume_body}");

    let store = harness.state.lock().await;
    let lifecycle = store.query_agent_lifecycle(50).expect("lifecycle");
    drop(store);
    let kinds: Vec<&str> = lifecycle
        .iter()
        .map(|row| row.event_kind.as_str())
        .collect();
    assert!(kinds.contains(&"agent.exited"), "kinds: {kinds:?}");
    assert!(
        kinds.contains(&"agent.restart_scheduled"),
        "kinds: {kinds:?}"
    );
    assert!(
        lifecycle
            .iter()
            .filter(|row| row.event_kind == "agent.started")
            .count()
            >= 2,
        "expected initial and restarted agent.started rows, got {kinds:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn never_policy_does_not_restart_after_agent_crash() {
    let tempdir = TempDir::new().expect("tempdir");
    let pid_path = tempdir.path().join("placebo-agent.pid");
    let mut config = test_config();
    let workspace = tempdir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    config.workspace.root = workspace.to_string_lossy().into_owned();
    config.workspace.uploads = workspace.join("uploads").to_string_lossy().into_owned();
    config.agent.cwd = Some(config.workspace.root.clone());
    config.agent.restart = "never".to_owned();
    config.agent.args.extend([
        "--write-pid".to_owned(),
        pid_path.to_string_lossy().into_owned(),
    ]);
    let harness = AgentHarness::spawn_with_config(config).await;
    let client = http().await;

    let reported_first_pid = start_agent_for_crash_test(&client, &harness.base_url).await;
    let first_pid = read_fake_agent_pid(&pid_path);
    assert_eq!(first_pid, reported_first_pid);
    let session_id = create_session_for_crash_test(&client, &harness.base_url).await;

    kill_process(first_pid);
    wait_for_agent_status(&client, &harness.base_url, |data| {
        data["process_state"].as_str() == Some("stopped") && data["pid"].is_null()
    })
    .await;

    let response = client
        .post(format!(
            "{}/v1/sessions/{}/resume",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .json(&json!({}))
        .send()
        .await
        .expect("resume");
    let resume_status = response.status();
    let resume_body: Value = response.json().await.expect("resume json");
    assert_eq!(resume_status, StatusCode::CONFLICT, "body: {resume_body}");
    assert_eq!(resume_body["error"]["code"], "agent.not_running");

    let store = harness.state.lock().await;
    let lifecycle = store.query_agent_lifecycle(50).expect("lifecycle");
    drop(store);
    let kinds: Vec<&str> = lifecycle
        .iter()
        .map(|row| row.event_kind.as_str())
        .collect();
    assert!(kinds.contains(&"agent.exited"), "kinds: {kinds:?}");
    assert!(kinds.contains(&"agent.restart_skipped"), "kinds: {kinds:?}");
    assert!(
        !kinds.contains(&"agent.restart_scheduled"),
        "never policy must not schedule restart: {kinds:?}"
    );
}

#[tokio::test]
async fn agent_restart_starts_when_not_running() {
    // POST /v1/agent/restart on a stopped supervisor degenerates into
    // a plain start. Confirms the endpoint exists, is admin-tier, and
    // returns the same capability payload as `agent/start`.
    let harness = AgentHarness::spawn().await;
    let client = http().await;
    let response = client
        .post(format!("{}/v1/agent/restart", harness.base_url))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("send restart");
    let status = response.status();
    let body_text = response.text().await.unwrap_or_default();
    assert_eq!(status, StatusCode::OK, "body: {body_text}");
    let body: Value = serde_json::from_str(&body_text).expect("restart json");
    assert_eq!(body["ok"], true);
    assert!(body["data"]["started_at"].as_str().is_some());
    assert!(body["data"]["stopped_at"].as_str().is_some());
    assert!(body["data"]["capabilities"].is_object());
    // Prior process didn't exist, so prior_exit_status is null.
    assert!(body["data"]["prior_exit_status"].is_null());
}

#[tokio::test]
async fn agent_restart_picks_up_config_written_after_daemon_start() {
    // Regression: the restart handler must re-read the config from
    // disk so a `acps agent set` that wrote new provider/model values
    // is honored on the next supervised process spawn — the in-memory
    // `state.config` cache would otherwise hand the stale config back
    // to the supervisor.
    use serde_json::Value as JsonValue;

    let harness = AgentHarness::spawn().await;
    let client = http().await;
    let initial = std::fs::read_to_string(&harness.config_path).expect("read initial config");

    // Simulate `acps agent set` mutating the config on disk AFTER
    // the daemon has cached its own copy. Point `command` at a path
    // that absolutely cannot resolve to a binary; the supervisor's
    // spawn step reads this field directly. If the handler reads
    // from disk on each restart (the intended behavior), the spawn
    // fails with `agent.spawn_failed`. If it regressed to using the
    // cached `state.config`, restart would succeed with the original
    // valid binary path and this assertion would fail.
    let mutated = initial.replace(
        &format!("command = \"{}\"", env!("CARGO_BIN_EXE_placebo-agent")),
        "command = \"/nonexistent/absolutely-not-a-binary\"",
    );
    std::fs::write(&harness.config_path, &mutated).expect("write mutated config");

    let response = client
        .post(format!("{}/v1/agent/restart", harness.base_url))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("send restart");
    let status = response.status();
    let body_text = response.text().await.unwrap_or_default();
    assert!(
        status.is_server_error() || status == StatusCode::BAD_GATEWAY,
        "restart must fail when on-disk command no longer exists; got {status} body={body_text}",
    );
    let body: JsonValue = serde_json::from_str(&body_text).expect("restart err json");
    let code = body["error"]["code"].as_str().expect("error code present");
    // Spawn failures and downstream initialize failures both prove
    // the on-disk command was honored. A regression that fell back
    // to the cached config would route through the original valid
    // binary and return 200 instead.
    assert!(
        matches!(code, "agent.spawn_failed" | "agent.initialize_failed"),
        "unexpected error code `{code}`; expected agent.spawn_failed or agent.initialize_failed",
    );
}

#[tokio::test]
async fn agent_start_picks_up_config_written_after_daemon_start() {
    use serde_json::Value as JsonValue;

    let harness = AgentHarness::spawn().await;
    let client = http().await;
    let initial = std::fs::read_to_string(&harness.config_path).expect("read initial config");

    let mutated = initial.replace(
        &format!("command = \"{}\"", env!("CARGO_BIN_EXE_placebo-agent")),
        "command = \"/nonexistent/absolutely-not-a-binary\"",
    );
    std::fs::write(&harness.config_path, &mutated).expect("write mutated config");

    let response = client
        .post(format!("{}/v1/agent/start", harness.base_url))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("send start");
    let status = response.status();
    let body_text = response.text().await.unwrap_or_default();
    assert!(
        status.is_server_error() || status == StatusCode::BAD_GATEWAY,
        "start must fail when on-disk command no longer exists; got {status} body={body_text}",
    );
    let body: JsonValue = serde_json::from_str(&body_text).expect("start err json");
    let code = body["error"]["code"].as_str().expect("error code present");
    assert!(
        matches!(code, "agent.spawn_failed" | "agent.initialize_failed"),
        "unexpected error code `{code}`; expected agent.spawn_failed or agent.initialize_failed",
    );
}

#[tokio::test]
async fn array_start_sees_target_added_after_daemon_start() {
    let harness = AgentHarness::spawn().await;
    let mut updated =
        Config::load_from_path(&harness.config_path).expect("config should load from disk");
    updated.array.enabled = true;
    let mut secondary = updated.agent.clone();
    secondary.id = "placebo-secondary".to_owned();
    secondary.name = "Placebo Secondary".to_owned();
    updated.array.targets.push(ArrayTargetConfig {
        id: "placebo-secondary".to_owned(),
        agent: secondary,
    });
    std::fs::write(
        &harness.config_path,
        updated.to_canonical_toml().expect("canonical config"),
    )
    .expect("config should be rewritten");

    let client = http().await;
    let response = client
        .post(format!(
            "{}/v1/array/targets/{}/start",
            harness.base_url, "placebo-secondary"
        ))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("send array start");
    let status = response.status();
    let body: Value = response.json().await.expect("array start json");

    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["ok"], true);
    assert!(body["data"]["capabilities"].is_object());
}

#[tokio::test]
async fn array_start_rejects_non_default_target_when_array_is_off() {
    let mut config = test_config();
    add_codex_placebo_target(&mut config);
    let harness = AgentHarness::spawn_with_config(config).await;
    let client = http().await;

    let response = client
        .post(format!(
            "{}/v1/array/targets/{}/start",
            harness.base_url, "codex"
        ))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("send array start");
    let status = response.status();
    let body: Value = response.json().await.expect("array start json");

    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    assert_eq!(body["error"]["code"], "request.invalid_param");
    assert!(
        body["error"]["message"]
            .as_str()
            .expect("message should be string")
            .contains("Array mode is off")
    );
}

#[tokio::test]
async fn array_status_reports_daemon_targets() {
    let mut config = test_config();
    config.array.enabled = true;
    add_codex_placebo_target(&mut config);
    let harness = AgentHarness::spawn_with_config(config).await;
    let client = http().await;

    let response = client
        .get(format!("{}/v1/array/status", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("send array status");
    let status = response.status();
    let body: Value = response.json().await.expect("array status json");

    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["enabled"], true);
    assert_eq!(body["data"]["primary_target"], "opencode");
    let targets = body["data"]["targets"]
        .as_array()
        .expect("targets should be an array");
    assert!(
        targets
            .iter()
            .any(|target| { target["id"] == "opencode" && target["process_state"] == "stopped" })
    );
    assert!(
        targets
            .iter()
            .any(|target| { target["id"] == "codex" && target["process_state"] == "stopped" })
    );
}

#[tokio::test]
async fn array_status_rejects_admin_key() {
    // Strict tiering: the read-only array status route is session-tier and must
    // reject a valid admin key with auth.wrong_kind (no admin superset).
    let harness = AgentHarness::spawn().await;
    let client = http().await;
    let response = client
        .get(format!("{}/v1/array/status", harness.base_url))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("send array status");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "auth.wrong_kind");
}

#[tokio::test]
async fn array_capabilities_rejects_admin_key() {
    // Session-tier per-target capabilities route also rejects admin keys.
    let harness = AgentHarness::spawn().await;
    let client = http().await;
    let response = client
        .get(format!(
            "{}/v1/array/targets/{}/capabilities",
            harness.base_url, "opencode"
        ))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("send array capabilities");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "auth.wrong_kind");
}

#[tokio::test]
async fn array_target_mutations_reject_session_key() {
    // The four state-altering per-target routes are admin-tier; a session key
    // must never gain the power to install/start/stop/restart an agent process.
    // The require_admin layer rejects before the handler routes on target_id,
    // so this guards against an accidental downgrade into the session router.
    let harness = AgentHarness::spawn().await;
    let client = http().await;
    for action in ["install", "start", "stop", "restart"] {
        let response = client
            .post(format!(
                "{}/v1/array/targets/{}/{}",
                harness.base_url, "opencode", action
            ))
            .header("Authorization", session_bearer())
            .send()
            .await
            .expect("send array mutation");
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "array {action} must reject a session key",
        );
        let body: Value = response.json().await.expect("json");
        assert_eq!(
            body["error"]["code"], "auth.wrong_kind",
            "array {action} wrong-tier code",
        );
    }
}

#[tokio::test]
async fn array_target_stop_and_restart_lifecycle() {
    // Exercise the previously-untested stop/restart routes for a secondary
    // target: start -> running, stop -> stopped, restart -> running.
    let mut config = test_config();
    config.array.enabled = true;
    add_codex_placebo_target(&mut config);
    let harness = AgentHarness::spawn_with_config(config).await;
    let client = http().await;

    let start = client
        .post(format!(
            "{}/v1/array/targets/{}/start",
            harness.base_url, "codex"
        ))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("send array start");
    assert_eq!(start.status(), StatusCode::OK);
    let start_body: Value = start.json().await.expect("start json");
    assert!(start_body["data"]["capabilities"].is_object());

    let stop = client
        .post(format!(
            "{}/v1/array/targets/{}/stop",
            harness.base_url, "codex"
        ))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("send array stop");
    assert_eq!(stop.status(), StatusCode::OK);

    let status: Value = client
        .get(format!("{}/v1/array/status", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("send array status")
        .json()
        .await
        .expect("status json");
    let codex_state = status["data"]["targets"]
        .as_array()
        .expect("targets array")
        .iter()
        .find(|target| target["id"] == "codex")
        .map(|target| target["process_state"].clone())
        .expect("codex target present");
    assert_eq!(codex_state, "stopped");

    let restart = client
        .post(format!(
            "{}/v1/array/targets/{}/restart",
            harness.base_url, "codex"
        ))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("send array restart");
    assert_eq!(restart.status(), StatusCode::OK);
    let restart_body: Value = restart.json().await.expect("restart json");
    assert!(restart_body["data"]["capabilities"].is_object());
}

#[tokio::test]
async fn agent_aliases_follow_default_target_changed_on_disk() {
    let mut config = test_config();
    config.array.enabled = true;
    add_codex_placebo_target(&mut config);
    let harness = AgentHarness::spawn_with_config(config).await;

    let mut updated =
        Config::load_from_path(&harness.config_path).expect("config should load from disk");
    let codex_agent = updated
        .array
        .target("codex")
        .expect("codex target exists")
        .agent
        .clone();
    updated.array.primary_target = "codex".to_owned();
    updated.agent = codex_agent;
    std::fs::write(
        &harness.config_path,
        updated.to_canonical_toml().expect("canonical config"),
    )
    .expect("config should be rewritten");

    let client = http().await;
    let start_response = client
        .post(format!("{}/v1/agent/start", harness.base_url))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("send agent start");
    let start_status = start_response.status();
    let start_body: Value = start_response.json().await.expect("start json");
    assert_eq!(start_status, StatusCode::OK, "body: {start_body}");

    let status_body: Value = client
        .get(format!("{}/v1/agent/status", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("send agent status")
        .json()
        .await
        .expect("status json");
    assert_eq!(status_body["data"]["agent"]["id"], "codex");
    assert_eq!(status_body["data"]["process_state"], "running");
}

#[tokio::test]
async fn health_ready_follows_default_target_changed_on_disk() {
    let mut config = test_config();
    config.array.enabled = true;
    add_codex_placebo_target(&mut config);
    let harness = AgentHarness::spawn_with_config(config).await;

    let mut updated =
        Config::load_from_path(&harness.config_path).expect("config should load from disk");
    let codex_agent = updated
        .array
        .target("codex")
        .expect("codex target exists")
        .agent
        .clone();
    updated.array.primary_target = "codex".to_owned();
    updated.agent = codex_agent;
    std::fs::write(
        &harness.config_path,
        updated.to_canonical_toml().expect("canonical config"),
    )
    .expect("config should be rewritten");

    let body: Value = http()
        .await
        .get(format!("{}/v1/health/ready", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("send health")
        .json()
        .await
        .expect("health json");
    assert_eq!(body["data"]["agent"]["id"], "codex");
}

#[tokio::test]
async fn agent_restart_requires_admin_key() {
    let harness = AgentHarness::spawn().await;
    let client = http().await;
    let response = client
        .post(format!("{}/v1/agent/restart", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "auth.wrong_kind");
}
