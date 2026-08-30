//! Shared fixtures for the `agent_*_tests` binaries. `tests/common/api.rs` and
//! `tests/common/sessions.rs` define same-named items with different key values and signatures;
//! the sets are deliberately separate, so do not merge or cross-import them.

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
    pub home: std::path::PathBuf,
    // Option so `respawn` can move the dir out of this `Drop` type.
    tempdir: Option<TempDir>,
    pub state: Arc<TokioMutex<StateStore>>,
    pub join: JoinHandle<acp_stack::error::Result<()>>,
}

impl AgentHarness {
    pub async fn spawn() -> Self {
        Self::spawn_with_config(test_config()).await
    }

    pub async fn spawn_with_config(config: Config) -> Self {
        Self::spawn_with_config_and_optional_home(config, None).await
    }

    /// Spawn against a caller-owned HOME, for tests that seed files into it
    /// before the routes read them.
    pub async fn spawn_with_config_and_home(config: Config, home: std::path::PathBuf) -> Self {
        Self::spawn_with_config_and_optional_home(config, Some(home)).await
    }

    async fn spawn_with_config_and_optional_home(
        mut config: Config,
        home: Option<std::path::PathBuf>,
    ) -> Self {
        let tempdir = TempDir::new().expect("tempdir");
        if config.workspace.root == "/workspace" {
            let workspace = tempdir.path().join("workspace");
            config.workspace.root = workspace.to_string_lossy().into_owned();
            config.workspace.uploads = workspace.join("uploads").to_string_lossy().into_owned();
        }
        let home = home.unwrap_or_else(|| tempdir.path().to_path_buf());
        Self::spawn_in_tempdir(tempdir, config, home, true).await
    }

    /// Boot a fresh server on the same tempdir, reloading config from disk as a restarted `acps`
    /// process would.
    pub async fn respawn(mut self) -> Self {
        self.join.abort();
        let tempdir = self.tempdir.take().expect("harness tempdir");
        let config_path = tempdir.path().join("acps-config.toml");
        let content = std::fs::read_to_string(&config_path).expect("on-disk config read");
        let config = load_config_from_str(&content).expect("on-disk config parses");
        let home = self.home.clone();
        Self::spawn_in_tempdir(tempdir, config, home, false).await
    }

    async fn spawn_in_tempdir(
        tempdir: TempDir,
        config: Config,
        home: std::path::PathBuf,
        write_config: bool,
    ) -> Self {
        std::fs::create_dir_all(&home).expect("harness home");
        let path = tempdir.path().join("state.sqlite");
        let config_path = tempdir.path().join("acps-config.toml");
        if write_config {
            std::fs::write(
                &config_path,
                config.to_canonical_toml().expect("canonical test config"),
            )
            .expect("test config write");
        }
        let store = StateStore::open(&path).expect("state open");
        store.migrate().expect("migrate");
        let effective_bind = config.api.bind.clone();
        let runtime_paths = RuntimePaths::new(config_path.clone(), path, home.clone());
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
            home,
            tempdir: Some(tempdir),
            state,
            join,
        }
    }
}

static DISCOVERY_FIXTURE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub struct EnvVarGuard<'a> {
    _lock: std::sync::MutexGuard<'a, ()>,
    previous: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl EnvVarGuard<'_> {
    pub fn set(key: &'static str, value: &std::path::Path) -> Self {
        Self::set_many(vec![(key, value.as_os_str().to_os_string())])
    }

    pub fn unset(key: &'static str) -> Self {
        let lock = DISCOVERY_FIXTURE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = std::env::var_os(key);
        // SAFETY: the lock is held, so no concurrent test observes a partial mutation.
        unsafe {
            std::env::remove_var(key);
        }
        Self {
            _lock: lock,
            previous: vec![(key, previous)],
        }
    }

    /// Sets several fixture env vars under one lock acquisition: stacking single-key guards would
    /// deadlock on the non-reentrant mutex. Keys must be unique or the restore is wrong.
    pub fn set_many(pairs: Vec<(&'static str, std::ffi::OsString)>) -> Self {
        let lock = DISCOVERY_FIXTURE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        debug_assert!(
            pairs
                .iter()
                .map(|(key, _)| key)
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                == pairs.len(),
            "set_many keys must be unique",
        );
        let mut previous = Vec::with_capacity(pairs.len());
        // SAFETY: DISCOVERY_FIXTURE_LOCK serializes every test touching these vars.
        unsafe {
            for (key, value) in pairs {
                previous.push((key, std::env::var_os(key)));
                std::env::set_var(key, value);
            }
        }
        Self {
            _lock: lock,
            previous,
        }
    }
}

impl Drop for EnvVarGuard<'_> {
    fn drop(&mut self) {
        // SAFETY: the lock is still held during the restore.
        unsafe {
            for (key, previous) in std::mem::take(&mut self.previous) {
                match previous {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}

impl Drop for AgentHarness {
    fn drop(&mut self) {
        self.join.abort();
    }
}

/// A test config pointing `[agent].command` at the placebo ACP fixture. `[agent].env` stays empty
/// so handlers never open a secret store that does not exist in the tempdir.
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

/// Amp is the one registry agent without an endpoint field (it reaches its own backend over a
/// websocket), so it stands in wherever a test needs an override-incapable target.
pub fn add_amp_placebo_target(config: &mut Config) {
    let mut secondary = config.agent.clone();
    secondary.id = "amp".to_owned();
    secondary.name = "Amp Code".to_owned();
    secondary.command = env!("CARGO_BIN_EXE_placebo-agent").to_owned();
    secondary.args = vec!["acp".into()];
    secondary.env = vec!["AMP_API_KEY".to_owned()];
    secondary.cwd = Some(std::env::temp_dir().to_string_lossy().into_owned());
    secondary.expected_sha256 = None;
    secondary.install = Some(acp_stack::config::AgentInstallConfig {
        install_type: "shell".into(),
        creates: "true".into(),
        shell: Some("true".into()),
    });
    config.array.targets.push(ArrayTargetConfig {
        id: "amp".to_owned(),
        agent: secondary,
    });
}

pub fn add_hermes_placebo_target(config: &mut Config) {
    let mut secondary = config.agent.clone();
    secondary.id = "hermes".to_owned();
    secondary.name = "Hermes Agent".to_owned();
    secondary.command = env!("CARGO_BIN_EXE_placebo-agent").to_owned();
    secondary.args = vec!["acp".into()];
    secondary.env = vec!["OPENROUTER_API_KEY".to_owned()];
    secondary.cwd = Some(std::env::temp_dir().to_string_lossy().into_owned());
    secondary.expected_sha256 = None;
    secondary.install = Some(acp_stack::config::AgentInstallConfig {
        install_type: "shell".into(),
        creates: "true".into(),
        shell: Some("true".into()),
    });
    config.array.targets.push(ArrayTargetConfig {
        id: "hermes".to_owned(),
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
    // Removal refuses directories without this install-time marker.
    std::fs::write(skill_dir.join(".acp-stack-managed"), "test-source\n").expect("marker");
}

pub fn write_kimi_registry_override(config_dir: &std::path::Path) {
    write_kimi_registry_override_with_command(config_dir, "true");
}

/// `write_kimi_registry_override` with a caller-chosen harness command, so a test can point at a
/// binary that does not exist yet and create it between attempts.
pub fn write_kimi_registry_override_with_command(config_dir: &std::path::Path, command: &str) {
    let body = format!(
        r#"
[[agents]]
id = "kimi"
name = "Kimi Code"
kind = "native"
headless_compatible = true
set_model = true
set_mode = true
supports_agent_skills = true
agent_skills_install_dir = "~/.agents/skills"
support_doc = "docs/agents/kimi.md"

[agents.harness]
id = "{command}"

[agents.harness.install.shell]
script = "true"
creates = "true"
"#
    );
    std::fs::write(config_dir.join("agents.toml"), body).expect("registry override");
}

/// Deliberately model-less synthetic amp entry: the embedded registry has no `set_model = false`
/// agent left, so capability-gate tests point here instead of the real catalog flags.
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

/// Same deliberately model-less synthetic amp shape as
/// `write_amp_registry_override`, plus a skills link directory.
pub fn write_amp_linked_skills_registry_override(config_dir: &std::path::Path) {
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
agent_skills_install_dir = "~/.agents/skills"
agent_skills_link_dir = "~/.amp/skills"
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

/// An executable shim that execs the placebo ACP binary, so a test can flip a start from failing to
/// succeeding without editing the config (which would change the switch journal's fingerprint).
pub fn write_placebo_shim(path: &std::path::Path) {
    let body = format!(
        "#!/bin/sh\nexec '{}' acp\n",
        env!("CARGO_BIN_EXE_placebo-agent")
    );
    std::fs::write(path, body).expect("placebo shim write");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .expect("placebo shim chmod");
    }
}

/// `write_placebo_shim` that exits 1 until `marker` exists. The shim file is present from the
/// start so the installer's resolve-and-spawn gate passes and only the agent launch fails.
pub fn write_gated_placebo_shim(path: &std::path::Path, marker: &std::path::Path) {
    let body = format!(
        "#!/bin/sh\nif [ ! -f '{}' ]; then\n  echo 'gated placebo shim: marker missing' >&2\n  exit 1\nfi\nexec '{}' acp\n",
        marker.to_string_lossy(),
        env!("CARGO_BIN_EXE_placebo-agent")
    );
    std::fs::write(path, body).expect("gated placebo shim write");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .expect("gated placebo shim chmod");
    }
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

/// Serve a fixed OpenAI-shaped `GET /models` payload locally; point
/// `ACP_STACK_PROVIDER_MODELS_BASE` at the returned base URL so fetches never leave the test host.
pub fn spawn_provider_models_server(payload: serde_json::Value) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind models fixture");
    let base = format!(
        "http://{}",
        listener.local_addr().expect("models fixture addr")
    );
    std::thread::spawn(move || {
        use std::io::{BufRead as _, BufReader, Write as _};
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let reader_stream = stream.try_clone().expect("clone models fixture stream");
            let mut reader = BufReader::new(reader_stream);
            let mut request_line = String::new();
            if reader.read_line(&mut request_line).is_err() {
                continue;
            }
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) if line == "\r\n" || line == "\n" => break,
                    Ok(_) => continue,
                    Err(_) => break,
                }
            }
            let body = payload.to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    base
}
