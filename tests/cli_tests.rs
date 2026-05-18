use acp_stack::api::{self, AppState, RuntimePaths};
use acp_stack::config::load_config_from_str;
use acp_stack::secrets::SecretStore;
use acp_stack::state::StateStore;
use assert_cmd::Command;
use base64::Engine;
use predicates::prelude::PredicateBooleanExt as _;
use serde_json::Value;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

const VALID_CONFIG: &str = include_str!("fixtures/valid-acp-stack.toml");
const SESSION_KEY: &str = "acps_session_cccccccccccccccccccccccccccccccccccccccccccc";
const ADMIN_KEY: &str = "acps_admin_dddddddddddddddddddddddddddddddddddddddddddd";

struct AgentCliHarness {
    base_url: String,
    join: JoinHandle<acp_stack::error::Result<()>>,
    _tempdir: TempDir,
}

impl AgentCliHarness {
    async fn spawn() -> Self {
        Self::spawn_inner(None).await
    }

    /// Spawn a harness that reports a custom `effective_bind` to the security
    /// check. Used to drive findings like `api.public_bind` from the CLI side
    /// without rewriting the actual TCP bind (we always bind to `127.0.0.1:0`
    /// for the test listener).
    async fn spawn_with_effective_bind(effective_bind: &str) -> Self {
        Self::spawn_inner(Some(effective_bind.to_owned())).await
    }

    async fn spawn_inner(effective_bind: Option<String>) -> Self {
        let tempdir = TempDir::new().expect("tempdir");
        let path = tempdir.path().join("state.sqlite");
        let store = StateStore::open(&path).expect("state open");
        store.migrate().expect("migrate");
        let config_path = create_runtime_files(tempdir.path(), &path);
        let runtime_paths = RuntimePaths::new(config_path, path.clone());
        let mut config = load_config_from_str(VALID_CONFIG).expect("config parses");
        config.agent.command = env!("CARGO_BIN_EXE_acps").to_owned();
        config.agent.args = vec!["__acps-test-fake-agent".into()];
        config.agent.env = vec![];
        config.agent.cwd = Some(std::env::temp_dir().to_string_lossy().into_owned());
        config.agent.expected_sha256 = None;
        let app_state = match effective_bind {
            Some(bind) => AppState::with_effective_bind_and_runtime_paths(
                config,
                store,
                SESSION_KEY.to_owned(),
                ADMIN_KEY.to_owned(),
                bind,
                runtime_paths,
            ),
            None => AppState::with_effective_bind_and_runtime_paths(
                config,
                store,
                SESSION_KEY.to_owned(),
                ADMIN_KEY.to_owned(),
                "127.0.0.1:7700".to_owned(),
                runtime_paths,
            ),
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let base_url = format!("http://{}", listener.local_addr().expect("local"));
        let join = tokio::spawn(async move { api::serve(app_state, listener).await });
        Self {
            base_url,
            join,
            _tempdir: tempdir,
        }
    }
}

impl Drop for AgentCliHarness {
    fn drop(&mut self) {
        self.join.abort();
    }
}

fn create_runtime_files(
    root: &std::path::Path,
    state_path: &std::path::Path,
) -> std::path::PathBuf {
    let config_dir = root.join(".config/acp-stack");
    let state_dir = state_path.parent().expect("state parent").to_path_buf();
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::create_dir_all(&state_dir).expect("state dir should be created");
    let config_path = config_dir.join("acp-stack.toml");
    let age_key_path = config_dir.join("age.key");
    let secret_store_path = state_dir.join("secrets.age");
    fs::write(&config_path, "test config").expect("config should be written");
    fs::write(&age_key_path, "test age key").expect("age key should be written");
    fs::write(&secret_store_path, "test secret store").expect("secret store should be written");
    #[cfg(unix)]
    {
        fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o700))
            .expect("config dir permissions should be set");
        fs::set_permissions(&state_dir, fs::Permissions::from_mode(0o700))
            .expect("state dir permissions should be set");
        for file in [&config_path, &age_key_path, state_path, &secret_store_path] {
            fs::set_permissions(file, fs::Permissions::from_mode(0o600))
                .expect("runtime file permissions should be set");
        }
    }
    config_path
}

fn write_cli_home(home: &std::path::Path, base_url: &str, admin_key: &str) {
    let config_dir = home.join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = VALID_CONFIG
        .replace(
            r#"public_url = "https://agent.example.com""#,
            &format!(r#"public_url = "{base_url}""#),
        )
        .replace(r#"env = ["OPENCODE_API_KEY"]"#, "env = []");
    fs::write(config_dir.join("acp-stack.toml"), config).expect("config should be written");
    let mut store = SecretStore::open_or_create(home).expect("secret store should open");
    store
        .set_many([
            ("ACP_STACK_SESSION_KEY", SESSION_KEY),
            ("ACP_STACK_ADMIN_KEY", admin_key),
        ])
        .expect("auth keys should be stored");
}

#[test]
fn prints_version() {
    let mut command = Command::cargo_bin("acps").expect("binary should build");

    command
        .arg("--version")
        .assert()
        .success()
        .stdout(predicates::str::contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn security_check_is_listed_in_help() {
    Command::cargo_bin("acps")
        .expect("binary should build")
        .args(["security", "--help"])
        .assert()
        .success()
        .stdout(predicates::str::contains("check"))
        .stdout(predicates::str::contains("runtime security self-check"));
}

#[test]
fn validates_explicit_config_path() {
    let mut command = Command::cargo_bin("acps").expect("binary should build");

    command
        .args(["config", "validate", "tests/fixtures/valid-acp-stack.toml"])
        .assert()
        .success()
        .stdout(predicates::str::contains("config is valid"));
}

#[test]
fn validate_failure_exits_nonzero_with_specific_error() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("invalid.toml");
    fs::write(
        &path,
        VALID_CONFIG.replace(r#"bind = "127.0.0.1:7700""#, r#"bind = "bad""#),
    )
    .expect("invalid config should be written");

    let mut command = Command::cargo_bin("acps").expect("binary should build");

    command
        .args([
            "config",
            "validate",
            path.to_str().expect("path should be UTF-8"),
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "api.bind must be a socket address",
        ));
}

#[test]
fn exports_default_home_config_to_stdout() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acp-stack.toml"), VALID_CONFIG).expect("config should be written");

    let mut command = Command::cargo_bin("acps").expect("binary should build");

    command
        .env("HOME", tempdir.path())
        .args(["config", "export"])
        .assert()
        .success()
        .stdout(predicates::str::contains("[api]"))
        .stdout(predicates::str::contains("[agent.install]"));
}

#[test]
fn exports_base64_default_home_config() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acp-stack.toml"), VALID_CONFIG).expect("config should be written");

    let mut command = Command::cargo_bin("acps").expect("binary should build");
    let output = command
        .env("HOME", tempdir.path())
        .args(["config", "export", "--base64"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let encoded = String::from_utf8(output).expect("stdout should be UTF-8");
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .expect("stdout should be base64 TOML");
    let toml = String::from_utf8(decoded).expect("decoded TOML should be UTF-8");

    assert!(toml.contains("[api]"));
    assert!(toml.contains("[agent.install]"));
}

#[test]
fn exports_default_home_config_to_output_path() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acp-stack.toml"), VALID_CONFIG).expect("config should be written");
    let output_path = tempdir.path().join("exported.toml");

    let mut command = Command::cargo_bin("acps").expect("binary should build");

    command
        .env("HOME", tempdir.path())
        .args([
            "config",
            "export",
            "--output",
            output_path.to_str().expect("path should be UTF-8"),
        ])
        .assert()
        .success()
        .stdout("");

    let exported = fs::read_to_string(output_path).expect("export should be readable");
    assert!(exported.contains("[api]"));
    assert!(exported.contains("[agent.install]"));
}

#[test]
fn init_creates_config_and_state() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    let mut command = Command::cargo_bin("acps").expect("binary should build");

    command
        .env("HOME", tempdir.path())
        .arg("init")
        .assert()
        .success()
        .stdout(predicates::str::contains("initialized acp-stack"));

    let config_path = tempdir.path().join(".config/acp-stack/acp-stack.toml");
    let state_path = tempdir.path().join(".local/share/acp-stack/state.sqlite");
    assert!(config_path.is_file());
    assert!(state_path.is_file());

    let config = fs::read_to_string(config_path).expect("starter config should be readable");
    assert!(config.contains("[workspace.source]"));
    assert!(config.contains(r#"type = "none""#));
}

#[test]
fn init_skips_opencode_config_without_configured_provider() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acp-stack.toml"), VALID_CONFIG).expect("config should be written");

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .arg("init")
        .assert()
        .success()
        .stdout(predicates::str::contains("OpenCode config:").not());

    let opencode_path = tempdir
        .path()
        .join(".config")
        .join("opencode")
        .join("opencode.json");
    assert!(!opencode_path.exists());
}

#[test]
fn init_provider_sets_opencode_auth_config_without_model() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .args([
            "init",
            "--agent",
            "opencode",
            "--provider",
            "openai",
            "--api-key-ref",
            "OPENAI_API_KEY",
            "--no-install-agent",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("OpenCode config:"));

    let config = fs::read_to_string(tempdir.path().join(".config/acp-stack/acp-stack.toml"))
        .expect("config should be readable");
    assert!(config.contains("[agent.provider]"));
    assert!(config.contains(r#"id = "openai""#));
    assert!(config.contains(r#"api_key_ref = "OPENAI_API_KEY""#));
    assert!(!config.contains(r#"model ="#));
    assert!(config.contains(r#"env = ["OPENAI_API_KEY"]"#));
    assert!(!config.contains(r#""OPENCODE_API_KEY""#));

    let opencode_path = tempdir
        .path()
        .join(".config")
        .join("opencode")
        .join("opencode.json");
    let opencode: Value = serde_json::from_str(
        &fs::read_to_string(opencode_path).expect("opencode config should be readable"),
    )
    .expect("opencode config should parse");
    assert!(opencode.get("model").is_none());
    assert_eq!(
        opencode["provider"]["openai"]["options"]["apiKey"],
        "{env:OPENAI_API_KEY}"
    );
}

#[test]
fn init_provider_failure_does_not_persist_selected_agent() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acp-stack.toml"), VALID_CONFIG).expect("config should be written");

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .args([
            "init",
            "--agent",
            "amp",
            "--provider",
            "openai",
            "--no-install-agent",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "Amp Code does not support provider configuration during init",
        ));

    let config =
        fs::read_to_string(config_dir.join("acp-stack.toml")).expect("config should be readable");
    assert!(config.contains(r#"id = "opencode""#));
    assert!(!config.contains(r#"id = "amp""#));
    assert!(!config.contains("[agent.provider]"));
}

#[test]
fn init_skips_pi_model_scope_without_configured_provider() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = VALID_CONFIG
        .replace(r#"id = "opencode""#, r#"id = "pi""#)
        .replace(r#"name = "OpenCode""#, r#"name = "Pi Agent""#)
        .replace(r#"command = "opencode""#, r#"command = "pi-acp""#)
        .replace(r#"args = ["acp"]"#, r#"args = []"#)
        .replace(
            r#"
[agent.install]
type = "shell"
shell = "curl -fsSL https://opencode.ai/install | bash"
creates = "opencode"
"#,
            "",
        );
    fs::write(config_dir.join("acp-stack.toml"), config).expect("config should be written");

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .arg("init")
        .assert()
        .success()
        .stdout(predicates::str::contains("Pi settings:").not());

    let pi_settings_path = tempdir
        .path()
        .join(".pi")
        .join("agent")
        .join("settings.json");
    assert!(!pi_settings_path.exists());
}

#[test]
fn agent_set_updates_config_and_generated_opencode_provider() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acp-stack.toml"), VALID_CONFIG).expect("config should be written");
    let options_path = write_acp_config_options(tempdir.path(), &["openai/gpt-5.5"], &[]);

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args([
            "agent",
            "set",
            "--provider",
            "openai",
            "--model",
            "openai/gpt-5.5",
            "--api-key-ref",
            "OPENAI_API_KEY",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent: configured"))
        .stdout(predicates::str::contains("api_key_ref: OPENAI_API_KEY"));

    let config = fs::read_to_string(config_dir.join("acp-stack.toml"))
        .expect("updated config should be readable");
    assert!(config.contains("[agent.provider]"));
    assert!(config.contains(r#"id = "openai""#));
    assert!(config.contains(r#"model = "openai/gpt-5.5""#));
    assert!(config.contains(r#"api_key_ref = "OPENAI_API_KEY""#));
    assert!(config.contains(r#""OPENCODE_API_KEY""#));
    assert!(config.contains(r#""OPENAI_API_KEY""#));

    let opencode_path = tempdir
        .path()
        .join(".config")
        .join("opencode")
        .join("opencode.json");
    let opencode: Value = serde_json::from_str(
        &fs::read_to_string(opencode_path).expect("opencode config should be readable"),
    )
    .expect("opencode config should parse");
    assert_eq!(opencode["model"], "openai/gpt-5.5");
    assert_eq!(
        opencode["provider"]["openai"]["options"]["apiKey"],
        "{env:OPENAI_API_KEY}"
    );
}

#[test]
fn agent_set_goose_provider_updates_generated_config() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = VALID_CONFIG
        .replace(r#"id = "opencode""#, r#"id = "goose""#)
        .replace(r#"name = "OpenCode""#, r#"name = "Goose""#)
        .replace(r#"command = "opencode""#, r#"command = "goose""#)
        .replace(
            r#"env = ["OPENCODE_API_KEY"]"#,
            r#"env = ["OPENROUTER_API_KEY"]"#,
        )
        .replace(
            r#"
[agent.install]
type = "shell"
shell = "curl -fsSL https://opencode.ai/install | bash"
creates = "opencode"
"#,
            "",
        );
    fs::write(config_dir.join("acp-stack.toml"), config).expect("config should be written");
    let options_path =
        write_acp_config_options(tempdir.path(), &["deepseek/deepseek-v4-flash"], &[]);

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args([
            "agent",
            "set",
            "--provider",
            "openrouter",
            "--model",
            "deepseek/deepseek-v4-flash",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent: configured"))
        .stdout(predicates::str::contains("api_key_ref: OPENROUTER_API_KEY"))
        .stdout(predicates::str::contains("Goose config:"));

    let config = fs::read_to_string(config_dir.join("acp-stack.toml"))
        .expect("updated config should be readable");
    assert!(config.contains("[agent.provider]"));
    assert!(config.contains(r#"id = "openrouter""#));
    assert!(config.contains(r#"model = "deepseek/deepseek-v4-flash""#));
    assert!(config.contains(r#"api_key_ref = "OPENROUTER_API_KEY""#));

    let goose_path = tempdir
        .path()
        .join(".config")
        .join("goose")
        .join("config.yaml");
    let goose: serde_yaml::Value = serde_yaml::from_str(
        &fs::read_to_string(goose_path).expect("goose config should be readable"),
    )
    .expect("goose config should parse");
    assert_eq!(goose["GOOSE_PROVIDER"], "openrouter");
    assert_eq!(goose["GOOSE_MODEL"], serde_yaml::Value::Null);
}

#[test]
fn agent_set_cursor_accepts_openai_model_from_acp_options() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = VALID_CONFIG
        .replace(r#"id = "opencode""#, r#"id = "cursor""#)
        .replace(r#"name = "OpenCode""#, r#"name = "Cursor CLI""#)
        .replace(r#"command = "opencode""#, r#"command = "cursor-agent""#)
        .replace(
            r#"env = ["OPENCODE_API_KEY"]"#,
            r#"env = ["CURSOR_API_KEY"]"#,
        )
        .replace(
            r#"
[agent.install]
type = "shell"
shell = "curl -fsSL https://opencode.ai/install | bash"
creates = "opencode"
"#,
            "",
        );
    fs::write(config_dir.join("acp-stack.toml"), &config).expect("config should be written");
    let options_path = write_acp_config_options(
        tempdir.path(),
        &["gpt-5.5[context=272k,reasoning=medium,fast=false]"],
        &[],
    );

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args(["agent", "set", "--model", "gpt-5.5"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "required_env_refs: CURSOR_API_KEY",
        ));

    let after =
        fs::read_to_string(config_dir.join("acp-stack.toml")).expect("config should be readable");
    assert!(after.contains(r#"env = ["CURSOR_API_KEY"]"#));
    assert!(!after.contains("[agent.provider]"));
    assert!(after.contains(r#"model = "gpt-5.5[context=272k,reasoning=medium,fast=false]""#));
    assert!(!after.contains(r#"api_key_ref = "CURSOR_API_KEY""#));
}

#[test]
fn agent_set_cursor_rejects_provider_argument() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = VALID_CONFIG
        .replace(r#"id = "opencode""#, r#"id = "cursor""#)
        .replace(r#"name = "OpenCode""#, r#"name = "Cursor CLI""#)
        .replace(r#"command = "opencode""#, r#"command = "cursor-agent""#)
        .replace(
            r#"env = ["OPENCODE_API_KEY"]"#,
            r#"env = ["CURSOR_API_KEY"]"#,
        )
        .replace(
            r#"
[agent.install]
type = "shell"
shell = "curl -fsSL https://opencode.ai/install | bash"
creates = "opencode"
"#,
            "",
        );
    fs::write(config_dir.join("acp-stack.toml"), &config).expect("config should be written");

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .args(["agent", "set", "--provider", "openai", "--model", "gpt-5.5"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "Cursor CLI does not support provider configuration",
        ));

    let after =
        fs::read_to_string(config_dir.join("acp-stack.toml")).expect("config should be readable");
    assert!(!after.contains("[agent.provider]"));
}

#[test]
fn agent_set_opencode_rejects_model_without_provider() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acp-stack.toml"), VALID_CONFIG).expect("config should be written");

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .args(["agent", "set", "--model", "gpt-5.5"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "pass --provider <provider-id> when setting a model for OpenCode",
        ));
}

#[test]
fn agent_set_rejects_provider_not_supported_by_agent() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acp-stack.toml"), VALID_CONFIG).expect("config should be written");

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .args([
            "agent",
            "set",
            "--provider",
            "fireworks",
            "--model",
            "fireworks/test-model",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "provider `fireworks` is not supported for agent `opencode`",
        ));

    let after =
        fs::read_to_string(config_dir.join("acp-stack.toml")).expect("config should be readable");
    assert!(!after.contains("[agent.provider]"));
}

#[test]
fn agent_set_rejects_providers_without_api_key_mapping() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acp-stack.toml"), VALID_CONFIG).expect("config should be written");

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .args([
            "agent",
            "set",
            "--provider",
            "google-vertex",
            "--model",
            "google-vertex/test-model",
            "--api-key-ref",
            "GOOGLE_APPLICATION_CREDENTIALS",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "provider `google-vertex` has no API-key env mapping for agent `opencode`",
        ));

    let after =
        fs::read_to_string(config_dir.join("acp-stack.toml")).expect("config should be readable");
    assert!(!after.contains("[agent.provider]"));
}

#[test]
fn agent_set_adds_cloudflare_companion_refs() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = VALID_CONFIG
        .replace(r#"id = "opencode""#, r#"id = "pi""#)
        .replace(r#"name = "OpenCode""#, r#"name = "Pi Agent""#)
        .replace(r#"command = "opencode""#, r#"command = "pi-acp""#)
        .replace(r#"args = ["acp"]"#, r#"args = []"#)
        .replace(
            r#"
[agent.install]
type = "shell"
shell = "curl -fsSL https://opencode.ai/install | bash"
creates = "opencode"
"#,
            "",
        );
    fs::write(config_dir.join("acp-stack.toml"), config).expect("config should be written");
    let options_path = write_acp_config_options(
        tempdir.path(),
        &["workers-ai/@cf/moonshotai/kimi-k2.6"],
        &[],
    );

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args([
            "agent",
            "set",
            "--provider",
            "cloudflare-ai-gateway",
            "--model",
            "workers-ai/@cf/moonshotai/kimi-k2.6",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "required_env_refs: CLOUDFLARE_API_KEY, CLOUDFLARE_ACCOUNT_ID, CLOUDFLARE_GATEWAY_ID",
        ));

    let config = fs::read_to_string(config_dir.join("acp-stack.toml"))
        .expect("updated config should be readable");
    assert!(config.contains(r#"id = "cloudflare-ai-gateway""#));
    assert!(config.contains(r#"model = "workers-ai/@cf/moonshotai/kimi-k2.6""#));
    assert!(config.contains(r#""CLOUDFLARE_API_KEY""#));
    assert!(config.contains(r#""CLOUDFLARE_ACCOUNT_ID""#));
    assert!(config.contains(r#""CLOUDFLARE_GATEWAY_ID""#));
}

#[test]
fn agent_set_opencode_cloudflare_gateway_uses_token_ref() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acp-stack.toml"), VALID_CONFIG).expect("config should be written");
    let options_path = write_acp_config_options(
        tempdir.path(),
        &["cloudflare-ai-gateway/workers-ai/@cf/moonshotai/kimi-k2.6"],
        &[],
    );

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args([
            "agent",
            "set",
            "--provider",
            "cloudflare-ai-gateway",
            "--model",
            "cloudflare-ai-gateway/workers-ai/@cf/moonshotai/kimi-k2.6",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "api_key_ref: CLOUDFLARE_API_TOKEN",
        ))
        .stdout(predicates::str::contains(
            "required_env_refs: CLOUDFLARE_API_TOKEN, CLOUDFLARE_ACCOUNT_ID, CLOUDFLARE_GATEWAY_ID",
        ));

    let config = fs::read_to_string(config_dir.join("acp-stack.toml"))
        .expect("updated config should be readable");
    assert!(config.contains(r#"api_key_ref = "CLOUDFLARE_API_TOKEN""#));
    assert!(config.contains(r#""CLOUDFLARE_API_TOKEN""#));
    assert!(config.contains(r#""CLOUDFLARE_ACCOUNT_ID""#));
    assert!(config.contains(r#""CLOUDFLARE_GATEWAY_ID""#));
    assert!(!config.contains(r#""CLOUDFLARE_API_KEY""#));

    let opencode_path = tempdir
        .path()
        .join(".config")
        .join("opencode")
        .join("opencode.json");
    let opencode: Value = serde_json::from_str(
        &fs::read_to_string(opencode_path).expect("opencode config should be readable"),
    )
    .expect("opencode config should parse");
    assert_eq!(
        opencode["model"],
        "cloudflare-ai-gateway/workers-ai/@cf/moonshotai/kimi-k2.6"
    );
    assert_eq!(
        opencode["provider"]["cloudflare-ai-gateway"]["options"]["apiKey"],
        "{env:CLOUDFLARE_API_TOKEN}"
    );
}

#[test]
fn agent_set_without_model_lists_choices_without_mutating_config() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acp-stack.toml"), VALID_CONFIG).expect("config should be written");
    let options_path = write_acp_config_options(
        tempdir.path(),
        &["cloudflare-workers-ai/@cf/moonshotai/kimi-k2.6"],
        &[],
    );

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args(["agent", "set", "--provider", "cloudflare-workers-ai"])
        .assert()
        .success()
        .stdout(predicates::str::contains("available model values:"))
        .stdout(predicates::str::contains(
            "cloudflare-workers-ai/@cf/moonshotai/kimi-k2.6",
        ));

    let config =
        fs::read_to_string(config_dir.join("acp-stack.toml")).expect("config should be readable");
    assert!(!config.contains("[agent.provider]"));
}

#[test]
fn agent_set_does_not_partially_write_main_config_when_provisioning_fails() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acp-stack.toml"), VALID_CONFIG).expect("config should be written");
    let options_path = write_acp_config_options(tempdir.path(), &["openai/gpt-5.5"], &[]);
    let opencode_dir = tempdir.path().join(".config").join("opencode");
    fs::create_dir_all(&opencode_dir).expect("opencode config dir should be created");
    fs::write(opencode_dir.join("opencode.json"), "[]")
        .expect("invalid opencode config should be written");

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args([
            "agent",
            "set",
            "--provider",
            "openai",
            "--model",
            "openai/gpt-5.5",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "existing JSON root must be an object",
        ));

    let config =
        fs::read_to_string(config_dir.join("acp-stack.toml")).expect("config should be readable");
    assert!(!config.contains("[agent.provider]"));
    assert!(!config.contains(r#""OPENAI_API_KEY""#));
}

#[test]
fn agent_set_validates_model_against_acp_config_options() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acp-stack.toml"), VALID_CONFIG).expect("config should be written");
    let options_path = write_acp_config_options(tempdir.path(), &["openai/gpt-5.5"], &[]);

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args([
            "agent",
            "set",
            "--provider",
            "openai",
            "--model",
            "openai/not-advertised",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "agent did not advertise `openai/not-advertised` as an available `model`",
        ));

    let config =
        fs::read_to_string(config_dir.join("acp-stack.toml")).expect("config should be readable");
    assert!(!config.contains("[agent.provider]"));
}

#[test]
fn agent_set_amp_rejects_mode_until_acp_advertises_it() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = VALID_CONFIG
        .replace(r#"id = "opencode""#, r#"id = "amp""#)
        .replace(r#"name = "OpenCode""#, r#"name = "Amp Code""#)
        .replace(r#"command = "opencode""#, r#"command = "amp-acp""#)
        .replace(r#"args = ["acp"]"#, r#"args = []"#)
        .replace(r#"env = ["OPENCODE_API_KEY"]"#, r#"env = ["AMP_API_KEY"]"#)
        .replace(
            r#"
[agent.install]
type = "shell"
shell = "curl -fsSL https://opencode.ai/install | bash"
creates = "opencode"
"#,
            "",
        );
    fs::write(config_dir.join("acp-stack.toml"), config).expect("config should be written");
    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .args(["agent", "set", "--mode", "smart"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "Amp Code does not support mode configuration",
        ));

    let config =
        fs::read_to_string(config_dir.join("acp-stack.toml")).expect("config should be readable");
    assert!(!config.contains(r#"mode = "smart""#));
    assert!(!config.contains("[agent.provider]"));
}

#[test]
fn agent_set_opencode_accepts_mode_only() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acp-stack.toml"), VALID_CONFIG).expect("config should be written");
    let options_path = write_acp_config_options(tempdir.path(), &[], &["build", "plan"]);

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args(["agent", "set", "--mode", "plan"])
        .assert()
        .success()
        .stdout(predicates::str::contains("mode: plan"));

    let config =
        fs::read_to_string(config_dir.join("acp-stack.toml")).expect("config should be readable");
    assert!(config.contains(r#"mode = "plan""#));
    assert!(!config.contains("[agent.provider]"));
}

#[test]
fn agent_set_cursor_accepts_mode_only() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = VALID_CONFIG
        .replace(r#"id = "opencode""#, r#"id = "cursor""#)
        .replace(r#"name = "OpenCode""#, r#"name = "Cursor CLI""#)
        .replace(r#"command = "opencode""#, r#"command = "cursor-agent""#)
        .replace(
            r#"env = ["OPENCODE_API_KEY"]"#,
            r#"env = ["CURSOR_API_KEY"]"#,
        )
        .replace(
            r#"
[agent.install]
type = "shell"
shell = "curl -fsSL https://opencode.ai/install | bash"
creates = "opencode"
"#,
            "",
        );
    fs::write(config_dir.join("acp-stack.toml"), config).expect("config should be written");
    let options_path = write_acp_config_options(tempdir.path(), &[], &["agent", "ask", "plan"]);

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args(["agent", "set", "--mode", "plan"])
        .assert()
        .success()
        .stdout(predicates::str::contains("mode: plan"));

    let config =
        fs::read_to_string(config_dir.join("acp-stack.toml")).expect("config should be readable");
    assert!(config.contains(r#"mode = "plan""#));
    assert!(!config.contains("[agent.provider]"));
}

#[test]
fn agent_set_pi_rejects_mode() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = VALID_CONFIG
        .replace(r#"id = "opencode""#, r#"id = "pi""#)
        .replace(r#"name = "OpenCode""#, r#"name = "Pi Agent""#)
        .replace(r#"command = "opencode""#, r#"command = "pi-acp""#)
        .replace(r#"args = ["acp"]"#, r#"args = []"#)
        .replace(
            r#"
[agent.install]
type = "shell"
shell = "curl -fsSL https://opencode.ai/install | bash"
creates = "opencode"
"#,
            "",
        );
    fs::write(config_dir.join("acp-stack.toml"), config).expect("config should be written");

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .args(["agent", "set", "--mode", "plan"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "Pi Agent does not support mode configuration",
        ));
}

#[test]
fn agent_set_amp_rejects_provider_model_settings() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = VALID_CONFIG
        .replace(r#"id = "opencode""#, r#"id = "amp""#)
        .replace(r#"name = "OpenCode""#, r#"name = "Amp Code""#)
        .replace(r#"command = "opencode""#, r#"command = "amp-acp""#)
        .replace(r#"args = ["acp"]"#, r#"args = []"#)
        .replace(r#"env = ["OPENCODE_API_KEY"]"#, r#"env = ["AMP_API_KEY"]"#)
        .replace(
            r#"
[agent.install]
type = "shell"
shell = "curl -fsSL https://opencode.ai/install | bash"
creates = "opencode"
"#,
            "",
        );
    fs::write(config_dir.join("acp-stack.toml"), config).expect("config should be written");

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .args([
            "agent",
            "set",
            "--provider",
            "openai",
            "--model",
            "openai/gpt-5.5",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "Amp Code does not support provider configuration",
        ));
}

#[test]
fn agent_install_registry_path_does_not_require_runtime_secret_store() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let workspace_root = tempdir.path().join("workspace");
    fs::create_dir(&workspace_root).expect("workspace dir should be created");
    let binary_path = tempdir
        .path()
        .join(".local")
        .join("bin")
        .join("cli-registry-agent");
    let script = format!(
        "mkdir -p {bin} && printf registry > {binary} && chmod 755 {binary}",
        bin = shell_quote_path(binary_path.parent().expect("binary has parent")),
        binary = shell_quote_path(&binary_path),
    );
    let config = VALID_CONFIG
        .replace(
            r#"command = "opencode""#,
            r#"command = "cli-registry-agent""#,
        )
        .replace(
            r#"root = "/workspace""#,
            &format!(r#"root = "{}""#, workspace_root.display()),
        )
        .replace(
            r#"uploads = "/workspace/uploads""#,
            &format!(r#"uploads = "{}/uploads""#, workspace_root.display()),
        )
        .replace(r#"args = ["acp"]"#, "args = []")
        .replace(
            r#"
[agent.install]
type = "shell"
shell = "curl -fsSL https://opencode.ai/install | bash"
creates = "opencode"
"#,
            "",
        );
    fs::write(config_dir.join("acp-stack.toml"), config).expect("config should be written");
    fs::write(
        config_dir.join("agents.toml"),
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
creates = "cli-registry-agent"
"#
        ),
    )
    .expect("registry should be written");

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .args(["agent", "install"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent install: installed"))
        .stdout(predicates::str::contains(
            binary_path.to_string_lossy().as_ref(),
        ));
}

#[cfg(unix)]
#[test]
fn init_creates_owner_only_config_and_state_paths() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .arg("init")
        .assert()
        .success();

    let config_dir = tempdir.path().join(".config/acp-stack");
    let state_dir = tempdir.path().join(".local/share/acp-stack");
    let config_path = config_dir.join("acp-stack.toml");
    let state_path = state_dir.join("state.sqlite");

    assert_eq!(mode(&config_dir), 0o700);
    assert_eq!(mode(&state_dir), 0o700);
    assert_eq!(mode(&config_path), 0o600);
    assert_eq!(mode(&state_path), 0o600);
}

#[test]
fn init_does_not_overwrite_existing_config() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config_path = config_dir.join("acp-stack.toml");
    fs::write(&config_path, VALID_CONFIG).expect("config should be written");

    let mut command = Command::cargo_bin("acps").expect("binary should build");

    command
        .env("HOME", tempdir.path())
        .arg("init")
        .assert()
        .success()
        .stdout(predicates::str::contains("validated existing config"));

    let config = fs::read_to_string(config_path).expect("config should be readable");
    assert_eq!(config, VALID_CONFIG);
}

#[test]
fn init_fails_when_existing_config_is_invalid() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(
        config_dir.join("acp-stack.toml"),
        VALID_CONFIG.replace(r#"bind = "127.0.0.1:7700""#, r#"bind = "bad""#),
    )
    .expect("invalid config should be written");

    let mut command = Command::cargo_bin("acps").expect("binary should build");

    command
        .env("HOME", tempdir.path())
        .arg("init")
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "api.bind must be a socket address",
        ));
}

#[test]
fn status_reports_config_state_schema_and_latest_event() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .arg("init")
        .assert()
        .success();

    let mut command = Command::cargo_bin("acps").expect("binary should build");
    command
        .env("HOME", tempdir.path())
        .arg("status")
        .assert()
        .success()
        .stdout(predicates::str::contains("config: ok"))
        .stdout(predicates::str::contains("state: ok"))
        .stdout(predicates::str::contains("schema_version: 9"))
        .stdout(predicates::str::contains("latest_event:"));
}

#[tokio::test(flavor = "multi_thread")]
async fn agent_start_and_stop_call_running_daemon() {
    let harness = AgentCliHarness::spawn().await;
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home(home.path(), &harness.base_url, ADMIN_KEY);

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", home.path())
        .args(["agent", "start"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent start: running"))
        .stdout(predicates::str::contains("pid: "));

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", home.path())
        .args(["agent", "stop"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent stop: stopped"));
}

#[tokio::test(flavor = "multi_thread")]
async fn security_check_calls_running_daemon_with_admin_key() {
    let harness = AgentCliHarness::spawn().await;
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home(home.path(), &harness.base_url, ADMIN_KEY);

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", home.path())
        .args(["security", "check"])
        .assert()
        .success()
        .stdout(predicates::str::contains("ok: "))
        .stdout(predicates::str::contains("auth_failures_total:"))
        .stdout(predicates::str::contains("findings:"));
}

#[tokio::test(flavor = "multi_thread")]
async fn security_check_renders_hint_line_for_each_finding() {
    // Drive a finding by reporting an unspecified-address effective_bind; the
    // self-check turns that into `api.public_bind` (warning). The CLI must
    // render the diagnostic line AND an indented `hint:` line with the
    // remediation prose.
    let harness = AgentCliHarness::spawn_with_effective_bind("0.0.0.0:7700").await;
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home(home.path(), &harness.base_url, ADMIN_KEY);

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", home.path())
        .args(["security", "check"])
        .assert()
        .success()
        .stdout(predicates::str::contains("api.public_bind"))
        .stdout(predicates::str::contains("    hint: "))
        .stdout(
            predicates::str::contains("loopback").or(predicates::str::contains("reverse proxy")),
        );
}

#[tokio::test(flavor = "multi_thread")]
async fn security_check_uses_admin_key_not_session_key() {
    let harness = AgentCliHarness::spawn().await;
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home(home.path(), &harness.base_url, SESSION_KEY);

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", home.path())
        .args(["security", "check"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("/v1/security/check"))
        .stderr(predicates::str::contains("401"));
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_new_list_prompt_close_round_trip() {
    let harness = AgentCliHarness::spawn().await;
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home(home.path(), &harness.base_url, ADMIN_KEY);

    // Start the agent first so /v1/sessions has a live ACP connection.
    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", home.path())
        .args(["agent", "start"])
        .assert()
        .success();

    let new_output = Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", home.path())
        .args(["sessions", "new"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(new_output).expect("utf8");
    let session_id = stdout
        .lines()
        .find_map(|line| line.strip_prefix("session: "))
        .expect("session: <id> line")
        .trim()
        .to_owned();

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", home.path())
        .args(["sessions", "list"])
        .assert()
        .success()
        .stdout(predicates::str::contains(session_id.as_str()));

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", home.path())
        .args(["sessions", "prompt", &session_id, "hello"])
        .timeout(std::time::Duration::from_secs(30))
        .assert()
        .success()
        .stdout(predicates::str::contains("prompt: completed"))
        .stdout(predicates::str::contains("stop_reason: end_turn"));

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", home.path())
        .args(["sessions", "close", &session_id])
        .assert()
        .success()
        .stdout(predicates::str::contains("session close: closed"));
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_prompt_no_wait_returns_immediately() {
    let harness = AgentCliHarness::spawn().await;
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home(home.path(), &harness.base_url, ADMIN_KEY);

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", home.path())
        .args(["agent", "start"])
        .assert()
        .success();

    let new_output = Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", home.path())
        .args(["sessions", "new"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(new_output).expect("utf8");
    let session_id = stdout
        .lines()
        .find_map(|line| line.strip_prefix("session: "))
        .expect("session: <id> line")
        .trim()
        .to_owned();

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", home.path())
        .args(["sessions", "prompt", &session_id, "ping", "--no-wait"])
        .assert()
        .success()
        .stdout(predicates::str::contains("prompt: pending"))
        .stdout(predicates::str::contains("prompt_id: "));
}

#[tokio::test(flavor = "multi_thread")]
async fn agent_start_reports_daemon_auth_failure() {
    let harness = AgentCliHarness::spawn().await;
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home(
        home.path(),
        &harness.base_url,
        "acps_admin_wrongwrongwrongwrongwrongwrongwrongwrongwrong",
    );

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", home.path())
        .args(["agent", "start"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "agent API request to /v1/agent/start failed with status 401",
        ));
}

#[cfg(unix)]
#[test]
fn status_creates_owner_only_state_when_config_exists_without_state() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config_path = config_dir.join("acp-stack.toml");
    fs::write(&config_path, VALID_CONFIG).expect("config should be written");
    fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o755))
        .expect("config dir permissions should be set");
    fs::set_permissions(&config_path, fs::Permissions::from_mode(0o644))
        .expect("config file permissions should be set");

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .arg("status")
        .assert()
        .success();

    let state_dir = tempdir.path().join(".local/share/acp-stack");
    let state_path = state_dir.join("state.sqlite");
    assert_eq!(mode(&config_dir), 0o700);
    assert_eq!(mode(&config_path), 0o600);
    assert_eq!(mode(&state_dir), 0o700);
    assert_eq!(mode(&state_path), 0o600);
}

#[cfg(unix)]
#[test]
fn status_repairs_config_permissions_before_validation_failure() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config_path = config_dir.join("acp-stack.toml");
    fs::write(
        &config_path,
        VALID_CONFIG.replace(r#"bind = "127.0.0.1:7700""#, r#"bind = "bad""#),
    )
    .expect("invalid config should be written");
    fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o755))
        .expect("config dir permissions should be set");
    fs::set_permissions(&config_path, fs::Permissions::from_mode(0o644))
        .expect("config file permissions should be set");

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .arg("status")
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "api.bind must be a socket address",
        ));

    assert_eq!(mode(&config_dir), 0o700);
    assert_eq!(mode(&config_path), 0o600);
}

#[test]
fn logs_query_shows_init_event() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .arg("init")
        .assert()
        .success();

    let mut command = Command::cargo_bin("acps").expect("binary should build");
    command
        .env("HOME", tempdir.path())
        .args(["logs", "query"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "info cli init.completed initialized",
        ));
}

#[cfg(unix)]
#[test]
fn logs_query_creates_owner_only_empty_state_when_missing() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .args(["logs", "query"])
        .assert()
        .success()
        .stdout("");

    let state_dir = tempdir.path().join(".local/share/acp-stack");
    let state_path = state_dir.join("state.sqlite");
    assert_eq!(mode(&state_dir), 0o700);
    assert_eq!(mode(&state_path), 0o600);
}

#[test]
fn logs_query_supports_limit_and_level_filter() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .arg("init")
        .assert()
        .success();
    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .arg("status")
        .assert()
        .success();

    let mut limit_command = Command::cargo_bin("acps").expect("binary should build");
    limit_command
        .env("HOME", tempdir.path())
        .args(["logs", "query", "--limit", "1"])
        .assert()
        .success()
        .stdout(predicates::str::contains("status.checked").count(1));

    let mut level_command = Command::cargo_bin("acps").expect("binary should build");
    level_command
        .env("HOME", tempdir.path())
        .args(["logs", "query", "--level", "error"])
        .assert()
        .success()
        .stdout("");
}

#[test]
fn failed_cli_command_records_error_after_state_exists() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .arg("init")
        .assert()
        .success();

    let config_path = tempdir.path().join(".config/acp-stack/acp-stack.toml");
    fs::write(
        config_path,
        VALID_CONFIG.replace(r#"bind = "127.0.0.1:7700""#, r#"bind = "bad""#),
    )
    .expect("invalid config should be written");

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .arg("status")
        .assert()
        .failure();

    let mut logs_command = Command::cargo_bin("acps").expect("binary should build");
    logs_command
        .env("HOME", tempdir.path())
        .args(["logs", "query", "--level", "error"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "error cli cli.error command failed",
        ));
}

#[test]
fn parse_failure_records_error_after_state_exists() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .arg("init")
        .assert()
        .success();

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .arg("unknown-command")
        .assert()
        .failure();

    let mut logs_command = Command::cargo_bin("acps").expect("binary should build");
    logs_command
        .env("HOME", tempdir.path())
        .args(["logs", "query", "--level", "error"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "error cli cli.error command failed",
        ));
}

#[test]
fn help_invocations_do_not_record_error_events() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .arg("init")
        .assert()
        .success();

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .arg("--help")
        .assert()
        .success();

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .arg("--version")
        .assert()
        .success();

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .args(["logs", "--help"])
        .assert()
        .success();

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .args(["logs", "query", "--help"])
        .assert()
        .success();

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .args(["logs", "query", "--level", "error"])
        .assert()
        .success()
        .stdout("");
}

#[cfg(unix)]
#[test]
fn cli_error_payload_handles_control_bytes_in_argument() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .arg("init")
        .assert()
        .success();

    // Path that mixes a stray ANSI escape sequence and a bare control byte. The runtime
    // must strip ANSI, encode the remaining bytes via serde_json, and still produce a
    // valid JSON payload that survives json_valid() in SQLite.
    let bad_path = OsString::from_vec(b"/tmp/acp\x1b[31m-missing\x07\x08-file.toml".to_vec());

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .args(["config", "validate"])
        .arg(&bad_path)
        .assert()
        .failure();

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .args(["logs", "query", "--level", "error"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "error cli cli.error command failed",
        ));
}

#[test]
fn empty_home_is_treated_as_unset() {
    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", "")
        .arg("init")
        .assert()
        .failure()
        .stderr(predicates::str::contains("HOME is not set"));
}

#[cfg(unix)]
#[test]
fn init_repairs_config_permissions_before_validation_failure() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config_path = config_dir.join("acp-stack.toml");
    fs::write(
        &config_path,
        VALID_CONFIG.replace(r#"bind = "127.0.0.1:7700""#, r#"bind = "bad""#),
    )
    .expect("invalid config should be written");
    fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o755))
        .expect("config dir perms should set");
    fs::set_permissions(&config_path, fs::Permissions::from_mode(0o644))
        .expect("config file perms should set");

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .arg("init")
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "api.bind must be a socket address",
        ));

    assert_eq!(mode(&config_dir), 0o700);
    assert_eq!(mode(&config_path), 0o600);
}

#[cfg(unix)]
#[test]
fn init_repairs_existing_permissive_state_file() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let state_dir = tempdir.path().join(".local/share/acp-stack");
    fs::create_dir_all(&state_dir).expect("state dir should be created");
    let state_path = state_dir.join("state.sqlite");
    fs::write(&state_path, b"").expect("placeholder state file should be written");
    fs::set_permissions(&state_path, fs::Permissions::from_mode(0o644))
        .expect("permissive perms should set");
    assert_eq!(mode(&state_path), 0o644);

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .arg("init")
        .assert()
        .success();

    assert_eq!(mode(&state_path), 0o600);
}

#[cfg(unix)]
#[test]
fn status_repairs_existing_permissive_state_file() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acp-stack.toml"), VALID_CONFIG)
        .expect("valid config should be written");

    let state_dir = tempdir.path().join(".local/share/acp-stack");
    fs::create_dir_all(&state_dir).expect("state dir should be created");
    let state_path = state_dir.join("state.sqlite");
    fs::write(&state_path, b"").expect("placeholder state file should be written");
    fs::set_permissions(&state_path, fs::Permissions::from_mode(0o644))
        .expect("permissive perms should set");

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .arg("status")
        .assert()
        .success();

    assert_eq!(mode(&state_path), 0o600);
}

#[cfg(unix)]
#[test]
fn logs_query_repairs_existing_permissive_state_file() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let state_dir = tempdir.path().join(".local/share/acp-stack");
    fs::create_dir_all(&state_dir).expect("state dir should be created");
    let state_path = state_dir.join("state.sqlite");
    fs::write(&state_path, b"").expect("placeholder state file should be written");
    fs::set_permissions(&state_path, fs::Permissions::from_mode(0o644))
        .expect("permissive perms should set");

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .args(["logs", "query"])
        .assert()
        .success();

    assert_eq!(mode(&state_path), 0o600);
}

#[cfg(unix)]
#[test]
fn error_recording_path_repairs_permissive_state_file() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .arg("init")
        .assert()
        .success();

    let state_path = tempdir.path().join(".local/share/acp-stack/state.sqlite");
    fs::set_permissions(&state_path, fs::Permissions::from_mode(0o644))
        .expect("permissive perms should set");
    assert_eq!(mode(&state_path), 0o644);

    // Corrupt the config so the next invocation fails through the error-recording path.
    let config_path = tempdir.path().join(".config/acp-stack/acp-stack.toml");
    fs::write(
        &config_path,
        VALID_CONFIG.replace(r#"bind = "127.0.0.1:7700""#, r#"bind = "bad""#),
    )
    .expect("invalid config should be written");

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .arg("status")
        .assert()
        .failure();

    assert_eq!(
        mode(&state_path),
        0o600,
        "record_cli_error_message must repair permissive perms before writing the error row",
    );

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .args(["logs", "query", "--level", "error"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "error cli cli.error command failed",
        ));
}

#[cfg(unix)]
fn mode(path: &std::path::Path) -> u32 {
    fs::metadata(path)
        .expect("metadata should be readable")
        .permissions()
        .mode()
        & 0o777
}

fn shell_quote_path(path: &std::path::Path) -> String {
    let text = path.to_string_lossy();
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn write_acp_config_options(
    root: &std::path::Path,
    models: &[&str],
    modes: &[&str],
) -> std::path::PathBuf {
    let options_path = root.join("acp-config-options.json");
    let mut options = Vec::new();
    if !models.is_empty() {
        options.push(serde_json::json!({
            "id": "model",
            "name": "Model",
            "category": "model",
            "type": "select",
            "currentValue": models[0],
            "options": models
                .iter()
                .map(|value| serde_json::json!({ "value": value, "name": value }))
                .collect::<Vec<_>>()
        }));
    }
    if !modes.is_empty() {
        options.push(serde_json::json!({
            "id": "mode",
            "name": "Mode",
            "category": "mode",
            "type": "select",
            "currentValue": modes[0],
            "options": modes
                .iter()
                .map(|value| serde_json::json!({ "value": value, "name": value }))
                .collect::<Vec<_>>()
        }));
    }
    fs::write(
        &options_path,
        serde_json::to_string(&options).expect("options serialize"),
    )
    .expect("options fixture should be written");
    options_path
}

// ----- 0.0.1 auth/secrets/reset/config-import tests -----

fn run_init_with_home(home: &std::path::Path) {
    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", home)
        .arg("init")
        .assert()
        .success();
}

#[test]
fn init_agent_flag_updates_config_non_interactively() {
    let tempdir = tempfile::tempdir().expect("tempdir");

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .args(["init", "--agent", "cursor", "--no-install-agent"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent: Cursor CLI (cursor)"));

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acp-stack.toml"))
        .expect("config readable");
    assert!(written.contains(r#"id = "cursor""#));
    assert!(written.contains(r#"command = "cursor-agent""#));
    assert!(written.contains(r#"args = ["acp"]"#));
    assert!(written.contains(r#"env = ["CURSOR_API_KEY"]"#));
    assert!(!written.contains("[agent.install]"));
}

#[test]
fn init_install_agent_runs_selected_registry_install() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir");
    let workspace_root = tempdir.path().join("workspace");
    fs::create_dir(&workspace_root).expect("workspace dir");
    let managed_binary = tempdir.path().join(".local/bin/init-test-agent");
    let config = VALID_CONFIG
        .replace(
            r#"root = "/workspace""#,
            &format!(r#"root = "{}""#, workspace_root.display()),
        )
        .replace(
            r#"uploads = "/workspace/uploads""#,
            &format!(r#"uploads = "{}/uploads""#, workspace_root.display()),
        );
    fs::write(config_dir.join("acp-stack.toml"), config).expect("config");
    let script = format!(
        "mkdir -p {bin} && printf init > {binary} && chmod 755 {binary}",
        bin = shell_quote_path(managed_binary.parent().expect("binary has parent")),
        binary = shell_quote_path(&managed_binary),
    );
    fs::write(
        config_dir.join("agents.toml"),
        format!(
            r#"
[[agents]]
id = "init-test"
name = "Init Test"
kind = "native"
headless_compatible = true
support_doc = "docs/agents/init-test.md"

[agents.harness]
id = "init-test-agent"

[agents.harness.install.shell]
script = {script:?}
creates = "init-test-agent"
"#
        ),
    )
    .expect("agents override");

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .args(["init", "--agent", "init-test", "--install-agent"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent install: installed"));

    assert!(managed_binary.is_file());
    let written = fs::read_to_string(config_dir.join("acp-stack.toml")).expect("config readable");
    assert!(written.contains(r#"id = "init-test""#));
    assert!(written.contains(r#"command = "init-test-agent""#));
}

#[test]
fn init_creates_age_key_and_encrypted_secret_store() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    let age_key = tempdir.path().join(".config/acp-stack/age.key");
    let store = tempdir.path().join(".local/share/acp-stack/secrets.age");
    assert!(age_key.is_file(), "age key must be written");
    assert!(store.is_file(), "secret store ciphertext must be written");
}

#[cfg(unix)]
#[test]
fn init_age_key_and_store_are_owner_only() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    assert_eq!(
        mode(&tempdir.path().join(".config/acp-stack/age.key")),
        0o600
    );
    assert_eq!(
        mode(&tempdir.path().join(".local/share/acp-stack/secrets.age")),
        0o600,
    );
}

#[test]
fn init_prints_session_and_admin_keys_on_first_run() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let output = Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .arg("init")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("utf8");
    assert!(stdout.contains("session key (ACP_STACK_SESSION_KEY): acps_"));
    assert!(stdout.contains("admin key (ACP_STACK_ADMIN_KEY): acps_"));
    assert!(stdout.contains("save the admin key now"));
}

#[test]
fn init_is_idempotent_and_preserves_keys() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    let store = tempdir.path().join(".local/share/acp-stack/secrets.age");
    let first = fs::read(&store).expect("ciphertext readable");

    let stdout = Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .arg("init")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(stdout).expect("utf8");
    assert!(
        stdout.contains("preserved existing API keys"),
        "second init must report preservation, got: {stdout}",
    );
    assert!(
        !stdout.contains("save the admin key now"),
        "second init must not print key material again",
    );

    let second = fs::read(&store).expect("ciphertext readable");
    assert_eq!(
        first, second,
        "ciphertext is rewritten on init even with no changes; investigate",
    );
}

#[test]
fn init_fails_fast_when_store_exists_with_both_auth_refs_missing() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    // Delete both auth refs via the library API (the CLI guard rejects this,
    // which is itself a separate test) and add an unrelated secret so the
    // store is non-empty. The new init logic must refuse to silently
    // re-generate the admin key in this corrupted state.
    let mut store = acp_stack::secrets::SecretStore::open(tempdir.path()).expect("open store");
    store.set("OPENCODE_API_KEY", "xyz").expect("set unrelated");
    store
        .delete("ACP_STACK_SESSION_KEY")
        .expect("delete session");
    store.delete("ACP_STACK_ADMIN_KEY").expect("delete admin");
    drop(store);

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .arg("init")
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "does not contain the admin key reference",
        ));
}

#[test]
fn secrets_set_only_captures_first_line_of_stdin() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .args(["secrets", "set", "MULTILINE_TEST"])
        .write_stdin("first-line\nsecond-line\n")
        .assert()
        .success();

    let store = acp_stack::secrets::SecretStore::open(tempdir.path()).expect("open store");
    assert_eq!(store.get("MULTILINE_TEST").expect("get"), "first-line");
}

#[test]
fn init_fails_fast_when_admin_key_missing_in_existing_store() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    // Delete the admin key via the library API (the CLI guard refuses to do
    // so directly, which is a separate test). The store now contains the
    // session key only, mimicking a partial wipe.
    let mut store = acp_stack::secrets::SecretStore::open(tempdir.path()).expect("open store");
    store.delete("ACP_STACK_ADMIN_KEY").expect("delete admin");
    drop(store);

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .arg("init")
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "does not contain the admin key reference",
        ));
}

#[test]
fn secrets_set_refuses_to_mutate_session_key_ref() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .args(["secrets", "set", "ACP_STACK_SESSION_KEY"])
        .write_stdin("attacker-supplied")
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "is the configured session key reference",
        ));
}

#[test]
fn secrets_set_refuses_to_mutate_admin_key_ref() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .args(["secrets", "set", "ACP_STACK_ADMIN_KEY"])
        .write_stdin("attacker-supplied")
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "is the configured admin key reference",
        ));
}

#[test]
fn secrets_delete_refuses_to_remove_admin_key_ref() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .args(["secrets", "delete", "ACP_STACK_ADMIN_KEY"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "is the configured admin key reference",
        ));
}

#[test]
fn secrets_list_shows_session_and_admin_names_only_after_init() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .args(["secrets", "list"])
        .assert()
        .success()
        .stdout(predicates::str::contains("ACP_STACK_ADMIN_KEY"))
        .stdout(predicates::str::contains("ACP_STACK_SESSION_KEY"))
        .stdout(predicates::str::contains("acps_").not());
}

#[test]
fn secrets_set_reads_value_from_stdin() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .args(["secrets", "set", "OPENCODE_API_KEY"])
        .write_stdin("super-secret-value\n")
        .assert()
        .success()
        .stdout(predicates::str::contains("set secret: OPENCODE_API_KEY"));

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .args(["secrets", "list"])
        .assert()
        .success()
        .stdout(predicates::str::contains("OPENCODE_API_KEY"));
}

#[test]
fn secrets_delete_removes_named_secret() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .args(["secrets", "set", "TEMP_VALUE"])
        .write_stdin("abc")
        .assert()
        .success();

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .args(["secrets", "delete", "TEMP_VALUE"])
        .assert()
        .success()
        .stdout(predicates::str::contains("deleted secret: TEMP_VALUE"));

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .args(["secrets", "delete", "TEMP_VALUE"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("was not found"));
}

#[test]
fn auth_regenerate_session_key_rotates_only_session() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let first_init_stdout = Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .arg("init")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let first_init = String::from_utf8(first_init_stdout).expect("utf8");
    let admin_line = first_init
        .lines()
        .find(|l| l.starts_with("admin key (ACP_STACK_ADMIN_KEY): "))
        .expect("init must print admin key");
    let admin_value_before = admin_line
        .trim_start_matches("admin key (ACP_STACK_ADMIN_KEY): ")
        .trim();
    let session_line = first_init
        .lines()
        .find(|l| l.starts_with("session key (ACP_STACK_SESSION_KEY): "))
        .expect("init must print session key");
    let session_before = session_line
        .trim_start_matches("session key (ACP_STACK_SESSION_KEY): ")
        .trim();

    let rotate_stdout = Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .args(["auth", "regenerate-session-key"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let rotate = String::from_utf8(rotate_stdout).expect("utf8");
    assert!(rotate.contains("session key rotated"));
    let rotated_line = rotate
        .lines()
        .find(|l| l.starts_with("value: "))
        .expect("rotate must print new value");
    let session_after = rotated_line.trim_start_matches("value: ").trim();

    assert_ne!(
        session_before, session_after,
        "session key must change on rotation",
    );

    // Read the admin key via the store layer to confirm it wasn't touched.
    let store = acp_stack::secrets::SecretStore::open(tempdir.path()).expect("open store");
    assert_eq!(
        store
            .get("ACP_STACK_ADMIN_KEY")
            .expect("admin key still present"),
        admin_value_before,
        "admin key must NOT change on session rotation",
    );
}

#[test]
fn reset_without_yes_lists_targets_and_keeps_files() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .arg("reset")
        .assert()
        .failure()
        .stdout(predicates::str::contains("acps reset would delete:"))
        .stdout(predicates::str::contains("acp-stack.toml"))
        .stdout(predicates::str::contains("state.sqlite"))
        .stdout(predicates::str::contains("age.key"))
        .stdout(predicates::str::contains("secrets.age"))
        .stdout(predicates::str::contains("re-run with --yes"));

    assert!(
        tempdir
            .path()
            .join(".config/acp-stack/acp-stack.toml")
            .exists(),
        "dry-run must NOT remove files",
    );
}

#[test]
fn reset_dry_run_does_not_write_cli_error_event() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .arg("reset")
        .assert()
        .failure();

    // The dry-run contract is "exits without touching the filesystem".
    // Recording a `cli.error` event row would touch state.sqlite, so the
    // event log must show no error rows after a dry-run reset.
    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .args(["logs", "query", "--level", "error"])
        .assert()
        .success()
        .stdout("");
}

#[test]
fn reset_with_yes_wipes_config_state_age_key_and_secret_store() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .args(["reset", "--yes"])
        .assert()
        .success()
        .stdout(predicates::str::contains("reset acp-stack"));

    assert!(
        !tempdir
            .path()
            .join(".config/acp-stack/acp-stack.toml")
            .exists()
    );
    assert!(!tempdir.path().join(".config/acp-stack/age.key").exists());
    assert!(
        !tempdir
            .path()
            .join(".local/share/acp-stack/state.sqlite")
            .exists()
    );
    assert!(
        !tempdir
            .path()
            .join(".local/share/acp-stack/secrets.age")
            .exists()
    );

    // Re-running reset is idempotent and does not error on missing files.
    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .args(["reset", "--yes"])
        .assert()
        .success();

    // Fresh init after reset produces a different admin key than the first.
    let init_after = Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .arg("init")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(init_after).expect("utf8");
    assert!(stdout.contains("admin key (ACP_STACK_ADMIN_KEY): acps_"));
}

#[test]
fn config_import_refuses_without_force_when_config_exists() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    let exported = Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .args(["config", "export"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let import_path = tempdir.path().join("exported.toml");
    fs::write(&import_path, exported).expect("write export");

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .args(["config", "import", import_path.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicates::str::contains("config already exists"));
}

#[test]
fn config_import_with_force_replaces_existing_config() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    // Build an alternate config with a recognizable bind addr.
    let modified = VALID_CONFIG.replace(r#"bind = "127.0.0.1:7700""#, r#"bind = "127.0.0.1:7777""#);
    let import_path = tempdir.path().join("alt.toml");
    fs::write(&import_path, &modified).expect("write alt");

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .args(["config", "import", import_path.to_str().unwrap(), "--force"])
        .assert()
        .success()
        .stdout(predicates::str::contains("imported config (replaced)"));

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acp-stack.toml"))
        .expect("config readable");
    assert!(written.contains("127.0.0.1:7777"));
}

#[test]
fn config_import_supports_base64_input() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    // No init first; we want to exercise the create-fresh import path.
    let modified = VALID_CONFIG.replace(r#"bind = "127.0.0.1:7700""#, r#"bind = "127.0.0.1:7788""#);
    let encoded = base64::engine::general_purpose::STANDARD.encode(modified);

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .args(["config", "import", "--base64", &encoded])
        .assert()
        .success()
        .stdout(predicates::str::contains("imported config:"));

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acp-stack.toml"))
        .expect("config readable");
    assert!(written.contains("127.0.0.1:7788"));
}

#[test]
fn config_import_refuses_to_change_auth_refs() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    // Build an alternate config that changes admin_key_ref. Import must
    // refuse, otherwise an operator could swap which secret is treated as
    // the admin key without going through `acps reset --yes`.
    let modified = VALID_CONFIG.replace(
        r#"admin_key_ref = "ACP_STACK_ADMIN_KEY""#,
        r#"admin_key_ref = "MY_NEW_ADMIN""#,
    );
    let import_path = tempdir.path().join("rotated.toml");
    fs::write(&import_path, &modified).expect("write rotated");

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .args(["config", "import", import_path.to_str().unwrap(), "--force"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "would change `[auth].admin_key_ref`",
        ));
}

#[test]
fn config_import_refuses_to_change_session_ref() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    let modified = VALID_CONFIG.replace(
        r#"session_key_ref = "ACP_STACK_SESSION_KEY""#,
        r#"session_key_ref = "MY_NEW_SESSION""#,
    );
    let import_path = tempdir.path().join("rotated-session.toml");
    fs::write(&import_path, &modified).expect("write rotated session");

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .args(["config", "import", import_path.to_str().unwrap(), "--force"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "would change `[auth].session_key_ref`",
        ));
}

#[test]
fn config_import_rejects_invalid_base64() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .args(["config", "import", "--base64", "!!!not-base64!!!"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("not valid base64"));
}
