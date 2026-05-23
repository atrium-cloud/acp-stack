//! End-to-end coverage for the agent HTTP routes: install, start,
//! capabilities, stop, and the session/admin tier enforcement on those.
//!
//! All tests drive a real `acps` HTTP server against a `Config` whose
//! `[agent].command` is the current test binary with an internal debug-only
//! fake-agent argv sentinel, which makes it speak ACP just well enough to
//! satisfy `initialize`.

use std::{sync::Arc, time::Duration};

use acp_stack::api::{self, AppState};
use acp_stack::config::{AgentAdapterConfig, Config, load_config_from_str};
use acp_stack::runtime::model_discovery::fetch_session_config_with_timeout;
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
async fn model_discovery_timeout_shuts_down_provisional_agent() {
    let _fixture_guard = EnvVarGuard::unset("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH");
    let tempdir = TempDir::new().expect("tempdir");
    let pid_path = tempdir.path().join("fake-agent.pid");
    let mut config = test_config();
    config.agent.args = vec![
        "__acps-test-fake-agent".into(),
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
        panic!("fake-agent process {pid} still alive after discovery timeout");
    }
}

#[cfg(unix)]
fn process_is_gone(pid: u32) -> bool {
    let result = unsafe { libc::kill(pid as i32, 0) };
    result != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

#[tokio::test]
async fn agent_restart_starts_when_not_running() {
    // POST /v1/agent/restart on a stopped supervisor degenerates into
    // a plain start. Confirms the endpoint exists, is admin-tier, and
    // returns the same capability payload as `agent/start`. The
    // handler re-reads the config from disk on each call (so a prior
    // `acps agent set` is visible), so the test writes a valid config
    // file at the default location before calling.
    let home = TempDir::new().expect("home tempdir");
    let config_dir = home.path().join(".config").join("acp-stack");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    let config_text = include_str!("fixtures/valid-acp-stack.toml");
    std::fs::write(config_dir.join("acp-stack.toml"), config_text).expect("write config");
    // Point the test config at the same fake-agent binary as the
    // default harness, so the on-disk config the restart handler
    // re-reads is compatible with what the supervisor expects.
    let on_disk = std::fs::read_to_string(config_dir.join("acp-stack.toml")).expect("read config");
    let patched = on_disk
        .replace(
            "command = \"opencode\"",
            &format!("command = \"{}\"", env!("CARGO_BIN_EXE_acps")),
        )
        .replace("args = [\"acp\"]", "args = [\"__acps-test-fake-agent\"]")
        // Skip the secret-store dependency — the test home has no
        // age key or encrypted store; an empty env list lets the
        // restart handler bypass `SecretStore::open`.
        .replace("env = [\"OPENCODE_API_KEY\"]", "env = []")
        // The default cwd in the fixture is `/workspace`, which does
        // not exist in this tempdir. Point at the OS temp dir, which
        // is always present and writable — matches what
        // `test_config()` uses for the standard harness.
        .replace(
            "cwd = \"/workspace\"",
            &format!("cwd = \"{}\"", std::env::temp_dir().display()),
        );
    std::fs::write(config_dir.join("acp-stack.toml"), patched).expect("write patched config");

    let _home_guard = HomeEnvGuard::set(home.path());
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

    let home = TempDir::new().expect("home tempdir");
    let config_dir = home.path().join(".config").join("acp-stack");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    let config_path = config_dir.join("acp-stack.toml");

    // Write initial config that the daemon will cache at AppState
    // construction. `[agent].id = "opencode"` matches the harness
    // default; we change the model later to prove the restart picks
    // it up.
    let initial = include_str!("fixtures/valid-acp-stack.toml")
        .replace(
            "command = \"opencode\"",
            &format!("command = \"{}\"", env!("CARGO_BIN_EXE_acps")),
        )
        .replace("args = [\"acp\"]", "args = [\"__acps-test-fake-agent\"]")
        .replace("env = [\"OPENCODE_API_KEY\"]", "env = []")
        .replace(
            "cwd = \"/workspace\"",
            &format!("cwd = \"{}\"", std::env::temp_dir().display()),
        );
    std::fs::write(&config_path, &initial).expect("write initial config");

    let _home_guard = HomeEnvGuard::set(home.path());
    let harness = AgentHarness::spawn().await;
    let client = http().await;

    // Simulate `acps agent set` mutating the config on disk AFTER
    // the daemon has cached its own copy. Point `command` at a path
    // that absolutely cannot resolve to a binary; the supervisor's
    // spawn step reads this field directly. If the handler reads
    // from disk on each restart (the intended behavior), the spawn
    // fails with `agent.spawn_failed`. If it regressed to using the
    // cached `state.config`, restart would succeed with the original
    // valid binary path and this assertion would fail.
    let mutated = initial.replace(
        &format!("command = \"{}\"", env!("CARGO_BIN_EXE_acps")),
        "command = \"/nonexistent/absolutely-not-a-binary\"",
    );
    std::fs::write(&config_path, &mutated).expect("write mutated config");

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
