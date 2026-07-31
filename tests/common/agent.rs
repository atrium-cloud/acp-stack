//! Shared fixtures for the `agent_*_tests` binaries: an in-process server
//! harness whose `[agent].command` points at the standalone placebo ACP
//! fixture, plus the registry-override, skill, and model-discovery fixture
//! writers its tests drive.
//!
//! `tests/common/api.rs` and `tests/common/sessions.rs` define same-named
//! items (`SESSION_KEY`, `ADMIN_KEY`, `test_config`, ...) with different key
//! values and different signatures. The sets are deliberately separate — do
//! not merge or cross-import them.

use std::sync::Arc;

use acp_stack::api::{self, AppState, RuntimePaths};
use acp_stack::config::{
    AgentAdapterConfig, ArrayTargetConfig, Config, HttpHeaderRef, McpConfig, McpHttpServer,
    McpServerConfig, McpStdioServer, load_config_from_str,
};
use acp_stack::state::StateStore;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::Mutex as TokioMutex;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

pub const SESSION_KEY: &str = "acps_session_cccccccccccccccccccccccccccccccccccccccccccc";
pub const ADMIN_KEY: &str = "acps_admin_dddddddddddddddddddddddddddddddddddddddddddd";

pub struct AgentHarness {
    pub base_url: String,
    pub config_path: std::path::PathBuf,
    pub _tempdir: TempDir,
    pub state: Arc<TokioMutex<StateStore>>,
    pub join: JoinHandle<acp_stack::error::Result<()>>,
}

impl AgentHarness {
    pub async fn spawn() -> Self {
        Self::spawn_with_config(test_config()).await
    }

    pub async fn spawn_with_config(mut config: Config) -> Self {
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

static DISCOVERY_FIXTURE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub struct EnvVarGuard<'a> {
    _lock: std::sync::MutexGuard<'a, ()>,
    key: &'static str,
    previous: Option<std::ffi::OsString>,
}

impl EnvVarGuard<'_> {
    pub fn set(key: &'static str, value: &std::path::Path) -> Self {
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

    pub fn unset(key: &'static str) -> Self {
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

impl Drop for AgentHarness {
    fn drop(&mut self) {
        self.join.abort();
    }
}

/// Build a test config that points `[agent].command` at the placebo ACP
/// fixture. Empty `[agent].env` so the handlers don't try to open a secret
/// store that doesn't exist in the test tempdir.
pub fn test_config() -> Config {
    let toml_text = include_str!("../fixtures/valid-opencode-stack.toml");
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
        source_url: Some("https://github.com/agentclientprotocol/codex-acp".to_owned()),
    });
    // Replace the install recipe with something that completes in milliseconds.
    config.agent.install = Some(acp_stack::config::AgentInstallConfig {
        install_type: "shell".into(),
        creates: "true".into(),
        shell: Some("true".into()),
    });
    config
}

pub fn add_codex_placebo_target(config: &mut Config) {
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

pub fn add_kimi_placebo_target(config: &mut Config) {
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

pub async fn http() -> reqwest::Client {
    reqwest::Client::builder().build().expect("reqwest client")
}

pub fn admin_bearer() -> String {
    format!("Bearer {ADMIN_KEY}")
}

pub fn session_bearer() -> String {
    format!("Bearer {SESSION_KEY}")
}

pub fn websocket_url(harness: &AgentHarness) -> String {
    format!(
        "{}/v1/ws",
        harness
            .base_url
            .strip_prefix("http://")
            .map(|rest| format!("ws://{rest}"))
            .unwrap_or_else(|| harness.base_url.replace("http", "ws"))
    )
}

pub fn websocket_request(harness: &AgentHarness) -> http::Request<()> {
    let mut request = websocket_url(harness)
        .into_client_request()
        .expect("websocket request");
    request.headers_mut().insert(
        "Authorization",
        http::HeaderValue::from_str(&session_bearer()).expect("bearer header"),
    );
    request
}

pub fn shell_quote_path(path: &std::path::Path) -> String {
    let text = path.to_string_lossy();
    format!("'{}'", text.replace('\'', "'\\''"))
}

pub fn write_installed_skill(root: &std::path::Path, name: &str, descriptor: &str) {
    let skill_dir = root.join(name);
    std::fs::create_dir_all(&skill_dir).expect("skill dir");
    std::fs::write(skill_dir.join("SKILL.md"), descriptor).expect("descriptor");
    std::fs::write(skill_dir.join("script.sh"), "true\n").expect("script");
}

pub fn write_cursor_registry_override(config_dir: &std::path::Path) {
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

pub fn write_amp_registry_override(config_dir: &std::path::Path) {
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

pub fn write_pi_registry_override(config_dir: &std::path::Path) {
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

pub fn write_config_options_fixture(root: &std::path::Path, models: &[&str]) -> std::path::PathBuf {
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

pub fn switch_mcp_config() -> McpConfig {
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
                headers: vec![HttpHeaderRef::from_ref("Authorization", "LINEAR_API_KEY")],
            }),
        ],
    }
}
