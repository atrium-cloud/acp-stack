//! End-to-end coverage for the agent HTTP routes: install, start,
//! capabilities, stop, and the session/admin tier enforcement on those.
//!
//! All tests drive a real `acps` HTTP server against a `Config` whose
//! `[agent].command` is the current test binary with an internal debug-only
//! fake-agent argv sentinel, which makes it speak ACP just well enough to
//! satisfy `initialize`.

use std::sync::Arc;

use acp_stack::api::{self, AppState};
use acp_stack::config::{AgentAdapterConfig, Config, load_config_from_str};
use acp_stack::state::StateStore;
use reqwest::StatusCode;
use serde_json::Value;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::Mutex as TokioMutex;
use tokio::task::JoinHandle;

const SESSION_KEY: &str = "acps_session_cccccccccccccccccccccccccccccccccccccccccccc";
const ADMIN_KEY: &str = "acps_admin_dddddddddddddddddddddddddddddddddddddddddddd";

struct AgentHarness {
    base_url: String,
    _tempdir: TempDir,
    state: Arc<TokioMutex<StateStore>>,
    join: JoinHandle<acp_stack::error::Result<()>>,
}

impl AgentHarness {
    async fn spawn() -> Self {
        Self::spawn_with_config(test_config()).await
    }

    async fn spawn_with_config(config: Config) -> Self {
        let tempdir = TempDir::new().expect("tempdir");
        let path = tempdir.path().join("state.sqlite");
        let store = StateStore::open(&path).expect("state open");
        store.migrate().expect("migrate");
        let app_state = AppState::new(config, store, SESSION_KEY.to_owned(), ADMIN_KEY.to_owned());
        let state = app_state.state.clone();
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let base_url = format!("http://{}", listener.local_addr().expect("local"));
        let join = tokio::spawn(async move { api::serve(app_state, listener).await });
        Self {
            base_url,
            _tempdir: tempdir,
            state,
            join,
        }
    }
}

struct HomeEnvGuard {
    previous: Option<std::ffi::OsString>,
}

impl HomeEnvGuard {
    fn set(home: &std::path::Path) -> Self {
        let previous = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", home);
        }
        Self { previous }
    }
}

impl Drop for HomeEnvGuard {
    fn drop(&mut self) {
        unsafe {
            match self.previous.take() {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
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
    assert_eq!(command, env!("CARGO_BIN_EXE_acps"));
}

impl Drop for AgentHarness {
    fn drop(&mut self) {
        self.join.abort();
    }
}

/// Build a test config that points `[agent].command` at the test binary in
/// fake-agent mode. Empty `[agent].env` so the handlers don't try to open
/// a secret store that doesn't exist in the test tempdir.
fn test_config() -> Config {
    let toml_text = include_str!("fixtures/valid-acp-stack.toml");
    let mut config = load_config_from_str(toml_text).expect("config parses");
    config.agent.command = env!("CARGO_BIN_EXE_acps").to_owned();
    config.agent.args = vec!["__acps-test-fake-agent".into()];
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

async fn http() -> reqwest::Client {
    reqwest::Client::builder().build().expect("reqwest client")
}

fn admin_bearer() -> String {
    format!("Bearer {ADMIN_KEY}")
}

fn session_bearer() -> String {
    format!("Bearer {SESSION_KEY}")
}

fn shell_quote_path(path: &std::path::Path) -> String {
    let text = path.to_string_lossy();
    format!("'{}'", text.replace('\'', "'\\''"))
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
