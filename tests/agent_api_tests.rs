#![cfg(feature = "test-fixtures")]

//! End-to-end coverage for the agent HTTP routes: install, start,
//! capabilities, stop, and the session/admin tier enforcement on those.
//!
//! All tests drive a real `acps` HTTP server against a `Config` whose
//! `[agent].command` is the standalone placebo ACP fixture.

use std::{sync::Arc, time::Duration};

use acp_stack::api::{self, AppState, RuntimePaths};
use acp_stack::config::{
    AgentAdapterConfig, ArrayTargetConfig, Config, HttpHeaderRef, McpConfig, McpHttpServer,
    McpServerConfig, McpStdioServer, load_config_from_str,
};
use acp_stack::runtime::agent::model_discovery::fetch_session_config_with_timeout;
use acp_stack::state::StateStore;
use futures::{SinkExt, StreamExt};
use reqwest::StatusCode;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::Mutex as TokioMutex;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const SESSION_KEY: &str = "acps_session_cccccccccccccccccccccccccccccccccccccccccccc";
const ADMIN_KEY: &str = "acps_admin_dddddddddddddddddddddddddddddddddddddddddddd";

struct AgentHarness {
    base_url: String,
    config_path: std::path::PathBuf,
    _tempdir: TempDir,
    state: Arc<TokioMutex<StateStore>>,
    join: JoinHandle<acp_stack::error::Result<()>>,
}

impl AgentHarness {
    async fn spawn() -> Self {
        Self::spawn_with_config(test_config()).await
    }

    async fn spawn_with_config(mut config: Config) -> Self {
        let tempdir = TempDir::new().expect("tempdir");
        if config.workspace.root == "/workspace" {
            let workspace = tempdir.path().join("workspace");
            config.workspace.root = workspace.to_string_lossy().into_owned();
            config.workspace.uploads = workspace.join("uploads").to_string_lossy().into_owned();
        }
        let path = tempdir.path().join("state.sqlite");
        let config_path = tempdir.path().join("acps-config.toml");
        std::fs::write(
            &config_path,
            config.to_canonical_toml().expect("canonical test config"),
        )
        .expect("test config write");
        let store = StateStore::open(&path).expect("state open");
        store.migrate().expect("migrate");
        let effective_bind = config.api.bind.clone();
        let runtime_paths = RuntimePaths::new(config_path.clone(), path);
        let app_state = AppState::with_effective_bind_and_runtime_paths(
            config,
            store,
            SESSION_KEY.to_owned(),
            ADMIN_KEY.to_owned(),
            effective_bind,
            runtime_paths,
        );
        let state = app_state.state.clone();
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let base_url = format!("http://{}", listener.local_addr().expect("local"));
        let join = tokio::spawn(async move { api::serve(app_state, listener).await });
        Self {
            base_url,
            config_path,
            _tempdir: tempdir,
            state,
            join,
        }
    }
}

/// Serializes HOME mutations across the parallel-by-default
/// `#[tokio::test]` functions in this file. Without this lock, two
/// tests that both `HomeEnvGuard::set(...)` concurrently would step
/// on each other's HOME and observe random subsets of the other's
/// tempdir state. The lock is held for the lifetime of each
/// `HomeEnvGuard`, which spans the full test body.
static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
static DISCOVERY_FIXTURE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct HomeEnvGuard<'a> {
    _lock: std::sync::MutexGuard<'a, ()>,
    previous: Option<std::ffi::OsString>,
}

impl HomeEnvGuard<'_> {
    fn set(home: &std::path::Path) -> Self {
        let lock = HOME_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = std::env::var_os("HOME");
        // SAFETY: HOME_LOCK serializes tests that mutate HOME via
        // this guard. Tests in this binary that depend on HOME route
        // through here, so there's no read racing the mutation.
        unsafe {
            std::env::set_var("HOME", home);
        }
        Self {
            _lock: lock,
            previous,
        }
    }
}

impl Drop for HomeEnvGuard<'_> {
    fn drop(&mut self) {
        // SAFETY: lock still held; restore the prior HOME (or remove
        // if unset coming in) before releasing it so the next test
        // sees a clean slate.
        unsafe {
            match self.previous.take() {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
        }
    }
}

struct EnvVarGuard<'a> {
    _lock: std::sync::MutexGuard<'a, ()>,
    key: &'static str,
    previous: Option<std::ffi::OsString>,
}

impl EnvVarGuard<'_> {
    fn set(key: &'static str, value: &std::path::Path) -> Self {
        let lock = DISCOVERY_FIXTURE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = std::env::var_os(key);
        // SAFETY: DISCOVERY_FIXTURE_LOCK serializes tests in this
        // binary that mutate or depend on this process-wide fixture
        // env var.
        unsafe {
            std::env::set_var(key, value);
        }
        Self {
            _lock: lock,
            key,
            previous,
        }
    }

    fn unset(key: &'static str) -> Self {
        let lock = DISCOVERY_FIXTURE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = std::env::var_os(key);
        // SAFETY: lock held; no concurrent test in this binary can
        // observe a partial fixture-env mutation through this guard.
        unsafe {
            std::env::remove_var(key);
        }
        Self {
            _lock: lock,
            key,
            previous,
        }
    }
}

impl Drop for EnvVarGuard<'_> {
    fn drop(&mut self) {
        // SAFETY: lock still held; restore the prior fixture setting
        // before releasing it.
        unsafe {
            match self.previous.take() {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

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

impl Drop for AgentHarness {
    fn drop(&mut self) {
        self.join.abort();
    }
}

/// Build a test config that points `[agent].command` at the placebo ACP
/// fixture. Empty `[agent].env` so the handlers don't try to open a secret
/// store that doesn't exist in the test tempdir.
fn test_config() -> Config {
    let toml_text = include_str!("fixtures/valid-opencode-stack.toml");
    let mut config = load_config_from_str(toml_text).expect("config parses");
    config.agent.command = env!("CARGO_BIN_EXE_placebo-agent").to_owned();
    config.agent.args = vec!["acp".into()];
    config.agent.env = vec![];
    config.agent.cwd = Some(std::env::temp_dir().to_string_lossy().into_owned());
    config.agent.expected_sha256 = None;
    config.agent.adapter = Some(AgentAdapterConfig {
        id: "codex-acp".to_owned(),
        name: "Codex ACP Adapter".to_owned(),
        upstream_agent: "codex-cli".to_owned(),
        source_url: Some("https://github.com/zed-industries/codex-acp".to_owned()),
    });
    // Replace the install recipe with something that completes in milliseconds.
    config.agent.install = Some(acp_stack::config::AgentInstallConfig {
        install_type: "shell".into(),
        creates: "true".into(),
        shell: Some("true".into()),
    });
    config
}

fn add_codex_placebo_target(config: &mut Config) {
    let mut secondary = config.agent.clone();
    secondary.id = "codex".to_owned();
    secondary.name = "Codex".to_owned();
    secondary.command = env!("CARGO_BIN_EXE_placebo-agent").to_owned();
    secondary.args = vec!["acp".into()];
    secondary.env = vec![];
    secondary.cwd = Some(std::env::temp_dir().to_string_lossy().into_owned());
    secondary.expected_sha256 = None;
    secondary.install = Some(acp_stack::config::AgentInstallConfig {
        install_type: "shell".into(),
        creates: "true".into(),
        shell: Some("true".into()),
    });
    config.array.targets.push(ArrayTargetConfig {
        id: "codex".to_owned(),
        agent: secondary,
    });
}

fn add_kimi_placebo_target(config: &mut Config) {
    let mut secondary = config.agent.clone();
    secondary.id = "kimi".to_owned();
    secondary.name = "Kimi Code".to_owned();
    secondary.command = env!("CARGO_BIN_EXE_placebo-agent").to_owned();
    secondary.args = vec!["acp".into()];
    secondary.env = vec!["KIMI_API_KEY".to_owned()];
    secondary.cwd = Some(std::env::temp_dir().to_string_lossy().into_owned());
    secondary.expected_sha256 = None;
    secondary.install = Some(acp_stack::config::AgentInstallConfig {
        install_type: "shell".into(),
        creates: "true".into(),
        shell: Some("true".into()),
    });
    config.array.targets.push(ArrayTargetConfig {
        id: "kimi".to_owned(),
        agent: secondary,
    });
}

async fn http() -> reqwest::Client {
    reqwest::Client::builder().build().expect("reqwest client")
}

fn admin_bearer() -> String {
    format!("Bearer {ADMIN_KEY}")
}

fn session_bearer() -> String {
    format!("Bearer {SESSION_KEY}")
}

fn websocket_url(harness: &AgentHarness) -> String {
    format!(
        "{}/v1/ws",
        harness
            .base_url
            .strip_prefix("http://")
            .map(|rest| format!("ws://{rest}"))
            .unwrap_or_else(|| harness.base_url.replace("http", "ws"))
    )
}

fn websocket_request(harness: &AgentHarness) -> http::Request<()> {
    let mut request = websocket_url(harness)
        .into_client_request()
        .expect("websocket request");
    request.headers_mut().insert(
        "Authorization",
        http::HeaderValue::from_str(&session_bearer()).expect("bearer header"),
    );
    request
}

fn shell_quote_path(path: &std::path::Path) -> String {
    let text = path.to_string_lossy();
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn write_installed_skill(root: &std::path::Path, name: &str, descriptor: &str) {
    let skill_dir = root.join(name);
    std::fs::create_dir_all(&skill_dir).expect("skill dir");
    std::fs::write(skill_dir.join("SKILL.md"), descriptor).expect("descriptor");
    std::fs::write(skill_dir.join("script.sh"), "true\n").expect("script");
}

fn write_cursor_registry_override(config_dir: &std::path::Path) {
    let body = r#"
[[agents]]
id = "cursor"
name = "Cursor CLI"
kind = "native"
headless_compatible = true
set_model = true
set_mode = true
supports_agent_skills = true
agent_skills_install_dir = "~/.agents/skills"
support_doc = "docs/agents/cursor.md"

[agents.harness]
id = "true"

[agents.harness.install.shell]
script = "true"
creates = "true"
"#;
    std::fs::write(config_dir.join("agents.toml"), body).expect("registry override");
}

fn write_amp_registry_override(config_dir: &std::path::Path) {
    let body = r#"
[[agents]]
id = "amp"
name = "Amp Code"
kind = "adapter"
headless_compatible = true
set_provider = false
set_model = false
set_mode = true
supports_agent_skills = true
agent_skills_install_dir = "~/.config/agents/skills"
support_doc = "docs/agents/amp.md"

[agents.adapter]
id = "true"

[agents.adapter.install.shell]
script = "true"
creates = "true"

[agents.harness]
id = "true"

[agents.harness.install.shell]
script = "true"
creates = "true"
"#;
    std::fs::write(config_dir.join("agents.toml"), body).expect("registry override");
}

fn write_pi_registry_override(config_dir: &std::path::Path) {
    let body = r#"
[[agents]]
id = "pi"
name = "Pi Agent"
kind = "adapter"
headless_compatible = true
set_provider = true
set_model = true
supports_agent_skills = true
agent_skills_install_dir = "~/.agents/skills"
support_doc = "docs/agents/pi.md"

[agents.adapter]
id = "true"

[agents.adapter.install.shell]
script = "true"
creates = "true"

[agents.harness]
id = "true"

[agents.harness.install.shell]
script = "true"
creates = "true"
"#;
    std::fs::write(config_dir.join("agents.toml"), body).expect("registry override");
}

fn write_config_options_fixture(root: &std::path::Path, models: &[&str]) -> std::path::PathBuf {
    let fixture_path = root.join("switch-config-options.json");
    let body = serde_json::json!([
        {
            "id": "model",
            "name": "Model",
            "category": "model",
            "type": "select",
            "currentValue": models[0],
            "options": models
                .iter()
                .map(|value| serde_json::json!({ "value": value, "name": value }))
                .collect::<Vec<_>>()
        }
    ]);
    std::fs::write(&fixture_path, body.to_string()).expect("fixture write");
    fixture_path
}

fn switch_mcp_config() -> McpConfig {
    McpConfig {
        servers: vec![
            McpServerConfig::Stdio(McpStdioServer {
                name: "local-tools".to_owned(),
                command: "/usr/local/bin/local-tools-mcp".to_owned(),
                args: vec!["--stdio".to_owned()],
                env: vec!["LOCAL_TOOLS_TOKEN".to_owned()],
            }),
            McpServerConfig::Http(McpHttpServer {
                name: "linear".to_owned(),
                url: "https://mcp.linear.app/mcp".to_owned(),
                headers: vec![HttpHeaderRef {
                    name: "Authorization".to_owned(),
                    value_ref: "LINEAR_API_KEY".to_owned(),
                }],
            }),
        ],
    }
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
        "https://github.com/zed-industries/codex-acp"
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
        "https://github.com/zed-industries/codex-acp"
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
    // Mirrors `tests/cli_tests.rs::write_acp_config_options` shape so
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

#[tokio::test]
async fn agent_switch_requires_admin_key() {
    let harness = AgentHarness::spawn().await;
    let client = http().await;
    let response = client
        .post(format!("{}/v1/agent/switch", harness.base_url))
        .header("Authorization", session_bearer())
        .json(&serde_json::json!({ "agent": "cursor" }))
        .send()
        .await
        .expect("send switch");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "auth.wrong_kind");
}

#[tokio::test]
async fn agent_switch_installs_target_and_returns_model_choices() {
    let tempdir = TempDir::new().expect("tempdir");
    let _home = HomeEnvGuard::set(tempdir.path());
    let config_dir = tempdir.path().join(".config/acp-stack");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    write_cursor_registry_override(&config_dir);
    let mut config = test_config();
    let workspace = tempdir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    config.workspace.root = workspace.to_string_lossy().into_owned();
    config.workspace.uploads = workspace.join("uploads").to_string_lossy().into_owned();
    config.agent.cwd = Some(config.workspace.root.clone());
    let mut secrets =
        acp_stack::secrets::SecretStore::open_or_create(tempdir.path()).expect("secret store");
    secrets
        .set_many([("CURSOR_API_KEY", "cursor-secret")])
        .expect("cursor secret");
    let fixture_path = write_config_options_fixture(tempdir.path(), &["cursor/gpt-5.5"]);
    let _fixture_guard = EnvVarGuard::set("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &fixture_path);

    let harness = AgentHarness::spawn_with_config(config).await;
    let client = http().await;
    let response = client
        .post(format!("{}/v1/agent/switch", harness.base_url))
        .header("Authorization", admin_bearer())
        .json(&serde_json::json!({ "agent": "cursor" }))
        .send()
        .await
        .expect("send switch");
    let status = response.status();
    let body_text = response.text().await.unwrap_or_default();
    assert_eq!(status, StatusCode::OK, "body: {body_text}");
    let body: Value = serde_json::from_str(&body_text).expect("switch json");
    assert_eq!(body["data"]["old_agent_id"], "opencode");
    assert_eq!(body["data"]["agent_id"], "cursor");
    assert_eq!(body["data"]["provider_status"], "not_applicable");
    assert_eq!(body["data"]["set_model"], true);
    assert_eq!(
        body["data"]["follow_up"],
        "acps agent set --model <model-id>"
    );
    assert!(matches!(
        body["data"]["install"]["outcome"].as_str(),
        Some("installed" | "already_present")
    ));
    assert_eq!(body["data"]["models"][0], "cursor/gpt-5.5");

    let written = std::fs::read_to_string(&harness.config_path).expect("read config");
    assert!(written.contains(r#"id = "cursor""#));
    assert!(written.contains(r#"env = ["CURSOR_API_KEY"]"#));
    assert!(!written.contains("[agent.provider]"));
    assert!(!written.contains("model ="));
}

#[tokio::test]
async fn agent_switch_preserves_mcp_runtime_config() {
    let tempdir = TempDir::new().expect("tempdir");
    let _home = HomeEnvGuard::set(tempdir.path());
    let config_dir = tempdir.path().join(".config/acp-stack");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    write_cursor_registry_override(&config_dir);
    let mut config = test_config();
    let workspace = tempdir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    config.workspace.root = workspace.to_string_lossy().into_owned();
    config.workspace.uploads = workspace.join("uploads").to_string_lossy().into_owned();
    config.agent.cwd = Some(config.workspace.root.clone());
    let expected_mcp = switch_mcp_config();
    config.mcp = expected_mcp.clone();
    let mut secrets =
        acp_stack::secrets::SecretStore::open_or_create(tempdir.path()).expect("secret store");
    secrets
        .set_many([("CURSOR_API_KEY", "cursor-secret")])
        .expect("cursor secret");
    let fixture_path = write_config_options_fixture(tempdir.path(), &["cursor/gpt-5.5"]);
    let _fixture_guard = EnvVarGuard::set("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &fixture_path);

    let harness = AgentHarness::spawn_with_config(config).await;
    let client = http().await;
    let response = client
        .post(format!("{}/v1/agent/switch", harness.base_url))
        .header("Authorization", admin_bearer())
        .json(&serde_json::json!({ "agent": "cursor" }))
        .send()
        .await
        .expect("send switch");
    let status = response.status();
    let body_text = response.text().await.unwrap_or_default();
    assert_eq!(status, StatusCode::OK, "body: {body_text}");

    let written = std::fs::read_to_string(&harness.config_path).expect("read config");
    let written_config = load_config_from_str(&written).expect("written config parses");
    assert_eq!(written_config.agent.id, "cursor");
    assert_eq!(written_config.mcp, expected_mcp);
}

#[tokio::test]
async fn agent_switch_preserves_adapter_metadata_and_skips_model_follow_up() {
    let tempdir = TempDir::new().expect("tempdir");
    let _home = HomeEnvGuard::set(tempdir.path());
    let config_dir = tempdir.path().join(".config/acp-stack");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    write_amp_registry_override(&config_dir);
    let mut config = test_config();
    let workspace = tempdir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    config.workspace.root = workspace.to_string_lossy().into_owned();
    config.workspace.uploads = workspace.join("uploads").to_string_lossy().into_owned();
    config.agent.cwd = Some(config.workspace.root.clone());
    let mut secrets =
        acp_stack::secrets::SecretStore::open_or_create(tempdir.path()).expect("secret store");
    secrets
        .set_many([("AMP_API_KEY", "amp-secret")])
        .expect("amp secret");

    let harness = AgentHarness::spawn_with_config(config).await;
    let client = http().await;
    let response = client
        .post(format!("{}/v1/agent/switch", harness.base_url))
        .header("Authorization", admin_bearer())
        .json(&serde_json::json!({ "agent": "amp" }))
        .send()
        .await
        .expect("send switch");
    let status = response.status();
    let body_text = response.text().await.unwrap_or_default();
    assert_eq!(status, StatusCode::OK, "body: {body_text}");
    let body: Value = serde_json::from_str(&body_text).expect("switch json");
    assert_eq!(body["data"]["agent_id"], "amp");
    assert_eq!(body["data"]["set_model"], false);
    assert!(body["data"].get("follow_up").is_none());
    assert!(
        body["data"]["models"]
            .as_array()
            .expect("models array")
            .is_empty()
    );

    let response = client
        .get(format!("{}/v1/agent/status", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("send status");
    let status_body: Value = response.json().await.expect("status json");
    assert_eq!(status_body["data"]["agent"]["id"], "amp");
    assert_eq!(status_body["data"]["agent"]["adapter"]["id"], "true");
    assert_eq!(
        status_body["data"]["agent"]["adapter"]["upstream_agent"],
        "true"
    );
}

#[tokio::test]
async fn agent_switch_ports_skills_to_target_install_dir() {
    let tempdir = TempDir::new().expect("tempdir");
    let _home = HomeEnvGuard::set(tempdir.path());
    let config_dir = tempdir.path().join(".config/acp-stack");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    write_amp_registry_override(&config_dir);
    write_installed_skill(
        &tempdir.path().join(".agents/skills"),
        "repo-map",
        "# Source Repo Map\n",
    );
    let mut secrets =
        acp_stack::secrets::SecretStore::open_or_create(tempdir.path()).expect("secret store");
    secrets
        .set_many([("AMP_API_KEY", "amp-secret")])
        .expect("amp secret");

    let mut config = test_config();
    let workspace = tempdir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    config.workspace.root = workspace.to_string_lossy().into_owned();
    config.workspace.uploads = workspace.join("uploads").to_string_lossy().into_owned();
    config.agent.cwd = Some(config.workspace.root.clone());
    let harness = AgentHarness::spawn_with_config(config).await;
    let response = http()
        .await
        .post(format!("{}/v1/agent/switch", harness.base_url))
        .header("Authorization", admin_bearer())
        .json(&serde_json::json!({ "agent": "amp" }))
        .send()
        .await
        .expect("send switch");
    let status = response.status();
    let body_text = response.text().await.unwrap_or_default();
    assert_eq!(status, StatusCode::OK, "body: {body_text}");
    let body: Value = serde_json::from_str(&body_text).expect("switch json");

    assert_eq!(body["data"]["agent_id"], "amp");
    assert_eq!(body["data"]["skills_port"]["status"], "copied");
    assert_eq!(body["data"]["skills_port"]["copied"][0]["name"], "repo-map");
    assert!(
        tempdir
            .path()
            .join(".config/agents/skills/repo-map/SKILL.md")
            .is_file()
    );
}

#[tokio::test]
async fn agent_switch_reports_shared_skills_dir_without_copying() {
    let tempdir = TempDir::new().expect("tempdir");
    let _home = HomeEnvGuard::set(tempdir.path());
    let config_dir = tempdir.path().join(".config/acp-stack");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    write_cursor_registry_override(&config_dir);
    write_installed_skill(
        &tempdir.path().join(".agents/skills"),
        "repo-map",
        "# Source Repo Map\n",
    );
    let mut secrets =
        acp_stack::secrets::SecretStore::open_or_create(tempdir.path()).expect("secret store");
    secrets
        .set_many([("CURSOR_API_KEY", "cursor-secret")])
        .expect("cursor secret");
    let fixture_path = write_config_options_fixture(tempdir.path(), &["cursor/gpt-5.5"]);
    let _fixture_guard = EnvVarGuard::set("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &fixture_path);

    let mut config = test_config();
    let workspace = tempdir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    config.workspace.root = workspace.to_string_lossy().into_owned();
    config.workspace.uploads = workspace.join("uploads").to_string_lossy().into_owned();
    config.agent.cwd = Some(config.workspace.root.clone());
    let harness = AgentHarness::spawn_with_config(config).await;
    let response = http()
        .await
        .post(format!("{}/v1/agent/switch", harness.base_url))
        .header("Authorization", admin_bearer())
        .json(&serde_json::json!({ "agent": "cursor" }))
        .send()
        .await
        .expect("send switch");
    let status = response.status();
    let body_text = response.text().await.unwrap_or_default();
    assert_eq!(status, StatusCode::OK, "body: {body_text}");
    let body: Value = serde_json::from_str(&body_text).expect("switch json");

    assert_eq!(body["data"]["agent_id"], "cursor");
    assert_eq!(body["data"]["skills_port"]["status"], "shared");
    assert!(
        tempdir
            .path()
            .join(".agents/skills/repo-map/SKILL.md")
            .is_file()
    );
}

#[tokio::test]
async fn agent_switch_skill_port_failure_aborts_config_write() {
    let tempdir = TempDir::new().expect("tempdir");
    let _home = HomeEnvGuard::set(tempdir.path());
    let config_dir = tempdir.path().join(".config/acp-stack");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    write_amp_registry_override(&config_dir);
    write_installed_skill(
        &tempdir.path().join(".agents/skills"),
        "repo-map",
        "# Source Repo Map\n",
    );
    std::fs::create_dir_all(tempdir.path().join(".config/agents/skills")).expect("target root");
    std::fs::write(
        tempdir.path().join(".config/agents/skills/repo-map"),
        "not a directory\n",
    )
    .expect("conflict");
    let mut secrets =
        acp_stack::secrets::SecretStore::open_or_create(tempdir.path()).expect("secret store");
    secrets
        .set_many([("AMP_API_KEY", "amp-secret")])
        .expect("amp secret");

    let mut config = test_config();
    let workspace = tempdir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    config.workspace.root = workspace.to_string_lossy().into_owned();
    config.workspace.uploads = workspace.join("uploads").to_string_lossy().into_owned();
    config.agent.cwd = Some(config.workspace.root.clone());
    let harness = AgentHarness::spawn_with_config(config).await;
    let response = http()
        .await
        .post(format!("{}/v1/agent/switch", harness.base_url))
        .header("Authorization", admin_bearer())
        .json(&serde_json::json!({ "agent": "amp" }))
        .send()
        .await
        .expect("send switch");
    let status = response.status();
    let body_text = response.text().await.unwrap_or_default();

    assert_eq!(status, StatusCode::CONFLICT, "body: {body_text}");
    let written = std::fs::read_to_string(&harness.config_path).expect("read config");
    assert!(written.contains(r#"id = "opencode""#));
}

#[tokio::test]
async fn agent_switch_copies_provider_secret_to_target_default_ref() {
    let tempdir = TempDir::new().expect("tempdir");
    let _home = HomeEnvGuard::set(tempdir.path());
    let config_dir = tempdir.path().join(".config/acp-stack");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    write_pi_registry_override(&config_dir);
    let mut config = test_config();
    let workspace = tempdir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    config.workspace.root = workspace.to_string_lossy().into_owned();
    config.workspace.uploads = workspace.join("uploads").to_string_lossy().into_owned();
    config.agent.cwd = Some(config.workspace.root.clone());
    config.agent.env = vec![
        "CLOUDFLARE_API_TOKEN".to_owned(),
        "CLOUDFLARE_ACCOUNT_ID".to_owned(),
        "CLOUDFLARE_GATEWAY_ID".to_owned(),
    ];
    config.agent.provider = Some(acp_stack::config::AgentProviderConfig {
        id: "cloudflare-ai-gateway".to_owned(),
        model: Some("cloudflare-ai-gateway/workers-ai/@cf/test".to_owned()),
        api_key_ref: Some("CLOUDFLARE_API_TOKEN".to_owned()),
        custom: None,
    });
    let mut secrets =
        acp_stack::secrets::SecretStore::open_or_create(tempdir.path()).expect("secret store");
    secrets
        .set_many([
            ("CLOUDFLARE_API_TOKEN", "cloudflare-secret"),
            ("CLOUDFLARE_ACCOUNT_ID", "account-id"),
            ("CLOUDFLARE_GATEWAY_ID", "gateway-id"),
        ])
        .expect("cloudflare secrets");
    let fixture_path = write_config_options_fixture(
        tempdir.path(),
        &["cloudflare-ai-gateway/workers-ai/@cf/test"],
    );
    let _fixture_guard = EnvVarGuard::set("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &fixture_path);

    let harness = AgentHarness::spawn_with_config(config).await;
    let client = http().await;
    let response = client
        .post(format!("{}/v1/agent/switch", harness.base_url))
        .header("Authorization", admin_bearer())
        .json(&serde_json::json!({ "agent": "pi" }))
        .send()
        .await
        .expect("send switch");
    let status = response.status();
    let body_text = response.text().await.unwrap_or_default();
    assert_eq!(status, StatusCode::OK, "body: {body_text}");
    let body: Value = serde_json::from_str(&body_text).expect("switch json");
    assert_eq!(body["data"]["agent_id"], "pi");
    assert_eq!(body["data"]["api_key_ref"], "CLOUDFLARE_API_KEY");
    assert_eq!(
        body["data"]["secret_migrations"][0]["from_ref"],
        "CLOUDFLARE_API_TOKEN"
    );
    assert_eq!(
        body["data"]["secret_migrations"][0]["to_ref"],
        "CLOUDFLARE_API_KEY"
    );

    let secrets = acp_stack::secrets::SecretStore::open(tempdir.path()).expect("secret store");
    assert_eq!(
        secrets.get("CLOUDFLARE_API_KEY").expect("copied secret"),
        "cloudflare-secret"
    );
    let written = std::fs::read_to_string(&harness.config_path).expect("read config");
    assert!(written.contains(r#"api_key_ref = "CLOUDFLARE_API_KEY""#));
    assert!(written.contains(r#""CLOUDFLARE_API_KEY""#));
}

#[tokio::test]
async fn agent_switch_drop_cleans_source_config_and_preserves_secrets() {
    let tempdir = TempDir::new().expect("tempdir");
    let _home = HomeEnvGuard::set(tempdir.path());
    let config_dir = tempdir.path().join(".config/acp-stack");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    write_cursor_registry_override(&config_dir);
    let mut config = test_config();
    let workspace = tempdir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    config.workspace.root = workspace.to_string_lossy().into_owned();
    config.workspace.uploads = workspace.join("uploads").to_string_lossy().into_owned();
    config.agent.cwd = Some(config.workspace.root.clone());
    config.agent.env = vec!["OPENAI_API_KEY".to_owned()];
    config.agent.provider = Some(acp_stack::config::AgentProviderConfig {
        id: "openai".to_owned(),
        model: Some("openai/gpt-5.5".to_owned()),
        api_key_ref: Some("OPENAI_API_KEY".to_owned()),
        custom: None,
    });
    let opencode_path = tempdir
        .path()
        .join(".config")
        .join("opencode")
        .join("opencode.json");
    std::fs::create_dir_all(opencode_path.parent().expect("path has parent"))
        .expect("opencode dir");
    std::fs::write(
        &opencode_path,
        r#"{"$schema":"https://opencode.ai/config.json","model":"openai/gpt-5.5","small_model":"openai/gpt-5.5","enabled_providers":["openai"],"provider":{"openai":{"options":{"apiKey":"{env:OPENAI_API_KEY}"}}},"theme":"keep"}"#,
    )
    .expect("opencode config");
    let mut secrets =
        acp_stack::secrets::SecretStore::open_or_create(tempdir.path()).expect("secret store");
    secrets
        .set_many([
            ("OPENAI_API_KEY", "openai-secret"),
            ("CURSOR_API_KEY", "cursor-secret"),
        ])
        .expect("secrets");
    let fixture_path = write_config_options_fixture(tempdir.path(), &["cursor/gpt-5.5"]);
    let _fixture_guard = EnvVarGuard::set("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &fixture_path);

    let harness = AgentHarness::spawn_with_config(config).await;
    let client = http().await;
    let response = client
        .post(format!("{}/v1/agent/switch", harness.base_url))
        .header("Authorization", admin_bearer())
        .json(&serde_json::json!({ "agent": "cursor", "drop": true }))
        .send()
        .await
        .expect("send switch");
    let status = response.status();
    let body_text = response.text().await.unwrap_or_default();
    assert_eq!(status, StatusCode::OK, "body: {body_text}");
    let body: Value = serde_json::from_str(&body_text).expect("switch json");
    assert_eq!(body["data"]["agent_id"], "cursor");
    assert_eq!(
        body["data"]["cleaned_configs"][0]["path"],
        opencode_path.to_string_lossy().as_ref()
    );

    let value: Value = serde_json::from_str(
        &std::fs::read_to_string(&opencode_path).expect("opencode config remains"),
    )
    .expect("opencode json");
    assert_eq!(value["theme"], "keep");
    assert!(value.get("model").is_none());
    assert!(value.get("provider").is_none());

    let secrets = acp_stack::secrets::SecretStore::open(tempdir.path()).expect("secret store");
    assert_eq!(
        secrets.get("OPENAI_API_KEY").expect("source secret"),
        "openai-secret"
    );
    assert_eq!(
        secrets.get("CURSOR_API_KEY").expect("target secret"),
        "cursor-secret"
    );
}

#[tokio::test]
async fn agent_switch_drop_reports_cleanup_failure_without_failing_switch() {
    let tempdir = TempDir::new().expect("tempdir");
    let _home = HomeEnvGuard::set(tempdir.path());
    let config_dir = tempdir.path().join(".config/acp-stack");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    write_cursor_registry_override(&config_dir);
    let mut config = test_config();
    let workspace = tempdir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    config.workspace.root = workspace.to_string_lossy().into_owned();
    config.workspace.uploads = workspace.join("uploads").to_string_lossy().into_owned();
    config.agent.cwd = Some(config.workspace.root.clone());
    let opencode_path = tempdir
        .path()
        .join(".config")
        .join("opencode")
        .join("opencode.json");
    std::fs::create_dir_all(opencode_path.parent().expect("path has parent"))
        .expect("opencode dir");
    std::fs::write(&opencode_path, "not json").expect("opencode config");
    let mut secrets =
        acp_stack::secrets::SecretStore::open_or_create(tempdir.path()).expect("secret store");
    secrets
        .set_many([("CURSOR_API_KEY", "cursor-secret")])
        .expect("cursor secret");
    let fixture_path = write_config_options_fixture(tempdir.path(), &["cursor/gpt-5.5"]);
    let _fixture_guard = EnvVarGuard::set("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &fixture_path);

    let harness = AgentHarness::spawn_with_config(config).await;
    let client = http().await;
    let response = client
        .post(format!("{}/v1/agent/switch", harness.base_url))
        .header("Authorization", admin_bearer())
        .json(&serde_json::json!({ "agent": "cursor", "drop": true }))
        .send()
        .await
        .expect("send switch");
    let status = response.status();
    let body_text = response.text().await.unwrap_or_default();
    assert_eq!(status, StatusCode::OK, "body: {body_text}");
    let body: Value = serde_json::from_str(&body_text).expect("switch json");
    assert_eq!(body["data"]["agent_id"], "cursor");
    assert!(
        body["data"]["cleanup_errors"]
            .as_array()
            .is_some_and(|errors| !errors.is_empty()),
        "cleanup error should be reported: {body}"
    );
    let written = std::fs::read_to_string(&harness.config_path).expect("read config");
    assert!(written.contains(r#"id = "cursor""#));
}

#[tokio::test]
async fn model_discovery_timeout_shuts_down_provisional_agent() {
    let _fixture_guard = EnvVarGuard::unset("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH");
    let tempdir = TempDir::new().expect("tempdir");
    let pid_path = tempdir.path().join("placebo-agent.pid");
    let mut config = test_config();
    config.agent.args = vec![
        "acp".into(),
        "--session-new-stall".into(),
        "--write-pid".into(),
        pid_path.to_string_lossy().into_owned(),
    ];

    let err = fetch_session_config_with_timeout(tempdir.path(), &config, Duration::from_millis(50))
        .await
        .expect_err("discovery should time out");
    assert_eq!(err.error_code(), "agent.initialize_failed");
    assert!(
        err.to_string().contains("model discovery exceeded"),
        "unexpected error: {err}",
    );

    #[cfg(unix)]
    {
        let pid_text = std::fs::read_to_string(&pid_path).expect("pid written");
        let pid: u32 = pid_text.trim().parse().expect("pid parses");
        for _ in 0..40 {
            if process_is_gone(pid) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("placebo-agent process {pid} still alive after discovery timeout");
    }
}

#[cfg(unix)]
fn process_is_gone(pid: u32) -> bool {
    let result = unsafe { libc::kill(pid as i32, 0) };
    result != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
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
async fn agent_switch_selects_existing_array_target_config() {
    let tempdir = TempDir::new().expect("tempdir");
    let _home = HomeEnvGuard::set(tempdir.path());
    let mut config = test_config();
    config.array.enabled = true;
    add_codex_placebo_target(&mut config);
    let harness = AgentHarness::spawn_with_config(config).await;
    let client = http().await;

    let primary_start = client
        .post(format!("{}/v1/agent/start", harness.base_url))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("start primary");
    assert_eq!(primary_start.status(), StatusCode::OK);

    let response = client
        .post(format!("{}/v1/agent/switch", harness.base_url))
        .header("Authorization", admin_bearer())
        .json(&json!({ "agent": "codex" }))
        .send()
        .await
        .expect("switch target");
    let status = response.status();
    let body: Value = response.json().await.expect("switch json");
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["agent_id"], "codex");
    assert_eq!(body["data"]["provider_status"], "selected");
    assert_eq!(body["data"]["restarted"], true);

    let status_body: Value = client
        .get(format!("{}/v1/array/status", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("send array status")
        .json()
        .await
        .expect("array status json");
    assert_eq!(status_body["data"]["primary_target"], "codex");
    let targets = status_body["data"]["targets"]
        .as_array()
        .expect("targets should be an array");
    assert!(
        targets
            .iter()
            .any(|target| { target["id"] == "codex" && target["process_state"] == "running" })
    );
    assert!(
        targets
            .iter()
            .any(|target| { target["id"] == "opencode" && target["process_state"] == "stopped" })
    );
}

#[tokio::test]
async fn agent_switch_existing_kimi_target_reports_canonical_secret_ref() {
    let tempdir = TempDir::new().expect("tempdir");
    let _home = HomeEnvGuard::set(tempdir.path());
    let mut secrets =
        acp_stack::secrets::SecretStore::open_or_create(tempdir.path()).expect("secret store");
    secrets
        .set_many([("KIMI_API_KEY", "kimi-secret")])
        .expect("kimi secret");

    let mut config = test_config();
    config.array.enabled = true;
    add_kimi_placebo_target(&mut config);
    let harness = AgentHarness::spawn_with_config(config).await;

    let response = http()
        .await
        .post(format!("{}/v1/agent/switch", harness.base_url))
        .header("Authorization", admin_bearer())
        .json(&json!({ "agent": "kimi" }))
        .send()
        .await
        .expect("switch target");
    let status = response.status();
    let body: Value = response.json().await.expect("switch json");

    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["agent_id"], "kimi");
    assert_eq!(body["data"]["provider_status"], "selected");
    assert_eq!(body["data"]["required_env_refs"], json!(["KIMI_API_KEY"]));
}

#[tokio::test]
async fn agent_switch_to_existing_running_target_keeps_it_running() {
    let tempdir = TempDir::new().expect("tempdir");
    let _home = HomeEnvGuard::set(tempdir.path());
    let mut config = test_config();
    config.array.enabled = true;
    add_codex_placebo_target(&mut config);
    let harness = AgentHarness::spawn_with_config(config).await;
    let client = http().await;

    let primary_start = client
        .post(format!("{}/v1/agent/start", harness.base_url))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("start primary");
    assert_eq!(primary_start.status(), StatusCode::OK);
    let secondary_start = client
        .post(format!(
            "{}/v1/array/targets/{}/start",
            harness.base_url, "codex"
        ))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("start secondary");
    assert_eq!(secondary_start.status(), StatusCode::OK);

    let response = client
        .post(format!("{}/v1/agent/switch", harness.base_url))
        .header("Authorization", admin_bearer())
        .json(&json!({ "agent": "codex" }))
        .send()
        .await
        .expect("switch target");
    let status = response.status();
    let body: Value = response.json().await.expect("switch json");
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["agent_id"], "codex");
    assert_eq!(body["data"]["restarted"], true);
    assert_eq!(body["data"]["restart_started"], false);

    let status_body: Value = client
        .get(format!("{}/v1/array/status", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("send array status")
        .json()
        .await
        .expect("array status json");
    let targets = status_body["data"]["targets"]
        .as_array()
        .expect("targets should be an array");
    assert!(
        targets
            .iter()
            .any(|target| { target["id"] == "codex" && target["process_state"] == "running" })
    );
    assert!(
        targets
            .iter()
            .any(|target| { target["id"] == "opencode" && target["process_state"] == "stopped" })
    );
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

// Content string that sits between the 1 MiB per-file content cap and the
// ~6 MiB whole-request cap, so the request layer admits it and the handler
// rejects it on the content cap.
const OVER_CONTENT_UNDER_REQUEST_BYTES: usize = 2 * 1_048_576;

/// The inspect route is layered with `RequestBodyLimitLayer(IMPORT_REQUEST_SIZE_LIMIT)`,
/// which is deliberately looser than the 1 MiB content cap the handler enforces.
/// A ~2 MiB content string must reach the handler and fail on the content cap;
/// a content string large enough to blow past the ~6 MiB request cap must be
/// rejected at the body-limit layer before the handler runs.
#[tokio::test]
async fn native_config_inspect_request_layer_defers_to_content_cap() {
    let harness = AgentHarness::spawn().await;
    let home = harness
        .config_path
        .parent()
        .expect("config path has parent")
        .to_path_buf();
    let _home = HomeEnvGuard::set(&home);
    let client = http().await;

    // Between the content cap and the request cap: reaches the handler, fails
    // on the content cap with `native_config_too_large` (HTTP 413).
    let over_content = "x".repeat(OVER_CONTENT_UNDER_REQUEST_BYTES);
    let response = client
        .post(format!(
            "{}/v1/agent/config/native/inspect",
            harness.base_url
        ))
        .header("Authorization", admin_bearer())
        .json(&json!({ "filename": "opencode.json", "content": over_content }))
        .send()
        .await
        .expect("send inspect");
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let body: Value = response.json().await.expect("inspect json");
    assert_eq!(body["error"]["code"], "native_config_too_large");

    // Past the whole-request cap: rejected at the body-limit layer. The
    // middleware response may not be JSON, so assert on status and only on
    // the envelope if the body parses as JSON.
    let over_request = "x".repeat(acp_stack::config::IMPORT_REQUEST_SIZE_LIMIT + 1_048_576);
    let response = client
        .post(format!(
            "{}/v1/agent/config/native/inspect",
            harness.base_url
        ))
        .header("Authorization", admin_bearer())
        .json(&json!({ "filename": "opencode.json", "content": over_request }))
        .send()
        .await
        .expect("send oversize inspect");
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let text = response.text().await.unwrap_or_default();
    if let Ok(body) = serde_json::from_str::<Value>(&text) {
        assert_ne!(
            body["error"]["code"], "native_config_too_large",
            "oversize request should be rejected by the body-limit layer, not the content cap: {body}"
        );
    }
}

/// Drives the full inspect -> import -> cancel rollback loop through the HTTP
/// layer with a model-free native config, so apply never triggers model
/// discovery or an agent launch. Covers the happy-path rollback and the
/// digest-guard rejection when the applied native file is mutated on disk,
/// plus admin-tier enforcement on the cancel route.
#[tokio::test]
async fn native_config_cancel_rolls_back_and_guards_digest() {
    let harness = AgentHarness::spawn().await;
    let home = harness
        .config_path
        .parent()
        .expect("config path has parent")
        .to_path_buf();
    let _home = HomeEnvGuard::set(&home);
    // The import prepare path opens the secret store read-only, so it must
    // exist under HOME even though `{"theme":"dark"}` carries no secret refs.
    acp_stack::secrets::SecretStore::open_or_create(&home).expect("secret store");
    let native_path = home.join(".config").join("opencode").join("opencode.json");
    let client = http().await;

    // Admin-tier enforcement: the cancel route rejects a session key with
    // `auth.wrong_kind` (401), matching the other admin-route tests here.
    let rejected = client
        .post(format!(
            "{}/v1/agent/config/native/import/op_missing/cancel",
            harness.base_url
        ))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("send session cancel");
    assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED);
    let rejected_body: Value = rejected.json().await.expect("json");
    assert_eq!(rejected_body["error"]["code"], "auth.wrong_kind");

    let canonical_before =
        std::fs::read(&harness.config_path).expect("canonical config before import");

    let operation_id = apply_theme_import(&client, &harness.base_url).await;

    // Applied without a running agent: no restart required, native file on disk.
    assert!(
        native_path.is_file(),
        "native file should exist after apply"
    );

    // Happy path: cancel rolls back, dropping the native file and restoring
    // the canonical config bytes verbatim.
    let cancel = client
        .post(format!(
            "{}/v1/agent/config/native/import/{operation_id}/cancel",
            harness.base_url
        ))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("send cancel");
    let status = cancel.status();
    let body: Value = cancel.json().await.expect("cancel json");
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["status"], "cancelled");
    assert!(
        !native_path.exists(),
        "native file should be removed after cancel rollback"
    );
    let canonical_after =
        std::fs::read(&harness.config_path).expect("canonical config after cancel");
    assert_eq!(
        canonical_before, canonical_after,
        "canonical config bytes should be restored by rollback"
    );

    // Digest guard: a fresh apply, then mutate the applied native file on
    // disk. Cancel must refuse with `native_config_rollback_conflict` (409)
    // rather than roll back over the tampered file.
    let guarded_operation_id = apply_theme_import(&client, &harness.base_url).await;
    assert!(
        native_path.is_file(),
        "native file should exist after apply"
    );
    let mut mutated = std::fs::read(&native_path).expect("read applied native file");
    mutated.extend_from_slice(b"\n// tampered\n");
    std::fs::write(&native_path, &mutated).expect("mutate applied native file");

    let guarded_cancel = client
        .post(format!(
            "{}/v1/agent/config/native/import/{guarded_operation_id}/cancel",
            harness.base_url
        ))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("send guarded cancel");
    let guarded_status = guarded_cancel.status();
    let guarded_body: Value = guarded_cancel.json().await.expect("guarded cancel json");
    assert_eq!(guarded_status, StatusCode::CONFLICT, "body: {guarded_body}");
    assert_eq!(
        guarded_body["error"]["code"],
        "native_config_rollback_conflict"
    );
}

#[tokio::test]
async fn native_config_import_serializes_with_agent_config_mutation_lock() {
    let harness = AgentHarness::spawn().await;
    let home = harness
        .config_path
        .parent()
        .expect("config path has parent")
        .to_path_buf();
    let _home = HomeEnvGuard::set(&home);
    acp_stack::secrets::SecretStore::open_or_create(&home).expect("secret store");
    let client = http().await;

    let inspect = client
        .post(format!(
            "{}/v1/agent/config/native/inspect",
            harness.base_url
        ))
        .header("Authorization", admin_bearer())
        .json(&json!({ "filename": "opencode.json", "content": r#"{"theme":"dark"}"# }))
        .send()
        .await
        .expect("send inspect");
    let inspect_body: Value = inspect.json().await.expect("inspect json");
    let revision = inspect_body["data"]["revision"]
        .as_str()
        .expect("inspect revision")
        .to_owned();

    // Hold the cross-process mutation lock; the import must block on it and
    // only complete after release. `acps agent set` and the other serialized
    // writers take the same lock, so this pins the import side of the pairing.
    let lock = acp_stack::fs_util::acquire_agent_config_mutation_file_lock(&harness.config_path)
        .expect("acquire mutation lock");
    let import_client = client.clone();
    let base_url = harness.base_url.clone();
    let import_task = tokio::spawn(async move {
        import_client
            .post(format!("{base_url}/v1/agent/config/native/import"))
            .header("Authorization", admin_bearer())
            .json(&json!({
                "revision": revision,
                "selected_managed_field_ids": [],
                "executable_settings_acknowledged": false
            }))
            .send()
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    assert!(
        !import_task.is_finished(),
        "import must wait while the mutation lock is held"
    );
    drop(lock);
    let import = import_task
        .await
        .expect("join import task")
        .expect("send import");
    let status = import.status();
    let body: Value = import.json().await.expect("import json");
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["status"], "applied", "body: {body}");
}

/// Inspect `{"theme":"dark"}` then import the empty selection, returning the
/// applied operation id. The theme-only config carries no model key, so apply
/// never triggers model discovery or an agent launch.
async fn apply_theme_import(client: &reqwest::Client, base_url: &str) -> String {
    let inspect = client
        .post(format!("{base_url}/v1/agent/config/native/inspect"))
        .header("Authorization", admin_bearer())
        .json(&json!({ "filename": "opencode.json", "content": r#"{"theme":"dark"}"# }))
        .send()
        .await
        .expect("send inspect");
    let inspect_status = inspect.status();
    let inspect_body: Value = inspect.json().await.expect("inspect json");
    assert_eq!(inspect_status, StatusCode::OK, "body: {inspect_body}");
    let revision = inspect_body["data"]["revision"]
        .as_str()
        .expect("inspect revision")
        .to_owned();

    let import = client
        .post(format!("{base_url}/v1/agent/config/native/import"))
        .header("Authorization", admin_bearer())
        .json(&json!({
            "revision": revision,
            "selected_managed_field_ids": [],
            "executable_settings_acknowledged": false
        }))
        .send()
        .await
        .expect("send import");
    let import_status = import.status();
    let import_body: Value = import.json().await.expect("import json");
    assert_eq!(import_status, StatusCode::OK, "body: {import_body}");
    assert_eq!(
        import_body["data"]["status"], "applied",
        "body: {import_body}"
    );
    assert_eq!(
        import_body["data"]["restart"]["required"], false,
        "no running agent, so no restart required: {import_body}"
    );
    import_body["data"]["operation_id"]
        .as_str()
        .expect("operation id")
        .to_owned()
}
