#![cfg(all(feature = "dev-tools", feature = "test-fixtures"))]

use acp_stack::api::{self, AppState, RuntimePaths};
use acp_stack::auth::{AuthVerifierSet, KeyKind};
use acp_stack::config::{
    AgentInstallConfig, DependencyInstallScope, McpServerConfig, StackUpdatePolicy,
    load_config_from_str,
};
use acp_stack::dev_gates::TEST_SKIP_AGENT_INSTALL_ENV;
use acp_stack::secrets::SecretStore;
use acp_stack::state::{
    EVENT_SOURCE_CLI, INSTALLER_METHOD_GITHUB, INSTALLER_METHOD_NPM, INSTALLER_OPERATION_INSTALL,
    InstallerRunInput, StateStore, default_state_path,
};
use assert_cmd::Command;
use axum::{Json, Router, routing::get};
use base64::Engine;
use http::StatusCode;
use predicates::prelude::PredicateBooleanExt as _;
use serde_json::{Value, json};
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use tempfile::TempDir;
use tokio::net::{TcpListener, UnixListener};
use tokio::task::JoinHandle;

const VALID_CONFIG: &str = include_str!("fixtures/valid-opencode-stack.toml");
const VALID_PLACEBO_CONFIG: &str = include_str!("fixtures/valid-placebo-stack.toml");
const SESSION_KEY: &str = "acps_session_cccccccccccccccccccccccccccccccccccccccccccc";
const ADMIN_KEY: &str = "acps_admin_dddddddddddddddddddddddddddddddddddddddddddd";

fn acps_command() -> Command {
    let mut command = Command::cargo_bin("acps").expect("binary should build");
    command.env(
        "ACP_STACK_DEV_PLACEBO_REGISTRY",
        env!("CARGO_BIN_EXE_placebo-agent"),
    );
    command.env(TEST_SKIP_AGENT_INSTALL_ENV, "1");
    command
}

fn acps_command_without_placebo() -> Command {
    Command::cargo_bin("acps").expect("binary should build")
}

struct AgentCliHarness {
    base_url: String,
    socket_path: std::path::PathBuf,
    config_path: std::path::PathBuf,
    state_path: std::path::PathBuf,
    join: JoinHandle<acp_stack::error::Result<()>>,
    local_join: JoinHandle<acp_stack::error::Result<()>>,
    _tempdir: TempDir,
}

struct HealthProbeHarness {
    socket_path: std::path::PathBuf,
    join: JoinHandle<std::io::Result<()>>,
    _tempdir: TempDir,
}

impl HealthProbeHarness {
    async fn spawn(status: StatusCode, body: Value) -> Self {
        let tempdir = TempDir::new().expect("tempdir");
        let socket_path = tempdir.path().join("probe.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind local probe");
        let app = Router::new().route(
            "/v1/health/ready",
            get(move || {
                let body = body.clone();
                async move { (status, Json(body)) }
            }),
        );
        let join = tokio::spawn(async move { axum::serve(listener, app).await });
        Self {
            socket_path,
            join,
            _tempdir: tempdir,
        }
    }
}

impl Drop for HealthProbeHarness {
    fn drop(&mut self) {
        self.join.abort();
    }
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
        let runtime_paths = RuntimePaths::new(config_path.clone(), path.clone());
        let mut config = load_config_from_str(VALID_PLACEBO_CONFIG).expect("config parses");
        let socket_path = tempdir.path().join("acp-stack").join("acps-local.sock");
        let workspace = tempdir.path().join("workspace");
        let uploads = workspace.join("uploads");
        fs::create_dir_all(&uploads).expect("workspace uploads should be created");
        config.workspace.root = workspace.to_string_lossy().into_owned();
        config.workspace.uploads = uploads.to_string_lossy().into_owned();
        config.agent.command = env!("CARGO_BIN_EXE_placebo-agent").to_owned();
        config.agent.args = vec!["acp".into()];
        config.agent.env = vec![];
        config.agent.cwd = Some(config.workspace.root.clone());
        config.agent.expected_sha256 = None;
        config.local.socket_path = Some(socket_path.to_string_lossy().into_owned());
        fs::write(
            &config_path,
            config.to_canonical_toml().expect("canonical test config"),
        )
        .expect("config should be written");
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
        let bound_local = acp_stack::local_listener::bind_local(
            &socket_path,
            acp_stack::local_listener::ParentPolicy::RepairOwnerOnly,
        )
        .await
        .expect("bind local listener");
        let local_join = tokio::spawn(acp_stack::local_listener::serve_local(
            app_state.clone(),
            bound_local,
        ));
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let base_url = format!("http://{}", listener.local_addr().expect("local"));
        let join = tokio::spawn(async move { api::serve(app_state, listener).await });
        Self {
            base_url,
            socket_path,
            config_path,
            state_path: path,
            join,
            local_join,
            _tempdir: tempdir,
        }
    }
}

impl Drop for AgentCliHarness {
    fn drop(&mut self) {
        self.join.abort();
        self.local_join.abort();
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
    let config_path = config_dir.join("acps-config.toml");
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
    write_cli_home_with_socket(home, base_url, admin_key, None);
}

fn write_cli_home_with_socket(
    home: &std::path::Path,
    base_url: &str,
    admin_key: &str,
    socket_path: Option<&std::path::Path>,
) {
    write_cli_home_with_socket_and_session_auth(home, base_url, admin_key, socket_path, None);
}

fn write_cli_home_with_socket_and_session_auth(
    home: &std::path::Path,
    base_url: &str,
    admin_key: &str,
    socket_path: Option<&std::path::Path>,
    session_auth: Option<&str>,
) {
    let config_dir = home.join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let mut config = VALID_CONFIG
        .replace(
            r#"public_url = "https://agent.example.com""#,
            &format!(r#"public_url = "{base_url}""#),
        )
        .replace(r#"env = ["OPENCODE_API_KEY"]"#, "env = []");
    if socket_path.is_some() || session_auth.is_some() {
        config.push_str("\n[local]\n");
        if let Some(socket_path) = socket_path {
            config.push_str(&format!(
                "socket_path = {:?}\n",
                socket_path.to_string_lossy()
            ));
        }
        if let Some(session_auth) = session_auth {
            config.push_str(&format!("session_auth = \"{session_auth}\"\n"));
        }
    }
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");
    seed_auth_verifiers(home, SESSION_KEY, admin_key);
}

fn seed_auth_verifiers(home: &std::path::Path, session_key: &str, admin_key: &str) {
    let state_path = default_state_path(home);
    fs::create_dir_all(state_path.parent().expect("state parent")).expect("state dir");
    let store = StateStore::open(&state_path).expect("state store should open");
    store.migrate().expect("state schema should migrate");
    let verifiers = AuthVerifierSet::create(session_key, admin_key);
    store
        .upsert_auth_key(KeyKind::Session, &verifiers.session)
        .expect("session auth verifier should be stored");
    store
        .upsert_auth_key(KeyKind::Admin, &verifiers.admin)
        .expect("admin auth verifier should be stored");
}

fn parse_key_line(stdout: &str, label: &'static str) -> String {
    stdout
        .lines()
        .find_map(|line| line.strip_prefix(label))
        .unwrap_or_else(|| panic!("missing {label} in stdout: {stdout}"))
        .trim()
        .to_owned()
}

fn parse_init_keys(stdout: &str) -> (String, String) {
    (
        parse_key_line(stdout, "session key: "),
        parse_key_line(stdout, "admin key: "),
    )
}

fn run_init_with_home(home: &std::path::Path) -> (String, String) {
    let stdout = acps_command()
        .env("HOME", home)
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(stdout).expect("init stdout utf8");
    parse_init_keys(&stdout)
}

fn seed_init_secrets(home: &std::path::Path, extra: &[(&str, &str)]) {
    seed_auth_verifiers(home, SESSION_KEY, ADMIN_KEY);
    let mut store = SecretStore::open_or_create(home).expect("secret store should open");
    store
        .set_many(extra.iter().copied())
        .expect("secrets should be stored");
}

fn write_fake_agent_home(home: &std::path::Path, fake_args: &[&str]) {
    let config_dir = home.join(".config/acp-stack");
    let workspace = home.join("workspace");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::create_dir_all(&workspace).expect("workspace should be created");
    let mut args = vec!["acp"];
    args.extend_from_slice(fake_args);
    let args_toml = args
        .iter()
        .map(|arg| toml_string(arg))
        .collect::<Vec<_>>()
        .join(", ");
    let config = VALID_PLACEBO_CONFIG
        .replace(
            r#"root = "/workspace""#,
            &format!(r#"root = "{}""#, workspace.display()),
        )
        .replace(
            r#"uploads = "/workspace/uploads""#,
            &format!(r#"uploads = "{}/uploads""#, workspace.display()),
        )
        .replace(
            r#"command = "placebo-agent""#,
            &format!(
                "command = {}",
                toml_string(env!("CARGO_BIN_EXE_placebo-agent"))
            ),
        )
        .replace(r#"args = ["acp"]"#, &format!("args = [{args_toml}]"))
        .replace(
            r#"cwd = "/workspace""#,
            &format!("cwd = {}", toml_string(&workspace.to_string_lossy())),
        );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");
}

fn toml_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

#[test]
fn prints_version() {
    let mut command = acps_command();

    command
        .arg("--version")
        .assert()
        .success()
        .stdout(predicates::str::contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn security_check_is_listed_in_help() {
    acps_command()
        .args(["security", "--help"])
        .assert()
        .success()
        .stdout(predicates::str::contains("check"))
        .stdout(predicates::str::contains("runtime security self-check"));
}

#[test]
fn top_level_help_describes_common_subcommands() {
    acps_command()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "Initialize local config, secrets, workspace, and agent files",
        ))
        .stdout(predicates::str::contains(
            "Print daemon health and runtime status",
        ))
        .stdout(predicates::str::contains(
            "Rotate or inspect configured API key references",
        ))
        .stdout(predicates::str::contains(
            "Manage encrypted local secret values",
        ))
        .stdout(predicates::str::contains(
            "Validate, export, or import runtime config",
        ))
        .stdout(predicates::str::contains("Query durable runtime logs"))
        .stdout(predicates::str::contains(
            "Install, control, test, or configure the agent",
        ))
        .stdout(predicates::str::contains(
            "Configure OpenCode small-model behavior",
        ))
        .stdout(predicates::str::contains(
            "List, create, prompt, or close sessions",
        ))
        .stdout(predicates::str::contains("Run development-only workflows"))
        .stdout(predicates::str::contains(
            "acps config import acps-config.toml --dry-run",
        ))
        .stdout(predicates::str::contains("config import --path").not());
}

#[test]
fn config_help_uses_positional_import_path() {
    acps_command()
        .args(["config", "--help"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "acps config import acps-config.toml --dry-run",
        ))
        .stdout(predicates::str::contains("config import --path").not());
}

#[test]
fn validates_explicit_config_path() {
    let mut command = acps_command();

    command
        .args([
            "config",
            "validate",
            "tests/fixtures/valid-opencode-stack.toml",
        ])
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

    let mut command = acps_command();

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
        ))
        .stderr(predicates::str::contains(
            "hint: run the command with `--help` and correct the invalid input",
        ));
}

#[test]
fn exports_default_home_config_to_stdout() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");

    let mut command = acps_command();

    command
        .env("HOME", tempdir.path())
        .args(["config", "export"])
        .assert()
        .success()
        .stdout(predicates::str::contains("[api]"))
        .stdout(predicates::str::contains("[agent.install]"))
        .stdout(predicates::str::contains(SESSION_KEY).not())
        .stdout(predicates::str::contains(ADMIN_KEY).not())
        .stdout(predicates::str::contains("sk-proj-exampleinlinevalue").not());
}

#[test]
fn exports_base64_default_home_config() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");

    let mut command = acps_command();
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
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");
    let output_path = tempdir.path().join("exported.toml");

    let mut command = acps_command();

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
        .stdout(predicates::str::contains("progress: loading config"))
        .stdout(predicates::str::contains(
            "progress: rendering config export",
        ))
        .stdout(predicates::str::contains("progress: writing config export"));

    let exported = fs::read_to_string(output_path).expect("export should be readable");
    assert!(exported.contains("[api]"));
    assert!(exported.contains("[agent.install]"));
}

#[test]
fn init_creates_config_and_state() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    let mut command = acps_command();

    command
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .success()
        .stdout(predicates::str::contains("progress: initializing auth"))
        .stdout(predicates::str::contains("initialized acp-stack"));

    let config_path = tempdir.path().join(".config/acp-stack/acps-config.toml");
    let state_path = tempdir.path().join(".local/share/acp-stack/state.sqlite");
    assert!(config_path.is_file());
    assert!(state_path.is_file());

    let config = fs::read_to_string(config_path).expect("starter config should be readable");
    assert!(
        !config.contains("[workspace.source]"),
        "starter config must not retain the legacy single-source block"
    );
    assert!(
        !config.contains("[[workspace.code_sources]]")
            && !config.contains("[[workspace.data_sources]]"),
        "starter config should declare no sources by default"
    );
}

#[test]
fn init_writes_mcp_declarations_to_starter_config() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--skip-testflight",
            "--skip-workspace-init",
            "--mcp-preset",
            "linear",
            "--mcp-stdio",
            "local=local-mcp",
            "--mcp-stdio-env",
            "local=LOCAL_MCP_TOKEN",
            "--mcp-http",
            "remote=https://mcp.example/mcp",
            "--mcp-http-header",
            "remote=Authorization:REMOTE_MCP_TOKEN",
        ])
        .assert()
        .success();

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("starter config should be readable");
    let config = load_config_from_str(&written).expect("starter config should validate");
    assert_eq!(config.mcp.servers.len(), 3);
    let linear = config
        .mcp
        .servers
        .iter()
        .find(|server| server.name() == "linear")
        .expect("linear preset should be written");
    let McpServerConfig::Http(linear) = linear else {
        panic!("linear preset should be an HTTP MCP server");
    };
    assert_eq!(linear.url, "https://mcp.linear.app/mcp");
    assert_eq!(linear.headers.len(), 1);
    assert_eq!(linear.headers[0].name, "Authorization");
    assert_eq!(linear.headers[0].value_ref, "LINEAR_API_KEY");

    let local = config
        .mcp
        .servers
        .iter()
        .find(|server| server.name() == "local")
        .expect("custom stdio server should be written");
    let McpServerConfig::Stdio(local) = local else {
        panic!("local MCP server should be stdio");
    };
    assert_eq!(local.command, "local-mcp");
    assert!(local.args.is_empty());
    assert_eq!(local.env, vec!["LOCAL_MCP_TOKEN"]);

    let remote = config
        .mcp
        .servers
        .iter()
        .find(|server| server.name() == "remote")
        .expect("custom HTTP server should be written");
    let McpServerConfig::Http(remote) = remote else {
        panic!("remote MCP server should be HTTP");
    };
    assert_eq!(remote.url, "https://mcp.example/mcp");
    assert_eq!(remote.headers.len(), 1);
    assert_eq!(remote.headers[0].name, "Authorization");
    assert_eq!(remote.headers[0].value_ref, "REMOTE_MCP_TOKEN");
}

#[test]
fn init_rejects_removed_startup_script_flag() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--skip-testflight",
            "--skip-workspace-init",
            "--startup-script",
            "bootstrap=echo ready",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--startup-script"));
}

#[test]
fn init_custom_agent_writes_install_config() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    // Completing init at all proves the registry-only gates were bypassed:
    // `should_install_agent` would otherwise fail `lookup_required` on a
    // non-registry id even when agent install is fixture-skipped.
    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--custom-agent-id",
            "my-agent",
            "--custom-agent-name",
            "My Agent",
            "--custom-agent-command",
            "my-agent-bin",
            "--custom-agent-arg",
            "acp",
            "--custom-agent-install",
            "echo install my-agent",
            "--custom-agent-creates",
            "my-agent-bin",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success();

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("custom agent config should be readable");
    let config = load_config_from_str(&written).expect("custom agent config should validate");
    assert_eq!(config.agent.id, "my-agent");
    assert_eq!(config.agent.name, "My Agent");
    assert_eq!(config.agent.command, "my-agent-bin");
    assert_eq!(config.agent.args, vec!["acp".to_owned()]);
    let install = config
        .agent
        .install
        .as_ref()
        .expect("custom agent must write an [agent.install] escape hatch");
    assert_eq!(install.install_type, "shell");
    assert_eq!(install.creates, "my-agent-bin");
    assert_eq!(install.shell.as_deref(), Some("echo install my-agent"));
    // The custom agent block must round-trip canonical TOML.
    let canonical = config
        .to_canonical_toml()
        .expect("custom agent config should round-trip canonical TOML");
    assert!(canonical.contains("[agent.install]"));
}

#[test]
fn init_custom_agent_rejects_placeholder_id() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--custom-agent-id",
            "placeholder",
            "--custom-agent-command",
            "x",
            "--custom-agent-install",
            "echo x",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("placeholder"));

    assert!(
        !tempdir
            .path()
            .join(".config/acp-stack/acps-config.toml")
            .exists(),
        "a rejected custom agent must not leave a config on disk"
    );
}

#[test]
fn init_custom_agent_rejects_registry_id() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--custom-agent-id",
            "placebo",
            "--custom-agent-command",
            "x",
            "--custom-agent-install",
            "echo x",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--agent placebo"));

    assert!(
        !tempdir
            .path()
            .join(".config/acp-stack/acps-config.toml")
            .exists(),
        "a rejected custom registry id must not leave a config on disk"
    );
}

#[test]
fn init_custom_agent_requires_command() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--custom-agent-id",
            "my-agent",
            "--custom-agent-install",
            "echo x",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--custom-agent-command"));
}

#[test]
fn init_custom_agent_rejects_blank_command() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--custom-agent-id",
            "my-agent",
            "--custom-agent-command",
            "   ",
            "--custom-agent-install",
            "echo x",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--custom-agent-command"));
}

#[test]
fn init_custom_agent_rejects_explicit_model_flag_on_rerun() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--custom-agent-id",
            "my-agent",
            "--custom-agent-command",
            "my-agent-bin",
            "--custom-agent-install",
            "echo install",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success();

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--model",
            "some-model",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--model"));
}

#[test]
fn init_custom_agent_rejects_explicit_mode_flag_on_rerun() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--custom-agent-id",
            "my-agent",
            "--custom-agent-command",
            "my-agent-bin",
            "--custom-agent-install",
            "echo install",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success();

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--mode",
            "review",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--mode"));
}

#[test]
fn init_custom_agent_allows_explicit_registry_agent_switch_on_rerun() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--custom-agent-id",
            "my-agent",
            "--custom-agent-command",
            "my-agent-bin",
            "--custom-agent-install",
            "echo install",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success();

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success();

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    let config = load_config_from_str(&written).expect("config should validate");
    assert_eq!(config.agent.id, "placebo");
    assert!(
        config.agent.install.is_none(),
        "switching to a registry agent should clear custom install config"
    );
}

#[cfg(unix)]
#[test]
fn init_custom_agent_fails_when_installed_command_is_absent() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let workspace = tempdir.path().join("ws");
    fs::create_dir_all(&workspace).expect("workspace dir should be created");
    let creates = tempdir.path().join("custom-agent-marker");
    fs::write(&creates, "#!/bin/sh\nexit 0\n").expect("creates marker should be written");
    let mut permissions = fs::metadata(&creates)
        .expect("creates marker metadata should be readable")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&creates, permissions).expect("creates marker should be executable");

    let output = acps_command()
        .env("HOME", tempdir.path())
        .env_remove(TEST_SKIP_AGENT_INSTALL_ENV)
        .args([
            "dev",
            "init",
            "--custom-agent-id",
            "my-agent",
            "--custom-agent-command",
            "acpstack-missing-custom-command",
            "--custom-agent-install",
            "true",
            "--custom-agent-creates",
            creates.to_str().expect("creates path should be utf8"),
            "--workspace-root",
            workspace.to_str().expect("workspace path should be utf8"),
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let stderr = String::from_utf8(output).expect("stderr should be utf8");
    assert!(
        stderr.contains("did not resolve after custom agent install"),
        "{stderr}"
    );
}

#[cfg(unix)]
#[test]
fn init_custom_agent_acp_gate_skips_when_spawn_cwd_absent() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let workspace = tempdir.path().join("missing-workspace");
    let creates = tempdir.path().join("custom-agent-marker");
    fs::write(&creates, "#!/bin/sh\nexit 0\n").expect("creates marker should be written");
    let mut permissions = fs::metadata(&creates)
        .expect("creates marker metadata should be readable")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&creates, permissions).expect("creates marker should be executable");

    acps_command()
        .env("HOME", tempdir.path())
        .env_remove(TEST_SKIP_AGENT_INSTALL_ENV)
        .args([
            "dev",
            "init",
            "--custom-agent-id",
            "my-agent",
            "--custom-agent-command",
            "bin/my-agent",
            "--custom-agent-install",
            "true",
            "--custom-agent-creates",
            creates.to_str().expect("creates path should be utf8"),
            "--workspace-root",
            workspace.to_str().expect("workspace path should be utf8"),
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("spawn cwd"));
}

#[test]
fn init_agent_env_ref_appends_to_config() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    // Pre-seed the referenced secret; `--agent-env-ref` references an existing
    // secret and fails fast otherwise.
    seed_init_secrets(tempdir.path(), &[("MY_AGENT_TOKEN", "token-value")]);

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--agent-env-ref",
            "MY_AGENT_TOKEN",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success();

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    let config = load_config_from_str(&written).expect("config should validate");
    assert!(
        config.agent.env.contains(&"MY_AGENT_TOKEN".to_owned()),
        "agent.env should contain the operator env ref, got {:?}",
        config.agent.env
    );
}

#[test]
fn init_agent_env_ref_missing_secret_fails_fast() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    let output = acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--agent-env-ref",
            "MISSING_TOKEN",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let stderr = String::from_utf8(output).expect("stderr should be utf8");
    assert!(
        stderr.contains("secret `MISSING_TOKEN` was not found in the secret store"),
        "{stderr}"
    );
    // The ref must NOT be persisted to agent.env when verification fails, or a
    // later `--resume` would complete with an unresolved env ref.
    let config_path = tempdir.path().join(".config/acp-stack/acps-config.toml");
    if config_path.is_file() {
        let written = fs::read_to_string(&config_path).expect("config should be readable");
        let config = load_config_from_str(&written).expect("config should validate");
        assert!(
            !config.agent.env.contains(&"MISSING_TOKEN".to_owned()),
            "a failed env-ref verification must not persist the ref: {:?}",
            config.agent.env
        );
    }
}

#[test]
fn init_agent_env_ref_rejected_for_existing_config() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success();

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--agent-env-ref",
            "MY_AGENT_TOKEN",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--agent-env-ref"));
}

#[test]
fn init_dep_flag_writes_user_scope_dependency() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--dep",
            "ripgrep=apt-get install -y ripgrep",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success();

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    let config = load_config_from_str(&written).expect("config should validate");
    let entry = config
        .dependencies
        .commands
        .iter()
        .find(|entry| entry.name == "ripgrep")
        .expect("ripgrep dependency should be declared");
    let install = entry
        .install
        .as_ref()
        .expect("dep should have install action");
    assert_eq!(install.shell, "apt-get install -y ripgrep");
    assert_eq!(install.scope, DependencyInstallScope::User);
}

#[test]
fn init_dep_system_flag_writes_system_scope() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--dep-system",
            "nginx=apt-get install -y nginx",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success();

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    let config = load_config_from_str(&written).expect("config should validate");
    let install = config
        .dependencies
        .commands
        .iter()
        .find(|entry| entry.name == "nginx")
        .and_then(|entry| entry.install.as_ref())
        .expect("nginx dependency should be declared with an install action");
    assert_eq!(install.scope, DependencyInstallScope::System);
}

#[test]
fn init_deps_apply_requires_yes_noninteractive() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--dep",
            "acpstack-absent-tool=true",
            "--deps-apply",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--deps-apply-yes"));
}

#[test]
fn init_deps_apply_runs_pending_action_and_surfaces_failure() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    // The tool is not on PATH (pending), so the apply step runs its shell,
    // which exits non-zero — proving the step executes and surfaces failure.
    let output = acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--dep",
            "acpstack-failtool=exit 3",
            "--deps-apply",
            "--deps-apply-yes",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let stderr = String::from_utf8(output).expect("stderr should be utf8");
    assert!(
        stderr.contains("acpstack-failtool failed (exit=3)"),
        "{stderr}"
    );
}

#[test]
fn init_custom_agent_acp_gate_skips_when_binary_absent() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    // The custom binary is not on PATH (install is fixture-skipped), so the
    // connection gate skips cleanly and init still completes.
    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--custom-agent-id",
            "ghost",
            "--custom-agent-command",
            "acpstack-ghost-binary",
            "--custom-agent-install",
            "echo install",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("acp connection check skipped"));
}

#[test]
fn init_custom_agent_acp_gate_fails_for_non_acp_binary() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let workspace = tempdir.path().join("ws");
    std::fs::create_dir_all(&workspace).expect("workspace dir should be created");

    // `true` is a real binary on PATH but does not speak ACP, so the gate runs
    // and surfaces a connection failure instead of completing init.
    let output = acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--custom-agent-id",
            "t",
            "--custom-agent-command",
            "true",
            "--custom-agent-install",
            "echo install",
            "--workspace-root",
            workspace.to_str().expect("workspace path should be utf8"),
            "--skip-workspace-init",
            "--skip-testflight",
        ])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let stderr = String::from_utf8(output).expect("stderr should be utf8");
    assert!(
        stderr.contains("failed to complete an ACP session"),
        "{stderr}"
    );
}

#[test]
fn init_stack_update_off_sets_manual_policy() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--stack-update",
            "off",
            "--stack-update-frequency",
            "6m",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success();

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    let config = load_config_from_str(&written).expect("config should validate");
    assert_eq!(config.updates.acp_stack.policy, StackUpdatePolicy::Manual);
    assert_eq!(config.updates.acp_stack.frequency, "1d");
}

#[test]
fn init_stack_update_on_writes_compatible_policy_and_frequency() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--stack-update",
            "on",
            "--stack-update-frequency",
            "3w",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success();

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    let config = load_config_from_str(&written).expect("config should validate");
    assert_eq!(
        config.updates.acp_stack.policy,
        StackUpdatePolicy::Compatible
    );
    assert_eq!(config.updates.acp_stack.frequency, "3w");
}

#[test]
fn init_stack_update_rejects_sub_day_frequency() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--stack-update",
            "security",
            "--stack-update-frequency",
            "6m",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("day (d) or week (w)"));

    assert!(
        !tempdir
            .path()
            .join(".config/acp-stack/acps-config.toml")
            .exists(),
        "invalid stack-update frequency must fail before config creation"
    );
}

#[test]
fn init_stack_update_rejects_invalid_policy_before_config_creation() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--stack-update",
            "securty",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("on|security|off"));

    assert!(
        !tempdir
            .path()
            .join(".config/acp-stack/acps-config.toml")
            .exists(),
        "invalid stack-update policy must fail before config creation"
    );
}

#[test]
fn init_stack_update_existing_config_preserves_policy_without_flags() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--stack-update",
            "on",
            "--stack-update-frequency",
            "3w",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success();

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success();

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    let config = load_config_from_str(&written).expect("config should validate");
    assert_eq!(
        config.updates.acp_stack.policy,
        StackUpdatePolicy::Compatible
    );
    assert_eq!(config.updates.acp_stack.frequency, "3w");
}

#[test]
fn init_stack_update_default_preserved_non_interactive() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success();

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    let config = load_config_from_str(&written).expect("config should validate");
    // No --stack-update flag and non-interactive: the schema defaults are untouched.
    assert_eq!(
        config.updates.acp_stack.policy,
        StackUpdatePolicy::SecurityCritical
    );
    assert_eq!(config.updates.acp_stack.frequency, "1d");
}

#[test]
fn init_rejects_invalid_mcp_declarations() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    for (extra_args, expected) in [
        (
            &["--mcp-http", "remote=http://mcp.example/mcp"][..],
            "mcp-http",
        ),
        (&["--mcp-http", "remote=https://"], "mcp-http"),
        (
            &["--mcp-http", "remote=https://token@mcp.example/mcp"],
            "credentials",
        ),
        (&["--mcp-preset", "unknown"], "mcp-preset"),
        (&["--mcp-stdio", "local"], "mcp-stdio"),
        (&["--mcp-stdio", "=local-mcp"], "mcp-stdio"),
        (&["--mcp-http", "remote="], "mcp-http"),
        (
            &[
                "--mcp-preset",
                "linear",
                "--mcp-http",
                "linear=https://mcp.example/mcp",
            ],
            "duplicate name",
        ),
        (
            &[
                "--mcp-stdio",
                "local=local-a",
                "--mcp-stdio",
                "local=local-b",
            ],
            "duplicate name",
        ),
        (
            &[
                "--mcp-http",
                "remote=https://mcp-a.example/mcp",
                "--mcp-http",
                "remote=https://mcp-b.example/mcp",
            ],
            "duplicate name",
        ),
        (
            &[
                "--mcp-stdio",
                "shared=local-mcp",
                "--mcp-http",
                "shared=https://mcp.example/mcp",
            ],
            "duplicate name",
        ),
        (
            &["--mcp-http-header", "remote=Authorization"],
            "mcp-http-header",
        ),
        (
            &[
                "--mcp-http",
                "remote=https://mcp.example/mcp",
                "--mcp-http-header",
                "remote=:REMOTE_MCP_TOKEN",
            ],
            "non-empty header",
        ),
        (
            &[
                "--mcp-http",
                "remote=https://mcp.example/mcp",
                "--mcp-http-header",
                "remote=Authorization:",
            ],
            "non-empty header",
        ),
        (
            &[
                "--mcp-http",
                "remote=https://mcp.example/mcp",
                "--mcp-http-header",
                "remote=Bad Header:REMOTE_MCP_TOKEN",
            ],
            "valid HTTP header name",
        ),
        (
            &[
                "--mcp-http-header",
                "missing=Authorization:REMOTE_MCP_TOKEN",
            ],
            "mcp-http-header",
        ),
        (
            &[
                "--mcp-stdio",
                "local=local-mcp",
                "--mcp-http-header",
                "local=Authorization:REMOTE_MCP_TOKEN",
            ],
            "not an HTTP server",
        ),
        (
            &[
                "--mcp-http",
                "remote=https://mcp.example/mcp",
                "--mcp-stdio-env",
                "remote=LOCAL_MCP_TOKEN",
            ],
            "not a stdio server",
        ),
        (
            &[
                "--mcp-stdio",
                "local=local-mcp",
                "--mcp-stdio-env",
                "local=BAD REF",
            ],
            "secret ref name",
        ),
        (
            &[
                "--mcp-http",
                "remote=https://mcp.example/mcp",
                "--mcp-http-header",
                "remote=Authorization:BAD REF",
            ],
            "secret ref name",
        ),
        (
            &[
                "--mcp-stdio",
                "local=local-mcp",
                "--mcp-stdio-env",
                "local=SHARED_MCP_TOKEN",
                "--mcp-http",
                "remote=https://mcp.example/mcp",
                "--mcp-http-header",
                "remote=Authorization:SHARED_MCP_TOKEN",
            ],
            "declared more than once",
        ),
        (
            &[
                "--mcp-http",
                "remote=https://mcp.example/mcp",
                "--mcp-http-header",
                "remote=Authorization:FIRST_TOKEN",
                "--mcp-http-header",
                "remote=authorization:SECOND_TOKEN",
            ],
            "already has header",
        ),
        (
            &["--mcp-stdio-env", "missing=LOCAL_MCP_TOKEN"],
            "mcp-stdio-env",
        ),
    ] {
        assert_init_mcp_failure(tempdir.path(), extra_args, expected);
    }
}

#[test]
fn init_rejects_mcp_declarations_when_config_exists() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success();

    for (extra_args, expected) in [
        (&["--mcp-preset", "linear"][..], "--mcp-preset"),
        (&["--mcp-stdio", "local=local-mcp"], "--mcp-stdio"),
        (
            &["--mcp-stdio-env", "local=LOCAL_MCP_TOKEN"],
            "--mcp-stdio-env",
        ),
        (
            &["--mcp-http", "remote=https://mcp.example/mcp"],
            "--mcp-http",
        ),
        (
            &["--mcp-http-header", "remote=Authorization:REMOTE_MCP_TOKEN"],
            "--mcp-http-header",
        ),
    ] {
        assert_init_mcp_failure(tempdir.path(), extra_args, expected);
    }
}

fn assert_init_mcp_failure(home: &std::path::Path, extra_args: &[&str], expected: &str) {
    acps_command()
        .env("HOME", home)
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .args(extra_args)
        .assert()
        .failure()
        .stderr(predicates::str::contains(expected));
}

#[test]
fn init_rejects_mcp_secret_ref_duplicates_after_registry_defaults() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "amp",
            "--skip-testflight",
            "--skip-workspace-init",
            "--mcp-stdio",
            "local=local-mcp",
            "--mcp-stdio-env",
            "local=AMP_API_KEY",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("declared more than once"));
    assert!(
        !tempdir
            .path()
            .join(".config/acp-stack/acps-config.toml")
            .exists(),
        "invalid post-registry config must not be written"
    );
}

#[test]
fn init_rejects_private_drive_file_viewer_url_as_data_source() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "init",
            "--agent",
            "placebo",
            "--skip-testflight",
            "--data-from",
            "https://drive.google.com/file/d/abc123/view",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("private Drive file viewer link"));
}

#[test]
fn init_accepts_drive_uc_export_download_url_as_data_source() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--skip-testflight",
            "--skip-workspace-init",
            "--data-from",
            "https://drive.google.com/uc?export=download&id=abc123",
        ])
        .assert()
        .success();
}

#[test]
fn init_rejects_drive_folder_url_as_data_source() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "init",
            "--agent",
            "placebo",
            "--skip-testflight",
            "--data-from",
            "https://drive.google.com/drive/folders/abc123",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("Drive folder"));
}

#[test]
fn init_rejects_dropbox_preview_url_without_dl_flag() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "init",
            "--agent",
            "placebo",
            "--skip-testflight",
            "--data-from",
            "https://www.dropbox.com/scl/fi/abc123/file.zip?dl=0",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("Dropbox preview link"));
}

#[test]
fn init_accepts_dropbox_url_with_dl_one() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--skip-testflight",
            "--skip-workspace-init",
            "--data-from",
            "https://www.dropbox.com/scl/fi/abc123/file.zip?dl=1",
        ])
        .assert()
        .success();
}

#[test]
fn init_default_skips_testflight_under_non_interactive_runs() {
    // Non-interactive default with a registered agent: no --testflight, no
    // --skip-testflight, no stdin TTY. The runner should announce the skip
    // rather than silently continue — operators reading the log need to see
    // why testflight was not run.
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "testflight: skipped (non-interactive run; pass --testflight to opt in)",
        ));
}

#[test]
fn init_skip_testflight_flag_is_acknowledged_in_output() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "testflight: skipped (--skip-testflight)",
        ));
}

#[test]
fn init_creates_workspace_root_and_uploads_without_sources() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let workspace_root = tempdir.path().join("workspace");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "init",
            "--agent",
            "placebo",
            "--no-skills",
            "--skip-testflight",
            "--workspace-root",
            workspace_root.to_str().expect("workspace UTF-8"),
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "progress: materializing workspace sources",
        ))
        .stdout(predicates::str::contains("workspace root:"))
        .stdout(predicates::str::contains("workspace uploads:"));

    assert!(workspace_root.is_dir());
    assert!(workspace_root.join("uploads").is_dir());
}

#[test]
fn init_edge_profile_prints_edge_artifact_progress() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--no-skills",
            "--skip-testflight",
            "--skip-workspace-init",
            "--edge",
            "cloudflare",
            "--exposure",
            "tunnel",
            "--hostname",
            "agent.example.com",
            "--cloudflared-deployment",
            "external",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "progress: preparing Cloudflare edge artifacts",
        ))
        .stdout(predicates::str::contains(
            "workspace: skipped (--skip-workspace-init)",
        ))
        .stdout(predicates::str::contains("progress: materializing workspace sources").not());

    assert!(
        tempdir
            .path()
            .join(".config/acp-stack/cloudflared/config.yml")
            .is_file()
    );
    assert!(!tempdir.path().join("workspace").exists());
}

#[test]
fn init_skip_workspace_init_is_acknowledged_in_output() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let workspace_root = tempdir.path().join("workspace");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--no-skills",
            "--skip-testflight",
            "--skip-workspace-init",
            "--workspace-root",
            workspace_root.to_str().expect("workspace UTF-8"),
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "workspace: skipped (--skip-workspace-init)",
        ))
        .stdout(predicates::str::contains("progress: materializing workspace sources").not());

    assert!(!workspace_root.exists());
}

#[test]
fn init_rejects_skip_workspace_init_outside_dev_mode() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["init", "--skip-workspace-init"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--skip-workspace-init"))
        .stderr(predicates::str::contains(
            "acps dev init --skip-workspace-init",
        ));
}

#[test]
fn init_noninteractive_without_agent_fails_before_writing_config() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["init", "--non-interactive"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "non-interactive init requires selecting a real agent",
        ));

    assert!(
        !tempdir
            .path()
            .join(".config/acp-stack/acps-config.toml")
            .exists(),
        "failed non-interactive init without --agent must not write starter config"
    );
}

#[test]
fn init_help_hides_dev_only_workspace_skip() {
    acps_command()
        .args(["init", "--help"])
        .assert()
        .success()
        .stdout(predicates::str::contains("--skip-workspace-init").not());
}

#[test]
fn dev_init_help_shows_workspace_skip() {
    acps_command()
        .args(["dev", "init", "--help"])
        .assert()
        .success()
        .stdout(predicates::str::contains("--skip-workspace-init"));
}

#[test]
fn serve_help_hides_allow_root_outside_dev_command() {
    acps_command()
        .args(["serve", "--help"])
        .assert()
        .success()
        .stdout(predicates::str::contains("--allow-root").not());
}

#[test]
fn dev_serve_help_shows_allow_root() {
    acps_command()
        .args(["dev", "serve", "--help"])
        .assert()
        .success()
        .stdout(predicates::str::contains("--allow-root"));
}

#[test]
fn serve_rejects_dev_only_root_overrides() {
    let tempdir = tempfile::tempdir().expect("tempdir");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["serve", "--allow-root"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "development-only flag; use `acps dev serve --allow-root`",
        ));

    acps_command()
        .env("HOME", tempdir.path())
        .env("ACP_STACK_ALLOW_ROOT", "1")
        .args(["serve"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "development-only environment override; use `acps dev serve`",
        ));
}

#[test]
fn init_no_skills_flag_skips_skill_install_prompt() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--no-skills",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("initialized acp-stack"));
}

#[test]
fn init_rejects_skills_without_source() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["init", "--skills", "repo-map"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--skills-source"));
}

#[test]
fn init_rejects_source_without_skills() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["init", "--skills-source", "openai"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--skills"));
}

#[test]
fn init_validates_skill_names_before_download() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    seed_init_secrets(tempdir.path(), &[("OPENAI_API_KEY", "test-openai-key")]);

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "opencode",
            "--provider",
            "openai",
            "--skip-testflight",
            "--skip-workspace-init",
            "--skills-source",
            "openai",
            "--skills",
            "BadSkill",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("invalid skill name"));
}

#[test]
fn init_rejects_combining_testflight_and_skip_testflight() {
    // clap conflicts_with should fail at parse time, so init never starts.
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["init", "--testflight", "--skip-testflight"])
        .assert()
        .failure();
}

#[test]
fn init_explicit_testflight_prints_provider_credit_warning() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    seed_init_secrets(tempdir.path(), &[("OPENAI_API_KEY", "test-openai-key")]);

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "opencode",
            "--provider",
            "openai",
            "--testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .failure()
        .stdout(predicates::str::contains(
            "this may consume provider credits.",
        ));
}

#[test]
fn init_writes_deployment_controlled_workspace_defaults() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--workspace-root",
            "/srv/acp",
            "--workspace-uploads",
            "/srv/acp/uploads",
            "--runtime-user",
            "svc-acp",
            "--skip-workspace-init",
        ])
        .assert()
        .success();

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("starter config should be readable");
    let config = load_config_from_str(&written).expect("starter config should validate");
    assert_eq!(config.workspace.root, "/srv/acp");
    assert_eq!(config.workspace.uploads, "/srv/acp/uploads");
    assert_eq!(config.workspace.runtime_user, "svc-acp");
    assert_eq!(config.agent.cwd.as_deref(), Some("/srv/acp"));
}

#[test]
fn init_rejects_conflicting_deployment_overrides_for_existing_config() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .success();

    acps_command()
        .env("HOME", tempdir.path())
        .args(["init", "--workspace-root", "/srv/acp"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "query parameter `--workspace-root` is invalid",
        ))
        .stderr(predicates::str::contains(
            "deployment override applies only when creating a starter config",
        ));
}

#[test]
fn init_skips_opencode_config_without_configured_provider() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
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
    seed_init_secrets(tempdir.path(), &[("OPENAI_API_KEY", "test-openai-key")]);

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "opencode",
            "--provider",
            "openai",
            "--api-key-ref",
            "OPENAI_API_KEY",
            "--skip-workspace-init",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("OpenCode config:"));

    let config = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
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
fn init_provider_fails_noninteractive_when_default_secret_is_missing() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    let output = acps_command()
        .env_remove(TEST_SKIP_AGENT_INSTALL_ENV)
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "opencode",
            "--provider",
            "openai",
            "--skip-workspace-init",
        ])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let stderr = String::from_utf8(output).expect("stderr should be utf8");
    assert!(
        stderr.contains("secret `OPENAI_API_KEY` was not found in the secret store"),
        "{stderr}"
    );
    let run_id = stderr
        .lines()
        .find_map(|line| line.strip_prefix("init failed in run "))
        .expect("stderr should include failed init run id");
    assert!(
        stderr.contains("failed step: provider_configure"),
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!("retry: acps init --resume --run-id {run_id}")),
        "{stderr}"
    );
}

#[test]
fn init_existing_provider_requires_secret_before_model_discovery() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    seed_init_secrets(tempdir.path(), &[]);
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = format!(
        "{}\n\n[agent.provider]\nid = \"openai\"\napi_key_ref = \"OPENAI_API_KEY\"\n",
        VALID_CONFIG.replace(r#"env = ["OPENCODE_API_KEY"]"#, "env = []")
    );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--model",
            "openai/gpt-5.5",
            "--skip-workspace-init",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "secret `OPENAI_API_KEY` was not found in the secret store",
        ))
        .stderr(predicates::str::contains("failed step: provider_configure"));
}

#[test]
fn init_existing_provider_requires_secret_without_model_flag() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    seed_init_secrets(tempdir.path(), &[]);
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = format!(
        "{}\n\n[agent.provider]\nid = \"openai\"\napi_key_ref = \"OPENAI_API_KEY\"\n",
        VALID_CONFIG.replace(r#"env = ["OPENCODE_API_KEY"]"#, "env = []")
    );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--skip-workspace-init"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "secret `OPENAI_API_KEY` was not found in the secret store",
        ))
        .stderr(predicates::str::contains("failed step: provider_configure"));
}

#[test]
fn init_existing_provider_repairs_env_before_model_discovery() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    seed_init_secrets(tempdir.path(), &[("OPENAI_API_KEY", "test-openai-key")]);
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = format!(
        "{}\n\n[agent.provider]\nid = \"openai\"\napi_key_ref = \"OPENAI_API_KEY\"\n",
        VALID_CONFIG.replace(r#"env = ["OPENCODE_API_KEY"]"#, "env = []")
    );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");
    let options_path = write_acp_config_options(tempdir.path(), &["openai/gpt-5.5"], &[]);

    acps_command()
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args([
            "dev",
            "init",
            "--model",
            "openai/gpt-5.5",
            "--skip-workspace-init",
        ])
        .assert()
        .success();

    let config =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config should be readable");
    assert!(config.contains(r#"env = ["OPENAI_API_KEY"]"#));
    assert!(config.contains(r#"model = "openai/gpt-5.5""#));
}

#[test]
fn init_existing_provider_fills_default_api_key_ref_before_model_discovery() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    seed_init_secrets(tempdir.path(), &[("OPENAI_API_KEY", "test-openai-key")]);
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = format!(
        "{}\n\n[agent.provider]\nid = \"openai\"\n",
        VALID_CONFIG.replace(r#"env = ["OPENCODE_API_KEY"]"#, "env = []")
    );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");
    let options_path = write_acp_config_options(tempdir.path(), &["openai/gpt-5.5"], &[]);

    acps_command()
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args([
            "dev",
            "init",
            "--model",
            "openai/gpt-5.5",
            "--skip-workspace-init",
        ])
        .assert()
        .success();

    let config =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config should be readable");
    assert!(config.contains(r#"env = ["OPENAI_API_KEY"]"#));
    assert!(config.contains(r#"api_key_ref = "OPENAI_API_KEY""#));
    assert!(config.contains(r#"model = "openai/gpt-5.5""#));
}

#[test]
fn init_rejects_imported_provider_that_agent_does_not_support() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    seed_init_secrets(tempdir.path(), &[("CURSOR_API_KEY", "test-cursor-key")]);
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = format!(
        "{}\n\n[agent.provider]\nid = \"cursor\"\napi_key_ref = \"CURSOR_API_KEY\"\n",
        VALID_CONFIG.replace(r#"env = ["OPENCODE_API_KEY"]"#, "env = []")
    );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--skip-workspace-init"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "provider `cursor` is not supported for agent `opencode`",
        ))
        .stderr(predicates::str::contains("failed step: provider_configure"));
}

#[test]
fn init_skips_stale_provider_block_when_agent_cannot_set_provider() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    seed_init_secrets(tempdir.path(), &[]);
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = format!(
        "{}\n\n[agent.provider]\nid = \"openai\"\nmodel = \"openai/gpt-5.5\"\napi_key_ref = \"OPENAI_API_KEY\"\n",
        VALID_CONFIG
            .replace(r#"id = "opencode""#, r#"id = "cursor""#)
            .replace(r#"name = "OpenCode""#, r#"name = "Cursor CLI""#)
            .replace(r#"command = "opencode""#, r#"command = "cursor-agent""#)
            .replace(r#"env = ["OPENCODE_API_KEY"]"#, "env = []")
    );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");
    let options_path = write_acp_config_options(tempdir.path(), &["cursor/gpt-5.5"], &[]);

    acps_command()
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args([
            "dev",
            "init",
            "--model",
            "cursor/gpt-5.5",
            "--skip-workspace-init",
        ])
        .assert()
        .success();

    let config =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config should be readable");
    let config = load_config_from_str(&config).expect("config should parse");
    assert_eq!(config.agent.id, "cursor");
    assert_eq!(config.agent.model.as_deref(), Some("cursor/gpt-5.5"));
    assert!(config.agent.provider.is_none());
    assert!(
        !config.agent.env.iter().any(|name| name == "OPENAI_API_KEY"),
        "provider setup must not repair env for agents that cannot set provider"
    );
}

#[test]
fn init_resume_restores_recorded_edge_request_before_edge_step_exists() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    let output = acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "opencode",
            "--provider",
            "openai",
            "--edge",
            "cloudflare",
            "--exposure",
            "tunnel",
            "--hostname",
            "agent.example.com",
            "--cloudflared-deployment",
            "external",
            "--no-skills",
            "--skip-workspace-init",
        ])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let stderr = String::from_utf8(output).expect("stderr should be utf8");
    let run_id = stderr
        .lines()
        .find_map(|line| line.strip_prefix("init failed in run "))
        .expect("stderr should include failed init run id");

    seed_init_secrets(tempdir.path(), &[("OPENAI_API_KEY", "test-openai-key")]);

    acps_command()
        .env("HOME", tempdir.path())
        .args(["init", "--resume", "--run-id", run_id])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "progress: preparing Cloudflare edge artifacts",
        ))
        .stdout(predicates::str::contains(
            "workspace: skipped (--skip-workspace-init)",
        ))
        .stdout(predicates::str::contains("progress: materializing workspace sources").not());

    assert!(
        tempdir
            .path()
            .join(".config/acp-stack/cloudflared/config.yml")
            .is_file()
    );
    assert!(!tempdir.path().join("workspace").exists());
}

#[test]
fn init_resume_restores_recorded_provider_args_before_provider_step_exists() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir");
    let workspace = tempdir.path().join("workspace");
    fs::create_dir_all(&workspace).expect("workspace");
    let local_bin = tempdir.path().join(".local/bin");
    let managed_opencode = local_bin.join("opencode");
    fs::write(
        config_dir.join("agents.toml"),
        format!(
            r#"
[[agents]]
id = "opencode"
name = "OpenCode"
kind = "native"
headless_compatible = true
set_provider = true
set_model = true
allow_custom_provider = true
allow_custom_model = true
set_mode = true
support_doc = "docs/agents/opencode.md"

[agents.harness]
id = "opencode"

[agents.harness.install.shell]
script = "exit 9"
creates = {}
"#,
            toml_string(&managed_opencode.to_string_lossy()),
        ),
    )
    .expect("agents override");

    let output = acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "opencode",
            "--provider",
            "myprovider",
            "--custom-provider",
            "--provider-name",
            "My Provider",
            "--base-url",
            "https://api.myprovider.example/v1",
            "--api-key-ref",
            "MY_PROVIDER_API_KEY",
            "--model",
            "my-model",
            "--model-name",
            "My Model",
            "--workspace-root",
            workspace.to_str().expect("workspace UTF-8"),
            "--no-skills",
            "--skip-workspace-init",
            "--skip-testflight",
        ])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let stderr = String::from_utf8(output).expect("stderr should be utf8");
    let run_id = stderr
        .lines()
        .find_map(|line| line.strip_prefix("init failed in run "))
        .expect("stderr should include failed init run id");
    let config_before =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config should be readable");
    assert!(!config_before.contains("[agent.provider]"));

    fs::write(
        config_dir.join("agents.toml"),
        format!(
            r#"
[[agents]]
id = "opencode"
name = "OpenCode"
kind = "native"
headless_compatible = true
set_provider = true
set_model = true
allow_custom_provider = true
allow_custom_model = true
set_mode = true
support_doc = "docs/agents/opencode.md"

[agents.harness]
id = {}

[agents.harness.install.shell]
script = "true"
creates = "opencode"
"#,
            toml_string(env!("CARGO_BIN_EXE_placebo-agent")),
        ),
    )
    .expect("agents override");
    seed_init_secrets(
        tempdir.path(),
        &[("MY_PROVIDER_API_KEY", "test-provider-key")],
    );

    acps_command()
        .env("HOME", tempdir.path())
        .args(["init", "--resume", "--run-id", run_id])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "workspace: skipped (--skip-workspace-init)",
        ));

    let config_after =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config should be readable");
    assert!(config_after.contains("[agent.provider]"));
    assert!(config_after.contains(r#"id = "myprovider""#));
    assert!(config_after.contains("[agent.provider.custom]"));
    assert!(config_after.contains(r#"name = "My Provider""#));
    assert!(config_after.contains(r#"api_key_ref = "MY_PROVIDER_API_KEY""#));
    assert!(config_after.contains(r#"base_url = "https://api.myprovider.example/v1""#));
    assert!(config_after.contains(r#"model_name = "My Model""#));
}

#[test]
fn init_resume_restores_recorded_skip_testflight_before_testflight_step_exists() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    let output = acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "opencode",
            "--provider",
            "openai",
            "--no-skills",
            "--skip-workspace-init",
            "--skip-testflight",
        ])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let stderr = String::from_utf8(output).expect("stderr should be utf8");
    let run_id = stderr
        .lines()
        .find_map(|line| line.strip_prefix("init failed in run "))
        .expect("stderr should include failed init run id");

    seed_init_secrets(tempdir.path(), &[("OPENAI_API_KEY", "test-openai-key")]);

    acps_command()
        .env("HOME", tempdir.path())
        .args(["init", "--resume", "--run-id", run_id])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "testflight: skipped (--skip-testflight)",
        ))
        .stdout(
            predicates::str::contains(
                "testflight: skipped (non-interactive run; pass --testflight to opt in)",
            )
            .not(),
        );
}

#[test]
fn init_resume_restores_recorded_testflight_before_testflight_step_exists() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    let output = acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "opencode",
            "--provider",
            "openai",
            "--no-skills",
            "--skip-workspace-init",
            "--testflight",
        ])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let stderr = String::from_utf8(output).expect("stderr should be utf8");
    let run_id = stderr
        .lines()
        .find_map(|line| line.strip_prefix("init failed in run "))
        .expect("stderr should include failed init run id");

    seed_init_secrets(tempdir.path(), &[("OPENAI_API_KEY", "test-openai-key")]);

    let output = acps_command()
        .env("HOME", tempdir.path())
        .args(["init", "--resume", "--run-id", run_id])
        .assert()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("stdout should be utf8");
    assert!(
        stdout.contains("this may consume provider credits."),
        "{stdout}"
    );
    assert!(
        !stdout.contains("testflight: skipped (non-interactive run; pass --testflight to opt in)"),
        "{stdout}"
    );
}

#[test]
fn init_provider_succeeds_noninteractive_when_default_secret_exists() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    seed_init_secrets(tempdir.path(), &[("OPENAI_API_KEY", "test-openai-key")]);

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "opencode",
            "--provider",
            "openai",
            "--skip-workspace-init",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("OpenCode config:"));

    let config = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    assert!(config.contains(r#"api_key_ref = "OPENAI_API_KEY""#));
    assert!(config.contains(r#"env = ["OPENAI_API_KEY"]"#));
}

#[test]
fn init_custom_opencode_provider_writes_generated_config() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    seed_init_secrets(tempdir.path(), &[("CUSTOM_API_KEY", "test-custom-key")]);

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "opencode",
            "--provider",
            "myprovider",
            "--custom-provider",
            "--provider-name",
            "My Provider",
            "--base-url",
            "https://api.myprovider.example/v1",
            "--api-key-ref",
            "CUSTOM_API_KEY",
            "--model",
            "my-model",
            "--skip-workspace-init",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("OpenCode config:"));

    let config = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    assert!(config.contains(r#"id = "myprovider""#));
    assert!(config.contains(r#"api_key_ref = "CUSTOM_API_KEY""#));
    assert!(config.contains("[agent.provider.custom]"));
    assert!(config.contains(r#"api = "chat-completions""#));
    assert!(config.contains(r#"env = ["CUSTOM_API_KEY"]"#));

    let opencode_path = tempdir
        .path()
        .join(".config")
        .join("opencode")
        .join("opencode.json");
    let opencode: Value = serde_json::from_str(
        &fs::read_to_string(opencode_path).expect("opencode config should be readable"),
    )
    .expect("opencode config should parse");
    assert_eq!(opencode["model"], "my-model");
    assert_eq!(
        opencode["provider"]["myprovider"]["options"]["apiKey"],
        "{env:CUSTOM_API_KEY}"
    );
}

#[test]
fn init_custom_codex_provider_allows_known_mapped_provider_id() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    seed_init_secrets(
        tempdir.path(),
        &[("ANTHROPIC_API_KEY", "test-anthropic-key")],
    );

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "codex",
            "--provider",
            "anthropic",
            "--custom-provider",
            "--provider-name",
            "Anthropic Custom",
            "--base-url",
            "https://api.anthropic.example/v1",
            "--api-key-ref",
            "ANTHROPIC_API_KEY",
            "--model",
            "claude-custom",
            "--skip-workspace-init",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("Codex config:"));

    let config = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    assert!(config.contains(r#"id = "anthropic""#));
    assert!(config.contains("[agent.provider.custom]"));
    assert!(config.contains(r#"api = "responses""#));

    let codex_path = tempdir.path().join(".codex").join("config.toml");
    let codex: toml::Value =
        toml::from_str(&fs::read_to_string(codex_path).expect("codex config should be readable"))
            .expect("codex config should parse");
    assert_eq!(codex["model_provider"].as_str(), Some("anthropic"));
    assert_eq!(
        codex["model_providers"]["anthropic"]["wire_api"].as_str(),
        Some("responses")
    );
}

// L84-L87 cover the provisional ACP discovery flow during init: validate
// explicit `--model`/`--mode` against the harness's advertised values
// (L86) and surface the list when non-interactive callers omit `--model`
// (L87). The fixture env var short-circuits the actual spawn so these
// tests don't depend on a real opencode binary being installed.
fn write_workspace_init_config(home: &std::path::Path) {
    let config_dir = home.join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir");
    let workspace = home.join("workspace");
    fs::create_dir_all(&workspace).expect("workspace dir");
    let config = VALID_CONFIG
        .replace(
            r#"root = "/workspace""#,
            &format!(r#"root = "{}""#, workspace.display()),
        )
        .replace(
            r#"uploads = "/workspace/uploads""#,
            &format!(r#"uploads = "{}/uploads""#, workspace.display()),
        )
        .replace(
            r#"cwd = "/workspace""#,
            &format!(r#"cwd = "{}""#, workspace.display()),
        )
        .replace(r#"command = "opencode""#, r#"command = "/bin/true""#);
    fs::write(config_dir.join("acps-config.toml"), config).expect("config");
}

fn acps_with_empty_path(home: &std::path::Path) -> Command {
    let empty_bin = home.join("empty-bin");
    fs::create_dir_all(&empty_bin).expect("empty PATH dir");
    let mut command = acps_command();
    command.env("PATH", empty_bin);
    command
}

#[test]
fn init_explicit_model_validates_against_acp_advertised_values() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    write_workspace_init_config(tempdir.path());
    seed_init_secrets(tempdir.path(), &[("OPENAI_API_KEY", "test-openai-key")]);
    let options_path = write_acp_config_options(tempdir.path(), &["openai/gpt-5.5"], &[]);

    acps_with_empty_path(tempdir.path())
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args([
            "init",
            "--agent",
            "opencode",
            "--provider",
            "openai",
            "--api-key-ref",
            "OPENAI_API_KEY",
            "--model",
            "openai/gpt-5.5",
        ])
        .assert()
        .success();

    let config = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    assert!(config.contains(r#"model = "openai/gpt-5.5""#));
    assert!(!config.contains("[agent.subagent"));

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
    assert_eq!(opencode["small_model"], "openai/gpt-5.5");
}

#[test]
fn init_explicit_model_accepts_provider_model_shorthand() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    write_workspace_init_config(tempdir.path());
    seed_init_secrets(
        tempdir.path(),
        &[("OPENROUTER_API_KEY", "test-openrouter-key")],
    );
    let options_path = write_acp_config_options(
        tempdir.path(),
        &["openrouter/deepseek/deepseek-v4-flash"],
        &[],
    );

    acps_with_empty_path(tempdir.path())
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args([
            "init",
            "--agent",
            "opencode",
            "--provider",
            "openrouter",
            "--api-key-ref",
            "OPENROUTER_API_KEY",
            "--model",
            "deepseek/deepseek-v4-flash",
        ])
        .assert()
        .success();

    let config = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    assert!(config.contains(r#"model = "openrouter/deepseek/deepseek-v4-flash""#));

    let opencode_path = tempdir
        .path()
        .join(".config")
        .join("opencode")
        .join("opencode.json");
    let opencode: Value = serde_json::from_str(
        &fs::read_to_string(opencode_path).expect("opencode config should be readable"),
    )
    .expect("opencode config should parse");
    assert_eq!(opencode["model"], "openrouter/deepseek/deepseek-v4-flash");
}

#[test]
fn init_explicit_model_shorthand_prefers_selected_provider() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    write_workspace_init_config(tempdir.path());
    seed_init_secrets(
        tempdir.path(),
        &[("OPENROUTER_API_KEY", "test-openrouter-key")],
    );
    let options_path = write_acp_config_options(
        tempdir.path(),
        &[
            "deepseek/deepseek-v4-flash",
            "openrouter/deepseek/deepseek-v4-flash",
        ],
        &[],
    );

    acps_with_empty_path(tempdir.path())
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args([
            "init",
            "--agent",
            "opencode",
            "--provider",
            "openrouter",
            "--api-key-ref",
            "OPENROUTER_API_KEY",
            "--model",
            "deepseek/deepseek-v4-flash",
        ])
        .assert()
        .success();

    let config = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    assert!(config.contains(r#"model = "openrouter/deepseek/deepseek-v4-flash""#));
    assert!(!config.contains(r#"model = "deepseek/deepseek-v4-flash""#));
}

#[test]
fn init_rejected_model_restores_prior_headless_config() {
    // Pre-write a prior opencode headless config, then run init with
    // an unadvertised --model. The init must reject the value AND
    // leave the prior headless config exactly as it was (rollback
    // guarantee).
    let tempdir = tempfile::tempdir().expect("tempdir");
    write_workspace_init_config(tempdir.path());
    seed_init_secrets(tempdir.path(), &[("OPENAI_API_KEY", "test")]);
    let prior_opencode_path = tempdir
        .path()
        .join(".config")
        .join("opencode")
        .join("opencode.json");
    fs::create_dir_all(prior_opencode_path.parent().expect("parent")).expect("opencode dir");
    let prior_bytes = b"{\"prior\":\"sentinel\"}";
    fs::write(&prior_opencode_path, prior_bytes).expect("prior opencode config");

    let options_path = write_acp_config_options(tempdir.path(), &["openai/gpt-5.5"], &[]);

    acps_with_empty_path(tempdir.path())
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args([
            "init",
            "--agent",
            "opencode",
            "--provider",
            "openai",
            "--api-key-ref",
            "OPENAI_API_KEY",
            "--model",
            "definitely-not-advertised",
        ])
        .assert()
        .failure();

    let after = fs::read(&prior_opencode_path).expect("opencode config readable after rejection");
    assert_eq!(
        after, prior_bytes,
        "rejected --model must restore prior opencode headless config exactly",
    );
}

#[test]
fn init_explicit_model_rejects_value_not_in_advertised_list() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    write_workspace_init_config(tempdir.path());
    seed_init_secrets(tempdir.path(), &[("OPENAI_API_KEY", "test-openai-key")]);
    let options_path = write_acp_config_options(tempdir.path(), &["openai/gpt-5.5"], &[]);

    acps_with_empty_path(tempdir.path())
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args([
            "init",
            "--agent",
            "opencode",
            "--provider",
            "openai",
            "--api-key-ref",
            "OPENAI_API_KEY",
            "--model",
            "made-up-model",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "agent did not advertise `made-up-model` as an available `model`",
        ))
        .stderr(predicates::str::contains(
            "advertised models: [openai/gpt-5.5]",
        ));

    let config = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    assert!(!config.contains("made-up-model"));
}

#[test]
fn init_noninteractive_missing_model_prints_advertised_values_without_mutating_config() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    write_workspace_init_config(tempdir.path());
    seed_init_secrets(tempdir.path(), &[("OPENAI_API_KEY", "test-openai-key")]);
    let options_path =
        write_acp_config_options(tempdir.path(), &["openai/gpt-5.5", "openai/o4-mini"], &[]);

    acps_with_empty_path(tempdir.path())
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args([
            "init",
            "--agent",
            "opencode",
            "--provider",
            "openai",
            "--api-key-ref",
            "OPENAI_API_KEY",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("advertised models for OpenCode:"))
        .stdout(predicates::str::contains("openai/gpt-5.5"))
        .stdout(predicates::str::contains("openai/o4-mini"))
        .stdout(predicates::str::contains(
            "rerun with `acps init --model <value>` to write a model into config",
        ));

    let config = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    // L87 contract: provider was set this run, but the no-flag path
    // must not write a model into config.
    assert!(config.contains(r#"id = "openai""#));
    assert!(!config.contains(r#"model = "openai/"#));
}

#[test]
fn init_explicit_mode_validates_against_acp_advertised_values() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    write_workspace_init_config(tempdir.path());
    seed_init_secrets(tempdir.path(), &[("OPENAI_API_KEY", "test-openai-key")]);
    let options_path =
        write_acp_config_options(tempdir.path(), &["openai/gpt-5.5"], &["build", "plan"]);

    acps_with_empty_path(tempdir.path())
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args([
            "init",
            "--agent",
            "opencode",
            "--provider",
            "openai",
            "--api-key-ref",
            "OPENAI_API_KEY",
            "--model",
            "openai/gpt-5.5",
            "--mode",
            "plan",
        ])
        .assert()
        .success();

    let config = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    assert!(config.contains(r#"mode = "plan""#));
}

#[test]
fn init_explicit_mode_rejects_value_not_in_advertised_list() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    write_workspace_init_config(tempdir.path());
    seed_init_secrets(tempdir.path(), &[("OPENAI_API_KEY", "test-openai-key")]);
    let options_path =
        write_acp_config_options(tempdir.path(), &["openai/gpt-5.5"], &["build", "plan"]);

    acps_with_empty_path(tempdir.path())
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args([
            "init",
            "--agent",
            "opencode",
            "--provider",
            "openai",
            "--api-key-ref",
            "OPENAI_API_KEY",
            "--model",
            "openai/gpt-5.5",
            "--mode",
            "executor",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "agent did not advertise `executor` as an available `mode`",
        ))
        .stderr(predicates::str::contains("advertised modes: [build, plan]"));
}

#[test]
fn init_mode_only_does_not_print_model_picker() {
    // OpenCode advertises both model and mode. Running --mode plan
    // with an existing provider should only exercise the mode lane; the
    // model lane stays dormant. Regression for an audit-flagged
    // bug where `configure_model_for_init` ran whenever set_model was
    // true, surfacing an unrelated advertised-models block.
    let tempdir = tempfile::tempdir().expect("tempdir");
    write_workspace_init_config(tempdir.path());
    seed_init_secrets(tempdir.path(), &[("OPENAI_API_KEY", "test")]);
    let config_path = tempdir.path().join(".config/acp-stack/acps-config.toml");
    let mut config = fs::read_to_string(&config_path).expect("config should be readable");
    config.push_str("\n[agent.provider]\nid = \"openai\"\napi_key_ref = \"OPENAI_API_KEY\"\n");
    fs::write(&config_path, config).expect("config should be writable");
    let options_path =
        write_acp_config_options(tempdir.path(), &["openai/gpt-5.5"], &["build", "plan"]);

    acps_with_empty_path(tempdir.path())
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args(["init", "--agent", "opencode", "--mode", "plan"])
        .assert()
        .success()
        .stdout(predicates::str::contains("advertised models").not())
        .stdout(predicates::str::contains("advertised modes").not());

    let config = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    assert!(config.contains(r#"mode = "plan""#));
}

#[test]
fn init_provider_change_without_model_clears_stale_opencode_model() {
    // Pre-existing opencode.json with a stale model from a prior run.
    // An init that switches provider without picking a new model
    // (L87 path) must clear the stale model field so the launched
    // harness doesn't silently use it under the new provider.
    let tempdir = tempfile::tempdir().expect("tempdir");
    write_workspace_init_config(tempdir.path());
    seed_init_secrets(tempdir.path(), &[("OPENAI_API_KEY", "test")]);
    let opencode_path = tempdir
        .path()
        .join(".config")
        .join("opencode")
        .join("opencode.json");
    fs::create_dir_all(opencode_path.parent().expect("parent")).expect("opencode dir");
    fs::write(
        &opencode_path,
        br#"{"model":"anthropic/claude-sonnet-stale","provider":{"anthropic":{}}}"#,
    )
    .expect("prior opencode config");

    let options_path = write_acp_config_options(tempdir.path(), &["openai/gpt-5.5"], &[]);

    acps_with_empty_path(tempdir.path())
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args([
            "init",
            "--agent",
            "opencode",
            "--provider",
            "openai",
            "--api-key-ref",
            "OPENAI_API_KEY",
        ])
        .assert()
        .success();

    let after: Value =
        serde_json::from_str(&fs::read_to_string(&opencode_path).expect("opencode readable"))
            .expect("opencode parses");
    assert!(
        after.get("model").is_none(),
        "opencode.json must not retain the stale model field after L87 provider-only init",
    );
}

#[test]
fn init_same_provider_without_model_preserves_existing_model() {
    // First init pins provider=openai, model=openai/gpt-5.5. Second
    // init re-runs with --provider openai but no --model. The L87
    // path must print the advertised list while preserving the
    // previously-pinned model — wiping it would silently change the
    // launched harness's model on a no-op rerun.
    let tempdir = tempfile::tempdir().expect("tempdir");
    write_workspace_init_config(tempdir.path());
    seed_init_secrets(tempdir.path(), &[("OPENAI_API_KEY", "test")]);
    let options_path =
        write_acp_config_options(tempdir.path(), &["openai/gpt-5.5", "openai/o4-mini"], &[]);

    acps_with_empty_path(tempdir.path())
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args([
            "init",
            "--agent",
            "opencode",
            "--provider",
            "openai",
            "--api-key-ref",
            "OPENAI_API_KEY",
            "--model",
            "openai/gpt-5.5",
        ])
        .assert()
        .success();

    acps_with_empty_path(tempdir.path())
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args([
            "init",
            "--agent",
            "opencode",
            "--provider",
            "openai",
            "--api-key-ref",
            "OPENAI_API_KEY",
        ])
        .assert()
        .success();

    let config = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    assert!(
        config.contains(r#"model = "openai/gpt-5.5""#),
        "second init --provider openai (no --model) must preserve the previously pinned model",
    );
}

#[test]
fn init_rejects_mode_for_agents_without_set_mode_before_discovery() {
    // Pi has set_model=true and set_mode=false. The unsupported-mode
    // rejection must fire as a capability check, BEFORE any binary /
    // cwd / discovery error can hide the real reason.
    let tempdir = tempfile::tempdir().expect("tempdir");
    seed_init_secrets(tempdir.path(), &[("ANTHROPIC_API_KEY", "test")]);

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "init",
            "--agent",
            "pi",
            "--provider",
            "anthropic",
            "--api-key-ref",
            "ANTHROPIC_API_KEY",
            "--mode",
            "plan",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "does not support mode configuration through `acps init`",
        ));
}

#[test]
fn init_custom_provider_still_validates_mode_against_acp_advertised_values() {
    // Custom-provider skips MODEL validation (the model id is freeform),
    // but MODE is independent of provider choice and must still be
    // validated against the agent's ACP advertisement.
    let tempdir = tempfile::tempdir().expect("tempdir");
    write_workspace_init_config(tempdir.path());
    seed_init_secrets(tempdir.path(), &[("CUSTOM_API_KEY", "test")]);
    let options_path =
        write_acp_config_options(tempdir.path(), &["openai/gpt-5.5"], &["build", "plan"]);

    acps_with_empty_path(tempdir.path())
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args([
            "init",
            "--agent",
            "opencode",
            "--provider",
            "myprovider",
            "--custom-provider",
            "--provider-name",
            "My Provider",
            "--base-url",
            "https://api.myprovider.example/v1",
            "--api-key-ref",
            "CUSTOM_API_KEY",
            "--model",
            "my-freeform-model",
            "--mode",
            "executor",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "agent did not advertise `executor` as an available `mode`",
        ));
}

#[test]
fn init_goose_custom_provider_provision_failure_removes_sidecar() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    seed_init_secrets(tempdir.path(), &[("CUSTOM_API_KEY", "test")]);
    let goose_config_path = tempdir
        .path()
        .join(".config")
        .join("goose")
        .join("config.yaml");
    fs::create_dir_all(goose_config_path.parent().expect("parent")).expect("goose config dir");
    fs::write(&goose_config_path, "[").expect("invalid goose config");

    let sidecar_path = tempdir
        .path()
        .join(".config")
        .join("goose")
        .join("custom_providers")
        .join("myprovider.json");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "goose",
            "--provider",
            "myprovider",
            "--custom-provider",
            "--provider-name",
            "My Provider",
            "--base-url",
            "https://api.myprovider.example/v1",
            "--api-key-ref",
            "CUSTOM_API_KEY",
            "--model",
            "my-freeform-model",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("existing YAML is invalid"));

    assert!(
        !sidecar_path.exists(),
        "failed goose custom-provider init must remove the generated sidecar",
    );
}

#[test]
fn init_pi_custom_provider_provision_failure_removes_models_json() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    seed_init_secrets(tempdir.path(), &[("CUSTOM_API_KEY", "test")]);
    let settings_path = tempdir
        .path()
        .join(".pi")
        .join("agent")
        .join("settings.json");
    fs::create_dir_all(settings_path.parent().expect("parent")).expect("pi settings dir");
    fs::write(&settings_path, "not json").expect("invalid pi settings");

    let models_path = tempdir.path().join(".pi").join("agent").join("models.json");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "pi",
            "--provider",
            "myprovider",
            "--custom-provider",
            "--provider-name",
            "My Provider",
            "--base-url",
            "https://api.myprovider.example/v1",
            "--api-key-ref",
            "CUSTOM_API_KEY",
            "--model",
            "my-freeform-model",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("existing JSON is invalid"));

    assert!(
        !models_path.exists(),
        "failed pi custom-provider init must remove generated models.json",
    );
}

#[test]
fn init_rejects_model_for_agents_without_set_model_before_discovery() {
    // amp has set_model=false; --model must fail fast as a capability
    // check rather than being silently ignored or surfacing as a
    // downstream "binary not on PATH" error.
    let tempdir = tempfile::tempdir().expect("tempdir");
    seed_init_secrets(tempdir.path(), &[("AMP_API_KEY", "test")]);

    acps_command()
        .env("HOME", tempdir.path())
        .args(["init", "--agent", "amp", "--model", "anything"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "does not support model configuration through `acps init`",
        ));
}

#[test]
fn init_custom_codex_provider_allows_openai_provider_id() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    seed_init_secrets(
        tempdir.path(),
        &[("CUSTOM_OPENAI_API_KEY", "test-custom-openai-key")],
    );

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "codex",
            "--provider",
            "openai",
            "--custom-provider",
            "--provider-name",
            "OpenAI Compatible",
            "--base-url",
            "https://api.compat.example/v1",
            "--api-key-ref",
            "CUSTOM_OPENAI_API_KEY",
            "--model",
            "custom-responses-model",
            "--skip-workspace-init",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("Codex config:"));

    let config = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    assert!(config.contains(r#"api_key_ref = "CUSTOM_OPENAI_API_KEY""#));
    assert!(config.contains("[agent.provider.custom]"));
}

#[test]
fn init_custom_provider_fails_noninteractive_when_required_fields_are_missing() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "init",
            "--agent",
            "opencode",
            "--provider",
            "myprovider",
            "--custom-provider",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "--provider-name is required for custom provider init",
        ));
}

#[test]
fn init_codex_openai_rejects_api_key_ref() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "init",
            "--agent",
            "codex",
            "--provider",
            "openai",
            "--api-key-ref",
            "OPENAI_API_KEY",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "Codex OpenAI uses Codex-native auth; do not pass --api-key-ref",
        ));
}

#[test]
fn init_provider_failure_persists_selected_agent_for_resume() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["init", "--agent", "amp", "--provider", "openai"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "Amp Code does not support provider configuration during init",
        ));

    let config =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config should be readable");
    assert!(config.contains(r#"id = "amp""#));
    assert!(!config.contains(r#"id = "opencode""#));
    assert!(!config.contains("[agent.provider]"));
}

#[test]
fn init_requires_provider_for_provider_capable_agent_without_existing_provider() {
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
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--agent", "pi", "--skip-workspace-init"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "Pi Agent supports provider configuration; pass --provider <id>",
        ))
        .stderr(predicates::str::contains("failed step: provider_configure"));
}

#[test]
fn agent_set_updates_config_and_generated_opencode_provider() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");
    let options_path = write_acp_config_options(tempdir.path(), &["openai/gpt-5.5"], &[]);

    acps_command()
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
        .stdout(predicates::str::contains("agent: opencode"))
        .stdout(predicates::str::contains("api_key_ref: OPENAI_API_KEY"))
        .stdout(predicates::str::contains(
            "restart the supervised agent (`POST /v1/agent/restart`) to reload from disk",
        ));

    let config = fs::read_to_string(config_dir.join("acps-config.toml"))
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
fn agent_set_uses_agent_native_provider_id_for_collapsed_provider() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");
    let options_path = write_acp_config_options(tempdir.path(), &["vercel/test-model"], &[]);

    acps_command()
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args([
            "agent",
            "set",
            "--provider",
            "vercel-ai-gateway",
            "--model",
            "test-model",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("api_key_ref: AI_GATEWAY_API_KEY"));

    let config = fs::read_to_string(config_dir.join("acps-config.toml"))
        .expect("updated config should be readable");
    assert!(config.contains(r#"id = "vercel-ai-gateway""#));
    assert!(config.contains(r#"model = "vercel/test-model""#));

    let opencode_path = tempdir
        .path()
        .join(".config")
        .join("opencode")
        .join("opencode.json");
    let opencode: Value = serde_json::from_str(
        &fs::read_to_string(opencode_path).expect("opencode config should be readable"),
    )
    .expect("opencode config should parse");
    assert_eq!(opencode["model"], "vercel/test-model");
    assert_eq!(
        opencode["provider"]["vercel"]["options"]["apiKey"],
        "{env:AI_GATEWAY_API_KEY}"
    );
    assert!(opencode["provider"]["vercel-ai-gateway"].is_null());
}

#[test]
fn agent_set_custom_opencode_provider_writes_generated_config() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "agent",
            "set",
            "--custom-provider",
            "--provider",
            "myprovider",
            "--provider-name",
            "My Provider",
            "--base-url",
            "https://api.myprovider.example/v1",
            "--api-key-ref",
            "CUSTOM_API_KEY",
            "--model",
            "my-model",
            "--model-name",
            "My Model",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("api_key_ref: CUSTOM_API_KEY"));

    let config = fs::read_to_string(config_dir.join("acps-config.toml"))
        .expect("updated config should be readable");
    assert!(config.contains(r#"id = "myprovider""#));
    assert!(config.contains(r#"api_key_ref = "CUSTOM_API_KEY""#));
    assert!(config.contains("[agent.provider.custom]"));
    assert!(config.contains(r#"context = 200000"#));
    assert!(config.contains(r#"output_max_tokens = 65536"#));

    let opencode_path = tempdir
        .path()
        .join(".config")
        .join("opencode")
        .join("opencode.json");
    let opencode: Value = serde_json::from_str(
        &fs::read_to_string(opencode_path).expect("opencode config should be readable"),
    )
    .expect("opencode config should parse");
    assert_eq!(opencode["model"], "my-model");
    assert_eq!(
        opencode["provider"]["myprovider"]["options"]["apiKey"],
        "{env:CUSTOM_API_KEY}"
    );
    assert_eq!(
        opencode["provider"]["myprovider"]["models"]["my-model"]["limit"]["context"],
        200000
    );
}

#[test]
fn subagent_set_updates_config_and_generated_opencode_small_model() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = format!(
        "{}\n\n[agent.provider]\nid = \"openai\"\nmodel = \"openai/gpt-5.5\"\napi_key_ref = \"OPENAI_API_KEY\"\n",
        VALID_CONFIG.replace(
            r#"env = ["OPENCODE_API_KEY"]"#,
            r#"env = ["OPENCODE_API_KEY", "OPENAI_API_KEY"]"#,
        )
    );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");
    let options_path = write_acp_config_options(
        tempdir.path(),
        &["openai/gpt-5.5", "opencode-go/deepseek-v4-flash"],
        &[],
    );

    acps_command()
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args([
            "subagent",
            "set",
            "--provider",
            "opencode-go",
            "--model",
            "deepseek-v4-flash",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent: opencode"))
        .stdout(predicates::str::contains("subagent: small_model"))
        .stdout(predicates::str::contains("api_key_ref: OPENCODE_API_KEY"));

    let config = fs::read_to_string(config_dir.join("acps-config.toml"))
        .expect("updated config should be readable");
    assert!(config.contains("[agent.subagent.provider]"));
    assert!(config.contains(r#"model = "opencode-go/deepseek-v4-flash""#));

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
    assert_eq!(opencode["small_model"], "opencode-go/deepseek-v4-flash");
    assert_eq!(
        opencode["enabled_providers"],
        json!(["openai", "opencode-go"])
    );
}

#[test]
fn subagent_status_prints_provider_model_and_key_ref() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = format!(
        "{VALID_CONFIG}\n\n[agent.subagent.provider]\nid = \"opencode-go\"\nmodel = \"opencode-go/deepseek-v4-flash\"\napi_key_ref = \"OPENCODE_API_KEY\"\n"
    );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["subagent", "status"])
        .assert()
        .success()
        .stdout(predicates::str::contains("subagent: small_model"))
        .stdout(predicates::str::contains("provider: opencode-go"))
        .stdout(predicates::str::contains(
            "model: opencode-go/deepseek-v4-flash",
        ))
        .stdout(predicates::str::contains("api_key_ref: OPENCODE_API_KEY"));
}

#[test]
fn subagent_status_prints_inherited_main_model() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = format!(
        "{VALID_CONFIG}\n\n[agent.provider]\nid = \"opencode-go\"\nmodel = \"opencode-go/deepseek-v4-flash\"\napi_key_ref = \"OPENCODE_API_KEY\"\n"
    );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["subagent", "status"])
        .assert()
        .success()
        .stdout(predicates::str::contains("subagent: small_model"))
        .stdout(predicates::str::contains("status: inherited"))
        .stdout(predicates::str::contains("provider: opencode-go"))
        .stdout(predicates::str::contains(
            "model: opencode-go/deepseek-v4-flash",
        ))
        .stdout(predicates::str::contains("api_key_ref: OPENCODE_API_KEY"));
}

#[test]
fn subagent_match_clears_explicit_provider_and_uses_main_model() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = format!(
        "{}\n\n[agent.provider]\nid = \"openai\"\nmodel = \"openai/gpt-5.5\"\napi_key_ref = \"OPENAI_API_KEY\"\n\n[agent.subagent.provider]\nid = \"opencode-go\"\nmodel = \"opencode-go/deepseek-v4-flash\"\napi_key_ref = \"OPENCODE_API_KEY\"\n",
        VALID_CONFIG.replace(
            r#"env = ["OPENCODE_API_KEY"]"#,
            r#"env = ["OPENCODE_API_KEY", "OPENAI_API_KEY"]"#,
        )
    );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["subagent", "match"])
        .assert()
        .success()
        .stdout(predicates::str::contains("status: inherited"))
        .stdout(predicates::str::contains("provider: openai"))
        .stdout(predicates::str::contains("model: openai/gpt-5.5"))
        .stdout(predicates::str::contains("api_key_ref: OPENAI_API_KEY"));

    let config = fs::read_to_string(config_dir.join("acps-config.toml"))
        .expect("updated config should be readable");
    assert!(!config.contains("[agent.subagent"));

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
    assert_eq!(opencode["small_model"], "openai/gpt-5.5");
    assert_eq!(opencode["enabled_providers"], json!(["openai"]));
}

#[test]
fn subagent_match_reenables_inherit_after_disable() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = format!(
        "{}\n\n[agent.provider]\nid = \"openai\"\nmodel = \"openai/gpt-5.5\"\napi_key_ref = \"OPENAI_API_KEY\"\n\n[agent.subagent]\ndisabled = true\n",
        VALID_CONFIG.replace(
            r#"env = ["OPENCODE_API_KEY"]"#,
            r#"env = ["OPENCODE_API_KEY", "OPENAI_API_KEY"]"#,
        )
    );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["subagent", "match"])
        .assert()
        .success()
        .stdout(predicates::str::contains("status: inherited"))
        .stdout(predicates::str::contains("model: openai/gpt-5.5"));

    let config = fs::read_to_string(config_dir.join("acps-config.toml"))
        .expect("updated config should be readable");
    assert!(!config.contains("[agent.subagent"));

    let opencode_path = tempdir
        .path()
        .join(".config")
        .join("opencode")
        .join("opencode.json");
    let opencode: Value = serde_json::from_str(
        &fs::read_to_string(opencode_path).expect("opencode config should be readable"),
    )
    .expect("opencode config should parse");
    assert_eq!(opencode["small_model"], "openai/gpt-5.5");
}

#[test]
fn subagent_match_rejects_unsupported_agents() {
    for config in [codex_config(), goose_config()] {
        let tempdir = tempfile::tempdir().expect("tempdir should be created");
        let config_dir = tempdir.path().join(".config/acp-stack");
        fs::create_dir_all(&config_dir).expect("config dir should be created");
        fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

        acps_command()
            .env("HOME", tempdir.path())
            .args(["subagent", "match"])
            .assert()
            .failure()
            .stderr(predicates::str::contains(
                "Current agent does not support subagent configuration.",
            ));
    }
}

#[test]
fn subagent_match_requires_configured_main_model_without_mutating_config() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config_path = config_dir.join("acps-config.toml");
    fs::write(&config_path, VALID_CONFIG).expect("config should be written");
    let before = fs::read_to_string(&config_path).expect("config should be readable");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["subagent", "match"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "main provider/model must be configured before `acps subagent match`",
        ));

    let after = fs::read_to_string(config_path).expect("config should be readable after failure");
    assert_eq!(after, before);
}

#[test]
fn subagent_disable_writes_invalid_opencode_small_model() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = format!(
        "{}\n\n[agent.provider]\nid = \"openai\"\nmodel = \"openai/gpt-5.5\"\napi_key_ref = \"OPENAI_API_KEY\"\n",
        VALID_CONFIG.replace(
            r#"env = ["OPENCODE_API_KEY"]"#,
            r#"env = ["OPENCODE_API_KEY", "OPENAI_API_KEY"]"#,
        )
    );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["subagent", "disable"])
        .assert()
        .success()
        .stdout(predicates::str::contains("status: disabled"))
        .stdout(predicates::str::contains("model: invalid/model"));

    let config = fs::read_to_string(config_dir.join("acps-config.toml"))
        .expect("updated config should be readable");
    assert!(config.contains("[agent.subagent]"));
    assert!(config.contains("disabled = true"));
    assert!(!config.contains("[agent.subagent.provider]"));

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
    assert_eq!(opencode["small_model"], "invalid/model");
    assert_eq!(opencode["enabled_providers"], json!(["openai"]));
}

#[test]
fn subagent_free_infers_openrouter_from_main_provider() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = format!(
        "{}\n\n[agent.provider]\nid = \"openrouter\"\nmodel = \"openrouter/deepseek/deepseek-v4-flash\"\napi_key_ref = \"OPENROUTER_API_KEY\"\n",
        VALID_CONFIG.replace(
            r#"env = ["OPENCODE_API_KEY"]"#,
            r#"env = ["OPENROUTER_API_KEY"]"#,
        )
    );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["subagent", "free"])
        .assert()
        .success()
        .stdout(predicates::str::contains("provider: openrouter"))
        .stdout(predicates::str::contains("model: openrouter/free"));

    let config = fs::read_to_string(config_dir.join("acps-config.toml"))
        .expect("updated config should be readable");
    assert!(config.contains("[agent.subagent.provider]"));
    assert!(config.contains(r#"id = "openrouter""#));
    assert!(config.contains(r#"model = "openrouter/free""#));
    assert!(config.contains(r#"api_key_ref = "OPENROUTER_API_KEY""#));

    let opencode_path = tempdir
        .path()
        .join(".config")
        .join("opencode")
        .join("opencode.json");
    let opencode: Value = serde_json::from_str(
        &fs::read_to_string(opencode_path).expect("opencode config should be readable"),
    )
    .expect("opencode config should parse");
    assert_eq!(opencode["model"], "openrouter/deepseek/deepseek-v4-flash");
    assert_eq!(opencode["small_model"], "openrouter/free");
    assert_eq!(opencode["enabled_providers"], json!(["openrouter"]));
}

#[test]
fn subagent_free_can_use_opencode_big_pickle() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["subagent", "free"])
        .assert()
        .success()
        .stdout(predicates::str::contains("provider: opencode"))
        .stdout(predicates::str::contains("model: opencode/big-pickle"));

    let config = fs::read_to_string(config_dir.join("acps-config.toml"))
        .expect("updated config should be readable");
    assert!(config.contains(r#"id = "opencode""#));
    assert!(config.contains(r#"model = "opencode/big-pickle""#));
    assert!(config.contains(r#"api_key_ref = "OPENCODE_API_KEY""#));
}

#[test]
fn subagent_free_prefers_current_opencode_provider_over_stale_openrouter_env() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = format!(
        "{}\n\n[agent.provider]\nid = \"opencode-go\"\nmodel = \"opencode-go/deepseek-v4-flash\"\napi_key_ref = \"OPENCODE_API_KEY\"\n",
        VALID_CONFIG.replace(
            r#"env = ["OPENCODE_API_KEY"]"#,
            r#"env = ["OPENCODE_API_KEY", "OPENROUTER_API_KEY"]"#,
        )
    );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["subagent", "free"])
        .assert()
        .success()
        .stdout(predicates::str::contains("provider: opencode"))
        .stdout(predicates::str::contains("model: opencode/big-pickle"));

    let config = fs::read_to_string(config_dir.join("acps-config.toml"))
        .expect("updated config should be readable");
    assert!(config.contains(r#"id = "opencode""#));
    assert!(config.contains(r#"model = "opencode/big-pickle""#));
}

#[test]
fn subagent_free_rejects_provider_without_free_support() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = format!(
        "{}\n\n[agent.provider]\nid = \"openai\"\nmodel = \"openai/gpt-5.5\"\napi_key_ref = \"OPENAI_API_KEY\"\n",
        VALID_CONFIG.replace(
            r#"env = ["OPENCODE_API_KEY"]"#,
            r#"env = ["OPENAI_API_KEY"]"#,
        )
    );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["subagent", "free"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "Current provider does not support free.",
        ));
}

#[test]
fn subagent_free_rejects_unsupported_main_provider_despite_stale_free_env() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = format!(
        "{}\n\n[agent.provider]\nid = \"openai\"\nmodel = \"openai/gpt-5.5\"\napi_key_ref = \"OPENAI_API_KEY\"\n",
        VALID_CONFIG.replace(
            r#"env = ["OPENCODE_API_KEY"]"#,
            r#"env = ["OPENAI_API_KEY", "OPENCODE_API_KEY", "OPENROUTER_API_KEY"]"#,
        )
    );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["subagent", "free"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "Current provider does not support free.",
        ));
}

#[test]
fn subagent_free_resolves_opencode_go_alias_with_custom_main_api_key_ref() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = format!(
        "{}\n\n[agent.provider]\nid = \"opencode-go\"\nmodel = \"opencode-go/deepseek-v4-flash\"\napi_key_ref = \"MY_OPENCODE_KEY\"\n",
        VALID_CONFIG.replace(
            r#"env = ["OPENCODE_API_KEY"]"#,
            r#"env = ["MY_OPENCODE_KEY"]"#,
        )
    );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["subagent", "free"])
        .assert()
        .success()
        .stdout(predicates::str::contains("provider: opencode"))
        .stdout(predicates::str::contains("model: opencode/big-pickle"));

    let config = fs::read_to_string(config_dir.join("acps-config.toml"))
        .expect("updated config should be readable");
    assert!(config.contains(r#"api_key_ref = "MY_OPENCODE_KEY""#));
    assert!(!config.contains("OPENCODE_API_KEY"));
}

#[test]
fn subagent_free_preserves_custom_main_api_key_ref_when_provider_matches() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = format!(
        "{}\n\n[agent.provider]\nid = \"openrouter\"\nmodel = \"openrouter/some-paid-model\"\napi_key_ref = \"MY_OPENROUTER_KEY\"\n",
        VALID_CONFIG.replace(
            r#"env = ["OPENCODE_API_KEY"]"#,
            r#"env = ["MY_OPENROUTER_KEY"]"#,
        )
    );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["subagent", "free"])
        .assert()
        .success()
        .stdout(predicates::str::contains("provider: openrouter"))
        .stdout(predicates::str::contains("model: openrouter/free"));

    let config = fs::read_to_string(config_dir.join("acps-config.toml"))
        .expect("updated config should be readable");
    assert!(config.contains(r#"api_key_ref = "MY_OPENROUTER_KEY""#));
    assert!(!config.contains("OPENROUTER_API_KEY"));
}

#[test]
fn subagent_set_inherits_provider_and_api_key_ref_from_main_when_omitted() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = format!(
        "{}\n\n[agent.provider]\nid = \"openai\"\nmodel = \"openai/gpt-5.5\"\napi_key_ref = \"OPENAI_CUSTOM_KEY\"\n",
        VALID_CONFIG.replace(
            r#"env = ["OPENCODE_API_KEY"]"#,
            r#"env = ["OPENCODE_API_KEY", "OPENAI_CUSTOM_KEY"]"#,
        )
    );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");
    let options_path = write_acp_config_options(
        tempdir.path(),
        &["openai/gpt-5.5", "openai/gpt-5.5-mini"],
        &[],
    );

    acps_command()
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args(["subagent", "set", "--model", "openai/gpt-5.5-mini"])
        .assert()
        .success()
        .stdout(predicates::str::contains("provider: openai"))
        .stdout(predicates::str::contains("model: openai/gpt-5.5-mini"))
        .stdout(predicates::str::contains("api_key_ref: OPENAI_CUSTOM_KEY"));

    let config = fs::read_to_string(config_dir.join("acps-config.toml"))
        .expect("updated config should be readable");
    assert!(config.contains("[agent.subagent.provider]"));
    assert!(config.contains(r#"id = "openai""#));
    assert!(config.contains(r#"model = "openai/gpt-5.5-mini""#));
    assert!(config.contains(r#"api_key_ref = "OPENAI_CUSTOM_KEY""#));
}

#[test]
fn subagent_set_requires_main_provider_when_provider_omitted() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["subagent", "set", "--model", "openai/gpt-5.5-mini"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "--provider not supplied and no main agent provider configured",
        ));
}

#[test]
fn subagent_set_rejects_unsupported_agents() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = VALID_CONFIG
        .replace(r#"id = "opencode""#, r#"id = "cursor""#)
        .replace(r#"name = "OpenCode""#, r#"name = "Cursor CLI""#)
        .replace(r#"command = "opencode""#, r#"command = "cursor-agent""#);
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "subagent",
            "set",
            "--provider",
            "openai",
            "--model",
            "openai/gpt-5.5",
            "--api-key-ref",
            "OPENAI_API_KEY",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "Current agent does not support subagent configuration.",
        ));
}

#[test]
fn subagent_set_rejects_codex_and_goose() {
    for config in [codex_config(), goose_config()] {
        let tempdir = tempfile::tempdir().expect("tempdir should be created");
        let config_dir = tempdir.path().join(".config/acp-stack");
        fs::create_dir_all(&config_dir).expect("config dir should be created");
        fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

        acps_command()
            .env("HOME", tempdir.path())
            .args([
                "subagent",
                "set",
                "--provider",
                "openai",
                "--model",
                "openai/gpt-5.5",
                "--api-key-ref",
                "OPENAI_API_KEY",
            ])
            .assert()
            .failure()
            .stderr(predicates::str::contains(
                "Current agent does not support subagent configuration.",
            ));
    }
}

#[test]
fn subagent_status_rejects_codex_and_goose() {
    for config in [codex_config(), goose_config()] {
        let tempdir = tempfile::tempdir().expect("tempdir should be created");
        let config_dir = tempdir.path().join(".config/acp-stack");
        fs::create_dir_all(&config_dir).expect("config dir should be created");
        fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

        acps_command()
            .env("HOME", tempdir.path())
            .args(["subagent", "status"])
            .assert()
            .failure()
            .stderr(predicates::str::contains(
                "Current agent does not support subagent configuration.",
            ));
    }
}

#[test]
fn subagent_set_rejects_registry_override_for_non_opencode_agent() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = VALID_CONFIG
        .replace(r#"id = "opencode""#, r#"id = "goose""#)
        .replace(r#"name = "OpenCode""#, r#"name = "Goose""#)
        .replace(r#"command = "opencode""#, r#"command = "goose""#);
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");
    seed_auth_verifiers(tempdir.path(), SESSION_KEY, ADMIN_KEY);
    fs::write(
        config_dir.join("agents.toml"),
        r#"
[[agents]]
id = "goose"
name = "Goose"
kind = "native"
headless_compatible = true
set_provider = true
set_model = true
allow_custom_provider = true
allow_custom_model = true
subagents = true
subagent_alias = "small_model"
support_doc = "docs/agents/goose.md"

[agents.harness]
id = "goose"

[agents.harness.install.shell]
script = "true"
creates = "goose"
"#,
    )
    .expect("registry override should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "subagent",
            "set",
            "--provider",
            "opencode-go",
            "--model",
            "opencode-go/deepseek-v4-flash",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "Current agent does not support subagent configuration.",
        ));
}

#[test]
fn agent_set_custom_provider_rejects_comma_token_limits() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "agent",
            "set",
            "--custom-provider",
            "--provider",
            "myprovider",
            "--provider-name",
            "My Provider",
            "--base-url",
            "https://api.myprovider.example/v1",
            "--api-key-ref",
            "CUSTOM_API_KEY",
            "--model",
            "my-model",
            "--context",
            "200,000",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "must be a plain integer without commas",
        ));
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
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");
    let options_path =
        write_acp_config_options(tempdir.path(), &["deepseek/deepseek-v4-flash"], &[]);

    acps_command()
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
        .stdout(predicates::str::contains("agent: goose"))
        .stdout(predicates::str::contains("api_key_ref: OPENROUTER_API_KEY"))
        .stdout(predicates::str::contains("Goose config:"))
        // Goose-specific notice: model is switchable live via ACP
        // session/set_config_option; other settings still apply on
        // new sessions.
        .stdout(predicates::str::contains(
            "model can be switched live via ACP session/set_config_option",
        ));

    let config = fs::read_to_string(config_dir.join("acps-config.toml"))
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
    assert_eq!(goose["GOOSE_MODEL"], "deepseek/deepseek-v4-flash");
}

#[test]
fn agent_set_codex_openrouter_writes_responses_provider_config() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), codex_config())
        .expect("config should be written");
    let options_path =
        write_acp_config_options(tempdir.path(), &["deepseek/deepseek-v4-flash"], &[]);

    acps_command()
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
        .stdout(predicates::str::contains("agent: codex"))
        .stdout(predicates::str::contains("api_key_ref: OPENROUTER_API_KEY"))
        .stdout(predicates::str::contains("Codex config:"))
        .stdout(predicates::str::contains(
            "restart the supervised agent (`POST /v1/agent/restart`) to reload from disk",
        ));

    let config = fs::read_to_string(config_dir.join("acps-config.toml"))
        .expect("updated config should be readable");
    assert!(config.contains("[agent.provider]"));
    assert!(config.contains(r#"id = "openrouter""#));
    assert!(config.contains(r#"model = "deepseek/deepseek-v4-flash""#));
    assert!(config.contains(r#"api_key_ref = "OPENROUTER_API_KEY""#));
    assert!(config.contains(r#"env = ["OPENROUTER_API_KEY"]"#));

    let codex_path = tempdir.path().join(".codex").join("config.toml");
    let codex: toml::Value =
        toml::from_str(&fs::read_to_string(codex_path).expect("codex config should be readable"))
            .expect("codex config should parse");
    assert_eq!(codex["model"].as_str(), Some("deepseek/deepseek-v4-flash"));
    assert_eq!(codex["model_provider"].as_str(), Some("openrouter"));
    assert_eq!(
        codex["model_providers"]["openrouter"]["base_url"].as_str(),
        Some("https://openrouter.ai/api/v1/responses")
    );
    assert_eq!(
        codex["model_providers"]["openrouter"]["name"].as_str(),
        Some("OpenRouter")
    );
    assert_eq!(
        codex["model_providers"]["openrouter"]["env_key"].as_str(),
        Some("OPENROUTER_API_KEY")
    );
    assert_eq!(
        codex["model_providers"]["openrouter"]["wire_api"].as_str(),
        Some("responses")
    );
}

#[test]
fn agent_set_codex_openai_model_removes_custom_provider_with_backup() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), codex_config())
        .expect("config should be written");
    let codex_dir = tempdir.path().join(".codex");
    fs::create_dir_all(&codex_dir).expect("codex config dir should be created");
    fs::write(
        codex_dir.join("config.toml"),
        r#"model = "deepseek/deepseek-v4-flash"
model_provider = "openrouter"
preserve = "yes"

[model_providers.openrouter]
name = "OpenRouter"
base_url = "https://openrouter.ai/api/v1/responses"
env_key = "OPENROUTER_API_KEY"
wire_api = "responses"
"#,
    )
    .expect("codex config should be written");
    fs::write(codex_dir.join("config.openrouter.toml"), "occupied\n")
        .expect("existing backup should be written");
    let options_path = write_acp_config_options(tempdir.path(), &["gpt-5.5"], &[]);

    acps_command()
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args(["agent", "set", "--provider", "openai", "--model", "gpt-5.5"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent: codex"))
        .stdout(predicates::str::contains("provider: openai"))
        .stdout(predicates::str::contains("model: gpt-5.5"))
        .stdout(predicates::str::contains("api_key_ref:").not())
        .stdout(predicates::str::contains(
            "restart the supervised agent (`POST /v1/agent/restart`) to reload from disk",
        ));

    let config = fs::read_to_string(config_dir.join("acps-config.toml"))
        .expect("updated config should be readable");
    assert!(config.contains("[agent.provider]"));
    assert!(config.contains(r#"id = "openai""#));
    assert!(config.contains(r#"model = "gpt-5.5""#));
    assert!(config.contains("env = []"));
    let parsed_config: toml::Value = toml::from_str(&config).expect("config should parse");
    assert!(
        parsed_config["agent"]["provider"]
            .get("api_key_ref")
            .is_none()
    );

    let codex: toml::Value = toml::from_str(
        &fs::read_to_string(codex_dir.join("config.toml"))
            .expect("codex config should be readable"),
    )
    .expect("codex config should parse");
    assert_eq!(codex["model"].as_str(), Some("gpt-5.5"));
    assert_eq!(codex["model_provider"].as_str(), Some("openai"));
    assert_eq!(codex["preserve"].as_str(), Some("yes"));
    assert!(codex.get("model_providers").is_none());
    let backup = fs::read_to_string(codex_dir.join("config.openrouter-1.toml"))
        .expect("backup should be readable");
    assert!(backup.contains(r#"model_provider = "openrouter""#));
    assert!(backup.contains("[model_providers.openrouter]"));
}

#[test]
fn agent_set_codex_openai_rejects_api_key_ref() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), codex_config())
        .expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "agent",
            "set",
            "--provider",
            "openai",
            "--model",
            "gpt-5.5",
            "--api-key-ref",
            "OPENAI_API_KEY",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "Codex OpenAI uses Codex-native auth; do not pass --api-key-ref",
        ));

    let config =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config should be readable");
    assert!(!config.contains("[agent.provider]"));
}

#[test]
fn agent_set_codex_openai_requires_model() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), codex_config())
        .expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["agent", "set", "--provider", "openai"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "pass --model <model-id> when setting Codex OpenAI provider",
        ));

    let config =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config should be readable");
    assert!(!config.contains("[agent.provider]"));
}

#[test]
fn agent_set_codex_rejects_unsupported_provider() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), codex_config())
        .expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "agent",
            "set",
            "--provider",
            "anthropic",
            "--model",
            "anthropic/claude-sonnet-4-5",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "provider `anthropic` is not supported for agent `codex`",
        ));
}

#[test]
fn agent_set_codex_custom_provider_defaults_to_responses() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), codex_config())
        .expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "agent",
            "set",
            "--custom-provider",
            "--provider",
            "myprovider",
            "--provider-name",
            "My Provider",
            "--base-url",
            "https://api.myprovider.example/v1",
            "--api-key-ref",
            "CUSTOM_API_KEY",
            "--model",
            "my-model",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("Codex config:"));

    let config = fs::read_to_string(config_dir.join("acps-config.toml"))
        .expect("updated config should be readable");
    assert!(config.contains(r#"api = "responses""#));

    let codex_path = tempdir.path().join(".codex").join("config.toml");
    let codex: toml::Value =
        toml::from_str(&fs::read_to_string(codex_path).expect("codex config should be readable"))
            .expect("codex config should parse");
    assert_eq!(codex["model_provider"].as_str(), Some("myprovider"));
    assert_eq!(
        codex["model_providers"]["myprovider"]["wire_api"].as_str(),
        Some("responses")
    );
}

#[test]
fn agent_set_codex_rejects_chat_completions_custom_provider() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), codex_config())
        .expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "agent",
            "set",
            "--custom-provider",
            "--provider",
            "myprovider",
            "--provider-name",
            "My Provider",
            "--base-url",
            "https://api.myprovider.example/v1",
            "--provider-api",
            "chat-completions",
            "--api-key-ref",
            "CUSTOM_API_KEY",
            "--model",
            "my-model",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "Codex custom providers only support responses",
        ));
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
    fs::write(config_dir.join("acps-config.toml"), &config).expect("config should be written");
    let options_path = write_acp_config_options(
        tempdir.path(),
        &["gpt-5.5[context=272k,reasoning=medium,fast=false]"],
        &[],
    );

    acps_command()
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args(["agent", "set", "--model", "gpt-5.5"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "required_env_refs: CURSOR_API_KEY",
        ));

    let after =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config should be readable");
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
    fs::write(config_dir.join("acps-config.toml"), &config).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["agent", "set", "--provider", "openai", "--model", "gpt-5.5"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "Cursor CLI does not support provider configuration",
        ));

    let after =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config should be readable");
    assert!(!after.contains("[agent.provider]"));
}

#[test]
fn agent_set_amp_rejects_custom_provider() {
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
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "agent",
            "set",
            "--custom-provider",
            "--provider",
            "myprovider",
            "--provider-name",
            "My Provider",
            "--base-url",
            "https://api.myprovider.example/v1",
            "--api-key-ref",
            "CUSTOM_API_KEY",
            "--model",
            "my-model",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "Amp Code does not support custom provider setup",
        ));
}

#[test]
fn agent_set_opencode_rejects_model_without_provider() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["agent", "set", "--model", "gpt-5.5"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "pass --provider <provider-id> when setting a model for OpenCode",
        ));
}

#[test]
fn agent_set_model_uses_existing_provider() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = VALID_CONFIG
        .replace(
            r#"env = ["OPENCODE_API_KEY"]"#,
            r#"env = ["OPENAI_API_KEY"]"#,
        )
        .replace(
            r#"restart = "on-crash""#,
            r#"restart = "on-crash"

[agent.provider]
id = "openai"
api_key_ref = "OPENAI_API_KEY""#,
        );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");
    let options_path = write_acp_config_options(tempdir.path(), &["openai/gpt-5.5"], &[]);

    acps_command()
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args(["agent", "set", "--model", "gpt-5.5"])
        .assert()
        .success()
        .stdout(predicates::str::contains("provider: openai"))
        .stdout(predicates::str::contains("model: openai/gpt-5.5"));

    let config =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config should be readable");
    assert!(config.contains("[agent.provider]"));
    assert!(config.contains(r#"model = "openai/gpt-5.5""#));
}

#[test]
fn agent_set_rejects_provider_not_supported_by_agent() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "agent",
            "set",
            "--provider",
            "azure-openai-responses",
            "--model",
            "azure-openai-responses/test-model",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "provider `azure-openai-responses` is not supported for agent `opencode`",
        ));

    let after =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config should be readable");
    assert!(!after.contains("[agent.provider]"));
}

#[test]
fn agent_set_rejects_providers_without_api_key_mapping() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");

    acps_command()
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
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config should be readable");
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
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");
    let options_path = write_acp_config_options(
        tempdir.path(),
        &["workers-ai/@cf/moonshotai/kimi-k2.6"],
        &[],
    );

    acps_command()
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

    let config = fs::read_to_string(config_dir.join("acps-config.toml"))
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
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");
    let options_path = write_acp_config_options(
        tempdir.path(),
        &["cloudflare-ai-gateway/workers-ai/@cf/moonshotai/kimi-k2.6"],
        &[],
    );

    acps_command()
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

    let config = fs::read_to_string(config_dir.join("acps-config.toml"))
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
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");
    let options_path = write_acp_config_options(
        tempdir.path(),
        &["cloudflare-workers-ai/@cf/moonshotai/kimi-k2.6"],
        &[],
    );

    acps_command()
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
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config should be readable");
    assert!(!config.contains("[agent.provider]"));
}

#[test]
fn agent_set_does_not_partially_write_main_config_when_provisioning_fails() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");
    let options_path = write_acp_config_options(tempdir.path(), &["openai/gpt-5.5"], &[]);
    let opencode_dir = tempdir.path().join(".config").join("opencode");
    fs::create_dir_all(&opencode_dir).expect("opencode config dir should be created");
    fs::write(opencode_dir.join("opencode.json"), "[]")
        .expect("invalid opencode config should be written");

    acps_command()
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
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config should be readable");
    assert!(!config.contains("[agent.provider]"));
    assert!(!config.contains(r#""OPENAI_API_KEY""#));
}

#[test]
fn agent_set_validates_model_against_acp_config_options() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");
    let options_path = write_acp_config_options(tempdir.path(), &["openai/gpt-5.5"], &[]);

    acps_command()
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
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config should be readable");
    assert!(!config.contains("[agent.provider]"));
}

#[test]
fn agent_set_amp_accepts_mode_only() {
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
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");
    let options_path = write_acp_config_options(tempdir.path(), &[], &["smart", "rush", "deep"]);

    acps_command()
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args(["agent", "set", "--mode", "smart"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent: amp"))
        .stdout(predicates::str::contains("mode: smart"))
        .stdout(predicates::str::contains(
            "restart the supervised agent (`POST /v1/agent/restart`) to reload from disk",
        ));

    let config =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config should be readable");
    assert!(config.contains(r#"mode = "smart""#));
    assert!(!config.contains("[agent.provider]"));
}

#[test]
fn agent_set_opencode_accepts_mode_only() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");
    let options_path = write_acp_config_options(tempdir.path(), &[], &["build", "plan"]);

    acps_command()
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args(["agent", "set", "--mode", "plan"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent: opencode"))
        .stdout(predicates::str::contains("mode: plan"))
        .stdout(predicates::str::contains(
            "restart the supervised agent (`POST /v1/agent/restart`) to reload from disk",
        ));

    let config =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config should be readable");
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
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");
    let options_path = write_acp_config_options(tempdir.path(), &[], &["agent", "ask", "plan"]);

    acps_command()
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args(["agent", "set", "--mode", "plan"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent: cursor"))
        .stdout(predicates::str::contains("mode: plan"))
        .stdout(predicates::str::contains(
            "restart the supervised agent (`POST /v1/agent/restart`) to reload from disk",
        ));

    let config =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config should be readable");
    assert!(config.contains(r#"mode = "plan""#));
    assert!(!config.contains("[agent.provider]"));
}

#[test]
fn agent_set_codex_accepts_mode_only() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), codex_config())
        .expect("config should be written");
    let options_path =
        write_acp_config_options(tempdir.path(), &[], &["read-only", "auto", "full-access"]);

    acps_command()
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args(["agent", "set", "--mode", "full-access"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent: codex"))
        .stdout(predicates::str::contains("mode: full-access"))
        .stdout(predicates::str::contains(
            "restart the supervised agent (`POST /v1/agent/restart`) to reload from disk",
        ));

    let config =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config should be readable");
    assert!(config.contains(r#"mode = "full-access""#));
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
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    acps_command()
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
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    acps_command()
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
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");
    seed_auth_verifiers(tempdir.path(), SESSION_KEY, ADMIN_KEY);
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

    acps_command()
        .env("HOME", tempdir.path())
        .args(["agent", "install", "--yes", "--admin-key", ADMIN_KEY])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "progress: preparing agent install",
        ))
        .stdout(predicates::str::contains(
            "progress: resolving agent install plan",
        ))
        .stdout(predicates::str::contains(
            "progress: installing resolved agent artifacts",
        ))
        .stdout(predicates::str::contains("agent install: installed"))
        .stdout(predicates::str::contains(
            binary_path.to_string_lossy().as_ref(),
        ));
}

#[cfg(unix)]
#[test]
fn init_creates_owner_only_config_and_state_paths() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .success();

    let config_dir = tempdir.path().join(".config/acp-stack");
    let state_dir = tempdir.path().join(".local/share/acp-stack");
    let config_path = config_dir.join("acps-config.toml");
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
    let config_path = config_dir.join("acps-config.toml");
    fs::write(&config_path, VALID_PLACEBO_CONFIG).expect("config should be written");

    let mut command = acps_command();

    command
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--skip-workspace-init"])
        .assert()
        .success()
        .stdout(predicates::str::contains("validated existing config"));

    let config = fs::read_to_string(config_path).expect("config should be readable");
    assert_eq!(config, VALID_PLACEBO_CONFIG);
}

#[test]
fn init_fails_when_existing_config_is_invalid() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(
        config_dir.join("acps-config.toml"),
        VALID_CONFIG.replace(r#"bind = "127.0.0.1:7700""#, r#"bind = "bad""#),
    )
    .expect("invalid config should be written");

    let mut command = acps_command();

    command
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "api.bind must be a socket address",
        ));
}

#[test]
fn status_reports_config_state_workspace_agent_sink_and_deps() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    // Use a tempdir workspace so the test is deterministic across hosts.
    // Without this, `acps init` would pick the production default
    // `/workspace`, which is writable inside Docker dev images and the
    // Railway runtime but absent on the maintainer's macOS host. Pinning
    // workspace.root to a controlled tempdir keeps the assertion below
    // valid in both environments.
    let workspace_dir = tempdir.path().join("workspace");
    std::fs::create_dir_all(&workspace_dir).expect("workspace dir should be created");
    let uploads_dir = workspace_dir.join("uploads");
    std::fs::create_dir_all(&uploads_dir).expect("uploads dir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .arg("--workspace-root")
        .arg(&workspace_dir)
        .arg("--workspace-uploads")
        .arg(&uploads_dir)
        .assert()
        .success();

    let workspace_str = workspace_dir.display().to_string();
    let mut command = acps_command();
    command
        .env("HOME", tempdir.path())
        .arg("status")
        .assert()
        .success()
        .stdout(predicates::str::contains("config:    ok ("))
        .stdout(predicates::str::contains("state:     ok ("))
        .stdout(predicates::str::contains("schema=21"))
        .stdout(predicates::str::contains("latest_event="))
        .stdout(predicates::str::contains(format!(
            "workspace: ok ({workspace_str})"
        )))
        .stdout(predicates::str::contains("agent:"))
        .stdout(predicates::str::contains("sink:      supabase disabled"))
        .stdout(predicates::str::contains("deps:      no apply runs"))
        .stdout(predicates::str::contains("daemon:   unavailable"));
}

#[test]
fn status_format_json_reports_same_top_level_sections() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let workspace_dir = tempdir.path().join("workspace");
    std::fs::create_dir_all(&workspace_dir).expect("workspace dir should be created");
    let uploads_dir = workspace_dir.join("uploads");
    std::fs::create_dir_all(&uploads_dir).expect("uploads dir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .arg("--workspace-root")
        .arg(&workspace_dir)
        .arg("--workspace-uploads")
        .arg(&uploads_dir)
        .assert()
        .success();

    let output = acps_command()
        .env("HOME", tempdir.path())
        .args(["status", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let body: Value = serde_json::from_slice(&output).expect("status json parses");
    assert_eq!(body["config"]["ok"], true);
    assert_eq!(body["workspace"]["ok"], true);
    assert_eq!(
        body["workspace"]["root"],
        workspace_dir.display().to_string()
    );
    assert!(body["state"]["schema_version"].as_i64().is_some(), "{body}");
    assert_eq!(body["daemon"]["status"], "unavailable");
}

#[test]
fn status_reports_sink_open_failures_when_supabase_configured() {
    use chrono::{SecondsFormat, Utc};

    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = VALID_CONFIG.replace("enabled = false", "enabled = true");
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    let state_path = default_state_path(tempdir.path());
    fs::create_dir_all(state_path.parent().expect("state parent dir"))
        .expect("state dir should be created");
    let mut store = StateStore::open(&state_path).expect("state should open");
    store.migrate().expect("migration should pass");
    store.set_external_logging_enabled(true);
    let now = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
    store
        .append_event_with_source(
            "info",
            "test.seed",
            EVENT_SOURCE_CLI,
            "seed sink_outbox row",
            "{}",
        )
        .expect("append seed event");
    let batch = store
        .next_sink_outbox_batch(10, &now)
        .expect("read outbox batch");
    let ids: Vec<String> = batch.iter().map(|row| row.id.clone()).collect();
    assert!(
        !ids.is_empty(),
        "seed event should have enqueued an outbox row"
    );
    store
        .mark_sink_outbox_failure(&ids, "boom", &now, &now)
        .expect("mark outbox failure");
    drop(store);

    acps_command()
        .env("HOME", tempdir.path())
        .arg("status")
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "sink:      1 open failures (supabase",
        ));
}

#[tokio::test(flavor = "multi_thread")]
async fn status_reports_ready_daemon_when_health_probe_is_healthy() {
    let probe = HealthProbeHarness::spawn(
        StatusCode::OK,
        serde_json::json!({
            "ok": true,
            "data": {
                "ok": true,
                "failing": []
            }
        }),
    )
    .await;
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home_with_socket(
        home.path(),
        "http://127.0.0.1:9",
        ADMIN_KEY,
        Some(&probe.socket_path),
    );

    acps_command()
        .env("HOME", home.path())
        .arg("status")
        .assert()
        .success()
        .stdout(predicates::str::contains("daemon:   ready"));
}

#[tokio::test(flavor = "multi_thread")]
async fn status_reports_degraded_daemon_without_failing_command() {
    let probe = HealthProbeHarness::spawn(
        StatusCode::SERVICE_UNAVAILABLE,
        serde_json::json!({
            "ok": false,
            "data": {
                "ok": false,
                "failing": ["sink", "deps"]
            }
        }),
    )
    .await;
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home_with_socket(
        home.path(),
        "http://127.0.0.1:9",
        ADMIN_KEY,
        Some(&probe.socket_path),
    );

    acps_command()
        .env("HOME", home.path())
        .arg("status")
        .assert()
        .success()
        .stdout(predicates::str::contains("daemon:   degraded (sink, deps)"));
}

#[tokio::test(flavor = "multi_thread")]
async fn status_reports_unavailable_daemon_without_failing_command() {
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home(home.path(), "http://127.0.0.1:9", ADMIN_KEY);

    acps_command()
        .env("HOME", home.path())
        .arg("status")
        .assert()
        .success()
        .stdout(predicates::str::contains("daemon:   unavailable"));
}

#[test]
fn agent_check_reports_no_runs_when_state_is_empty() {
    // Without successful installer_runs the check command should report the
    // expected native install step as missing without hitting the network.
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");

    acps_command_without_placebo()
        .env("HOME", tempdir.path())
        .args(["agent", "check"])
        .assert()
        .failure()
        .stdout(predicates::str::contains("install: not installed"));
}

#[test]
fn agent_check_format_json_reports_steps() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");

    let output = acps_command()
        .env("HOME", tempdir.path())
        .args(["agent", "check", "--format", "json"])
        .assert()
        .failure()
        .get_output()
        .stdout
        .clone();
    let body: Value = serde_json::from_slice(&output).expect("agent check json parses");
    assert_eq!(body["agent"], "opencode");
    assert_eq!(body["ok"], false);
    assert_eq!(body["steps"][0]["step"], "install");
    assert_eq!(body["steps"][0]["result"]["status"], "not_installed");
}

#[test]
fn agent_check_reports_missing_adapter_step() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), amp_config()).expect("config should be written");

    let state_path = default_state_path(tempdir.path());
    fs::create_dir_all(state_path.parent().expect("state parent dir"))
        .expect("state dir should be created");
    let store = StateStore::open(&state_path).expect("state should open");
    store.migrate().expect("migration should pass");
    store
        .append_installer_run(InstallerRunInput {
            agent_id: "amp",
            started_at: "2026-05-22T00:00:00.000000000Z",
            finished_at: Some("2026-05-22T00:00:01.000000000Z"),
            status: "ran",
            stdout: "",
            stderr: "",
            exit_status: Some(0),
            step: "harness",
            version: None,
            operation: INSTALLER_OPERATION_INSTALL,
            method: None,
            log_dir: None,
            apply_run_id: None,
        })
        .expect("seed harness row");
    drop(store);

    acps_command_without_placebo()
        .env("HOME", tempdir.path())
        .args(["agent", "check"])
        .assert()
        .failure()
        .stdout(predicates::str::contains("harness: unknown"))
        .stdout(predicates::str::contains("adapter: not installed"));
}

#[test]
fn installer_history_reports_empty_state_when_nothing_recorded() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    fs::create_dir_all(tempdir.path().join(".config/acp-stack"))
        .expect("config dir should be created");
    fs::write(
        tempdir.path().join(".config/acp-stack/acps-config.toml"),
        VALID_CONFIG,
    )
    .expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["installer", "history"])
        .assert()
        .success()
        .stdout(predicates::str::contains("no installer runs recorded"));
}

#[test]
fn installer_history_renders_rows_with_filter() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    fs::create_dir_all(tempdir.path().join(".config/acp-stack"))
        .expect("config dir should be created");
    fs::write(
        tempdir.path().join(".config/acp-stack/acps-config.toml"),
        VALID_CONFIG,
    )
    .expect("config should be written");

    let state_path = default_state_path(tempdir.path());
    fs::create_dir_all(state_path.parent().expect("state parent dir"))
        .expect("state dir should be created");
    let store = StateStore::open(&state_path).expect("state should open");
    store.migrate().expect("migration should pass");
    store
        .append_installer_run(InstallerRunInput {
            agent_id: "opencode",
            started_at: "2026-05-22T00:00:00.000000000Z",
            finished_at: Some("2026-05-22T00:00:00.250000000Z"),
            status: "ran",
            stdout: "",
            stderr: "",
            exit_status: Some(0),
            step: "harness",
            version: Some("v1.0.0"),
            operation: INSTALLER_OPERATION_INSTALL,
            method: Some(INSTALLER_METHOD_GITHUB),
            log_dir: None,
            apply_run_id: None,
        })
        .expect("seed harness row");
    store
        .append_installer_run(InstallerRunInput {
            agent_id: "codex",
            started_at: "2026-05-22T00:00:01.000000000Z",
            finished_at: Some("2026-05-22T00:00:02.000000000Z"),
            status: "failed",
            stdout: "",
            stderr: "boom",
            exit_status: Some(2),
            step: "adapter",
            version: None,
            operation: INSTALLER_OPERATION_INSTALL,
            method: Some(INSTALLER_METHOD_GITHUB),
            log_dir: None,
            apply_run_id: None,
        })
        .expect("seed adapter row");
    drop(store);

    // No filter: both rows visible, newest (codex) first.
    acps_command()
        .env("HOME", tempdir.path())
        .args(["installer", "history"])
        .assert()
        .success()
        .stdout(predicates::str::contains("started_at"))
        .stdout(predicates::str::contains("codex"))
        .stdout(predicates::str::contains("opencode"))
        .stdout(predicates::str::contains("v1.0.0"))
        .stdout(predicates::str::contains("failed"));

    // Filter to opencode: only the harness row should appear.
    acps_command()
        .env("HOME", tempdir.path())
        .args(["installer", "history", "--agent", "opencode"])
        .assert()
        .success()
        .stdout(predicates::str::contains("opencode"))
        .stdout(predicates::str::contains("v1.0.0"))
        .stdout(predicates::str::contains("codex").not());
}

#[test]
fn installer_history_format_json_renders_runs() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    fs::create_dir_all(tempdir.path().join(".config/acp-stack"))
        .expect("config dir should be created");
    fs::write(
        tempdir.path().join(".config/acp-stack/acps-config.toml"),
        VALID_CONFIG,
    )
    .expect("config should be written");

    let state_path = default_state_path(tempdir.path());
    fs::create_dir_all(state_path.parent().expect("state parent dir"))
        .expect("state dir should be created");
    let store = StateStore::open(&state_path).expect("state should open");
    store.migrate().expect("migration should pass");
    store
        .append_installer_run(InstallerRunInput {
            agent_id: "opencode",
            started_at: "2026-05-22T01:00:00.000000000Z",
            finished_at: Some("2026-05-22T01:00:01.000000000Z"),
            status: "ran",
            stdout: "hi",
            stderr: "",
            exit_status: Some(0),
            step: "harness",
            version: Some("v1.0.0"),
            operation: INSTALLER_OPERATION_INSTALL,
            method: Some(INSTALLER_METHOD_GITHUB),
            log_dir: Some("/tmp/installer-logs/opencode/harness"),
            apply_run_id: None,
        })
        .expect("seed row");
    drop(store);

    let output = acps_command()
        .env("HOME", tempdir.path())
        .args(["installer", "history", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let body: Value = serde_json::from_slice(&output).expect("installer json parses");
    let runs = body["runs"].as_array().expect("runs should be an array");
    assert_eq!(runs.len(), 1, "{body}");
    assert_eq!(runs[0]["agent_id"], "opencode");
    assert_eq!(runs[0]["duration_ms"], 1_000);
}

#[test]
fn installer_history_renders_log_dir_continuation_line() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    fs::create_dir_all(tempdir.path().join(".config/acp-stack"))
        .expect("config dir should be created");
    fs::write(
        tempdir.path().join(".config/acp-stack/acps-config.toml"),
        VALID_CONFIG,
    )
    .expect("config should be written");

    let state_path = default_state_path(tempdir.path());
    fs::create_dir_all(state_path.parent().expect("state parent dir"))
        .expect("state dir should be created");
    let store = StateStore::open(&state_path).expect("state should open");
    store.migrate().expect("migration should pass");
    store
        .append_installer_run(InstallerRunInput {
            agent_id: "opencode",
            started_at: "2026-05-22T01:00:00.000000000Z",
            finished_at: Some("2026-05-22T01:00:01.000000000Z"),
            status: "ran",
            stdout: "hi",
            stderr: "",
            exit_status: Some(0),
            step: "harness",
            version: Some("v1.0.0"),
            operation: INSTALLER_OPERATION_INSTALL,
            method: Some(INSTALLER_METHOD_GITHUB),
            log_dir: Some("/tmp/installer-logs/opencode/2026-05-22T01:00:00.000000000Z/harness"),
            apply_run_id: None,
        })
        .expect("seed row with log_dir");
    drop(store);

    acps_command()
        .env("HOME", tempdir.path())
        .args(["installer", "history"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "log_dir: /tmp/installer-logs/opencode/2026-05-22T01:00:00.000000000Z/harness",
        ));
}

#[test]
fn installer_history_rejects_zero_limit() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    fs::create_dir_all(tempdir.path().join(".config/acp-stack"))
        .expect("config dir should be created");
    fs::write(
        tempdir.path().join(".config/acp-stack/acps-config.toml"),
        VALID_CONFIG,
    )
    .expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["installer", "history", "--limit", "0"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("limit must be"));
}

#[test]
fn deps_apply_prints_before_and_after_status() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");

    let dependency_one_name = "deps-apply-before-after-marker-one";
    let dependency_two_name = "deps-apply-before-after-marker-two";
    let feature = "deps-apply-before-after";
    let marker_one = tempdir.path().join("deps-apply-marker-one");
    let marker_two = tempdir.path().join("deps-apply-marker-two");
    let shell_one = format!(
        "printf '#!/bin/sh\\nexit 0\\n' > {marker} && chmod 755 {marker}",
        marker = shell_quote_path(&marker_one),
    );
    let shell_two = format!(
        "printf '#!/bin/sh\\nexit 0\\n' > {marker} && chmod 755 {marker}",
        marker = shell_quote_path(&marker_two),
    );
    let config = VALID_CONFIG.replace(
        "[agent]",
        &format!(
            r#"[[dependencies.commands]]
	name = "{dependency_one_name}"
	required = true
	feature = "{feature}"
	
	[dependencies.commands.install]
	shell = {}
	creates = {}

[[dependencies.commands]]
	name = "{dependency_two_name}"
	required = true
	feature = "{feature}"
	
	[dependencies.commands.install]
	shell = {}
	creates = {}
	
	[agent]"#,
            toml_string(&shell_one),
            toml_string(&marker_one.to_string_lossy()),
            toml_string(&shell_two),
            toml_string(&marker_two.to_string_lossy()),
        ),
    );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    let state_path = default_state_path(tempdir.path());
    fs::create_dir_all(state_path.parent().expect("state parent dir"))
        .expect("state dir should be created");
    let store = StateStore::open(&state_path).expect("state should open");
    store.migrate().expect("migration should pass");
    drop(store);
    seed_auth_verifiers(tempdir.path(), SESSION_KEY, ADMIN_KEY);

    let output = acps_command()
        .env("HOME", tempdir.path())
        .args([
            "deps",
            "apply",
            "--yes",
            "--feature",
            feature,
            "--admin-key",
            ADMIN_KEY,
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("stdout should be utf8");

    let before_index = stdout.find("before:\n").expect("before section");
    let progress_one_index = stdout
        .find(&format!(
            "progress: applying dependency 1/2: {dependency_one_name}\n"
        ))
        .expect("first progress line");
    let progress_two_index = stdout
        .find(&format!(
            "progress: applying dependency 2/2: {dependency_two_name}\n"
        ))
        .expect("second progress line");
    let results_index = stdout.find("results:\n").expect("results section");
    let after_index = stdout.find("after:\n").expect("after section");
    let audit_index = stdout.find("audit run: dap_").expect("audit run line");
    assert!(
        progress_one_index < progress_two_index
            && progress_two_index < before_index
            && before_index < results_index
            && results_index < after_index
            && after_index < audit_index,
        "expected before/results/after ordering, got:\n{stdout}",
    );
    assert!(
        stdout[before_index..results_index].contains(&format!("  MISS {dependency_one_name}")),
        "before section must report missing dependency, got:\n{stdout}",
    );
    assert!(
        stdout[before_index..results_index].contains(&format!("  MISS {dependency_two_name}")),
        "before section must report missing dependency, got:\n{stdout}",
    );
    assert!(
        stdout[after_index..].contains(&format!("  OK   {dependency_one_name}")),
        "after section must report available dependency, got:\n{stdout}",
    );
    assert!(
        stdout[after_index..].contains(&format!("  OK   {dependency_two_name}")),
        "after section must report available dependency, got:\n{stdout}",
    );
}

#[test]
fn deps_apply_requires_admin_key() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["deps", "apply", "--yes"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--admin-key"));
}

#[test]
fn deps_check_format_json_reports_dependency_shape() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");

    let config = VALID_CONFIG.replace(
        "[agent]",
        r#"[[dependencies.commands]]
name = "deps-check-json"
required = true

[agent]"#,
    );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    let output = acps_command()
        .env("HOME", tempdir.path())
        .args(["deps", "check", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let body: Value = serde_json::from_slice(&output).expect("deps check json parses");
    let deps = body["dependencies"]
        .as_array()
        .expect("dependencies should be an array");
    assert_eq!(deps[0]["name"], "deps-check-json");
    assert_eq!(deps[0]["available"], false);
}

#[test]
fn deps_apply_format_json_omits_stderr_tail() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");

    let marker = tempdir.path().join("deps-apply-failed-marker");
    let config = VALID_CONFIG.replace(
        "[agent]",
        &format!(
            r#"[[dependencies.commands]]
name = "deps-apply-json-failure"
required = true

[dependencies.commands.install]
shell = "printf 'token sk-test-secret' >&2; exit 7"
creates = {}

[agent]"#,
            toml_string(&marker.to_string_lossy()),
        ),
    );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    let state_path = default_state_path(tempdir.path());
    fs::create_dir_all(state_path.parent().expect("state parent dir"))
        .expect("state dir should be created");
    let store = StateStore::open(&state_path).expect("state should open");
    store.migrate().expect("migration should pass");
    drop(store);
    seed_auth_verifiers(tempdir.path(), SESSION_KEY, ADMIN_KEY);

    let output = acps_command()
        .env("HOME", tempdir.path())
        .args([
            "deps",
            "apply",
            "--yes",
            "--format",
            "json",
            "--admin-key",
            ADMIN_KEY,
        ])
        .assert()
        .failure()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8_lossy(&output);
    assert!(!stdout.contains("running dependency install actions"));
    assert!(!stdout.contains("progress: applying dependency"));
    assert!(!stdout.contains("sk-test-secret"));
    let body: Value = serde_json::from_slice(&output).expect("deps apply json parses");
    assert!(
        body["apply_run_id"]
            .as_str()
            .is_some_and(|value| value.starts_with("dap_")),
        "{body}",
    );
    let outcome = &body["results"][0]["outcome"];
    assert_eq!(outcome["kind"], "failed");
    assert_eq!(outcome["exit_code"], 7);
    assert_eq!(outcome["stderr_tail_omitted"], true);
    assert!(outcome.get("stderr_tail").is_none(), "{body}");
}

#[test]
fn deps_apply_persists_one_apply_run_id_for_all_rows() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");

    let installed_marker = tempdir.path().join("deps-apply-installed-marker");
    let skipped_marker = tempdir.path().join("deps-apply-skipped-marker");
    fs::write(&skipped_marker, "#!/bin/sh\nexit 0\n").expect("skipped marker should be written");
    #[cfg(unix)]
    fs::set_permissions(&skipped_marker, fs::Permissions::from_mode(0o755))
        .expect("skipped marker should be executable");
    let shell = format!(
        "printf '#!/bin/sh\\nexit 0\\n' > {marker} && chmod 755 {marker}",
        marker = shell_quote_path(&installed_marker),
    );
    let config = VALID_CONFIG.replace(
        "[agent]",
        &format!(
            r#"[[dependencies.commands]]
name = "deps-apply-installed"
required = true

[dependencies.commands.install]
shell = {}
creates = {}

[[dependencies.commands]]
name = "deps-apply-skipped"
required = true

[dependencies.commands.install]
shell = "exit 99"
creates = {}

[agent]"#,
            toml_string(&shell),
            toml_string(&installed_marker.to_string_lossy()),
            toml_string(&skipped_marker.to_string_lossy()),
        ),
    );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    let state_path = default_state_path(tempdir.path());
    fs::create_dir_all(state_path.parent().expect("state parent dir"))
        .expect("state dir should be created");
    let store = StateStore::open(&state_path).expect("state should open");
    store.migrate().expect("migration should pass");
    drop(store);
    seed_auth_verifiers(tempdir.path(), SESSION_KEY, ADMIN_KEY);

    acps_command()
        .env("HOME", tempdir.path())
        .args(["deps", "apply", "--yes", "--admin-key", ADMIN_KEY])
        .assert()
        .success();

    let store = StateStore::open(&state_path).expect("state should open");
    let rows = store
        .query_installer_runs_filtered(Some("deps_apply"), 10)
        .expect("deps rows should query");
    assert_eq!(
        rows.len(),
        2,
        "expected one row per declared install action"
    );
    let apply_run_id = rows[0]
        .apply_run_id
        .as_deref()
        .expect("apply_run_id should be present");
    assert!(
        apply_run_id.starts_with("dap_"),
        "apply_run_id should use the deps apply prefix, got {apply_run_id}"
    );
    assert!(
        rows.iter()
            .all(|row| row.apply_run_id.as_deref() == Some(apply_run_id)),
        "all rows from one invocation must share apply_run_id, got {rows:?}"
    );
    assert!(rows.iter().any(|row| row.status == "installed"));
    assert!(rows.iter().any(|row| row.status == "skipped"));
}

#[test]
fn agent_status_surfaces_installed_versions_from_state() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");

    // Seed installer_runs rows so `acps agent status` surfaces the versions.
    // The latest-successful query buckets by `step`, so a 'harness' row with
    // a recorded version and an 'adapter' row without a version exercise both
    // the "show version" and "version unknown" branches of the surface.
    let state_path = default_state_path(tempdir.path());
    fs::create_dir_all(state_path.parent().expect("state parent dir"))
        .expect("state dir should be created");
    let store = StateStore::open(&state_path).expect("state should open");
    store.migrate().expect("migration should pass");
    store
        .upsert_agent_capabilities(
            "opencode",
            r#"{"protocol_version":1,"capabilities":{"loadSession":true},"agent_name":"opencode","agent_title":"OpenCode","agent_version":"1.15.10"}"#,
        )
        .expect("capability row should append");
    store
        .append_installer_run(InstallerRunInput {
            agent_id: "opencode",
            started_at: "2026-05-21T00:00:00.000000000Z",
            finished_at: Some("2026-05-21T00:00:01.000000000Z"),
            status: "ran",
            stdout: "",
            stderr: "",
            exit_status: Some(0),
            step: "install",
            version: Some("1.15.10"),
            operation: INSTALLER_OPERATION_INSTALL,
            method: Some(INSTALLER_METHOD_NPM),
            log_dir: None,
            apply_run_id: None,
        })
        .expect("install row should append");
    store
        .append_installer_run(InstallerRunInput {
            agent_id: "opencode",
            started_at: "2026-05-21T00:00:02.000000000Z",
            finished_at: Some("2026-05-21T00:00:03.000000000Z"),
            status: "ran",
            stdout: "",
            stderr: "",
            exit_status: Some(0),
            step: "harness",
            version: Some("v1.2.3"),
            operation: INSTALLER_OPERATION_INSTALL,
            method: Some(INSTALLER_METHOD_GITHUB),
            log_dir: None,
            apply_run_id: None,
        })
        .expect("harness row should append");
    store
        .append_installer_run(InstallerRunInput {
            agent_id: "opencode",
            started_at: "2026-05-21T00:00:04.000000000Z",
            finished_at: Some("2026-05-21T00:00:05.000000000Z"),
            status: "ran",
            stdout: "",
            stderr: "",
            exit_status: Some(0),
            step: "adapter",
            version: None,
            operation: INSTALLER_OPERATION_INSTALL,
            method: Some(INSTALLER_METHOD_GITHUB),
            log_dir: None,
            apply_run_id: None,
        })
        .expect("adapter row should append");
    drop(store);

    acps_command()
        .env("HOME", tempdir.path())
        .args(["agent", "status"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent version: 1.15.10"))
        .stdout(predicates::str::contains("harness version: v1.2.3"))
        .stdout(predicates::str::contains(
            "adapter version: version unknown",
        ))
        .stdout(predicates::str::contains("ACP version: 1"));
}

#[test]
fn agent_status_format_json_omits_lifecycle_payloads() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");

    let state_path = default_state_path(tempdir.path());
    fs::create_dir_all(state_path.parent().expect("state parent dir"))
        .expect("state dir should be created");
    let store = StateStore::open(&state_path).expect("state should open");
    store.migrate().expect("migration should pass");
    store
        .append_agent_lifecycle(
            "agent.failed",
            "agent failed",
            r#"{"reason":"token sk-test-secret"}"#,
        )
        .expect("lifecycle row should append");
    drop(store);

    let output = acps_command()
        .env("HOME", tempdir.path())
        .args(["agent", "status", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let body: Value = serde_json::from_slice(&output).expect("agent status json parses");
    let lifecycle = body["recent_lifecycle"]
        .as_array()
        .expect("recent_lifecycle is an array");
    assert_eq!(lifecycle.len(), 1, "{body}");
    assert!(lifecycle[0].get("payload").is_none(), "{body}");
    assert!(!String::from_utf8_lossy(&output).contains("sk-test-secret"));
}

#[test]
fn agent_test_succeeds_with_prompt() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    write_fake_agent_home(tempdir.path(), &[]);

    acps_command()
        .env("HOME", tempdir.path())
        .args(["agent", "test", "--prompt", "hello from cli"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent test: ok"))
        .stdout(predicates::str::contains("agent: placebo"))
        .stdout(predicates::str::contains("prompt: provided"))
        .stdout(predicates::str::contains("session_id: sess_fake_0"))
        .stdout(predicates::str::contains("stop_reason: end_turn"))
        .stdout(predicates::str::contains("updates: 2"));
}

#[test]
fn agent_test_uses_default_prompt_when_omitted() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    write_fake_agent_home(tempdir.path(), &[]);

    acps_command()
        .env("HOME", tempdir.path())
        .args(["agent", "test"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent test: ok"))
        .stdout(predicates::str::contains("agent: placebo"))
        .stdout(predicates::str::contains("prompt: default"))
        .stdout(predicates::str::contains("stop_reason: end_turn"));
}

#[test]
fn agent_test_applies_configured_model_before_prompt() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    write_fake_agent_home(
        tempdir.path(),
        &[
            "--model-config-option",
            "openai/gpt-5.5",
            "--expect-model-config",
            "openai/gpt-5.5",
        ],
    );
    let config_path = tempdir.path().join(".config/acp-stack/acps-config.toml");
    let config = fs::read_to_string(&config_path).expect("config should be readable");
    fs::write(
        &config_path,
        config.replace(
            r#"restart = "on-crash""#,
            "restart = \"on-crash\"\nmodel = \"openai/gpt-5.5\"",
        ),
    )
    .expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["agent", "test", "--prompt", "hello"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent test: ok"));
}

#[test]
fn agent_test_reports_initialize_failure_stage() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    write_fake_agent_home(tempdir.path(), &["--initialize-error"]);

    acps_command()
        .env("HOME", tempdir.path())
        .args(["agent", "test", "--prompt", "hello"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "agent test failed at ACP initialize",
        ))
        .stderr(predicates::str::contains("fake initialize failure"));
}

#[test]
fn agent_test_reports_session_creation_failure_stage() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    write_fake_agent_home(tempdir.path(), &["--session-new-error"]);

    acps_command()
        .env("HOME", tempdir.path())
        .args(["agent", "test", "--prompt", "hello"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "agent test failed at session creation",
        ))
        .stderr(predicates::str::contains("fake session/new failure"));
}

#[test]
fn agent_test_reports_prompt_failure_stage() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    write_fake_agent_home(tempdir.path(), &["--prompt-error"]);

    // Phase 2 sanitization: the prompt-failure path now drops the raw upstream
    // message (which could embed URLs, headers, or secrets) and surfaces a
    // fixed `"prompt request failed"` string instead. Assert on the sanitized
    // form rather than the agent-supplied text.
    acps_command()
        .env("HOME", tempdir.path())
        .args(["agent", "test", "--prompt", "hello"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "agent test failed at prompt completion",
        ))
        .stderr(predicates::str::contains("prompt request failed"));
}

#[test]
fn agent_test_reports_progress_timeout_after_stall() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    write_fake_agent_home(tempdir.path(), &["--prompt-stall-after-update"]);

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "agent",
            "test",
            "--prompt",
            "hello",
            "--progress-timeout",
            "50ms",
            "--timeout",
            "2s",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "agent test failed at prompt/progress timeout",
        ))
        .stderr(predicates::str::contains(
            "no new session/update or terminal prompt response within 50ms",
        ));
}

#[test]
fn agent_status_reports_provider_with_unset_model_and_mode() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = format!("{}\n[agent.provider]\nid = \"openai\"\n", codex_config());
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["agent", "status"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent: codex"))
        .stdout(predicates::str::contains("provider: openai"))
        .stdout(predicates::str::contains("model and mode unset"))
        .stdout(predicates::str::contains("unavailable").not());
}

#[test]
fn agent_status_reports_all_configured_params() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = VALID_CONFIG.replace(
        r#"restart = "on-crash""#,
        r#"restart = "on-crash"
mode = "build"

[agent.provider]
id = "opencode-go"
model = "deepseek-v4-pro"
api_key_ref = "OPENCODE_API_KEY""#,
    );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["agent", "status"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent: opencode"))
        .stdout(predicates::str::contains("provider: opencode-go"))
        .stdout(predicates::str::contains("model: deepseek-v4-pro"))
        .stdout(predicates::str::contains("mode: build"))
        .stdout(predicates::str::contains(" unset").not())
        .stdout(predicates::str::contains(" unavailable").not());
}

#[test]
fn agent_status_reports_model_only_agent_params() {
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
            r#"restart = "on-crash""#,
            r#"restart = "on-crash"
model = "gpt-5.5""#,
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
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["agent", "status"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent: cursor"))
        .stdout(predicates::str::contains("model: gpt-5.5"))
        .stdout(predicates::str::contains("mode unset"))
        .stdout(predicates::str::contains("provider unavailable"));
}

#[test]
fn agent_status_reports_amp_unavailable_provider_and_model() {
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
            r#"restart = "on-crash""#,
            r#"restart = "on-crash"
mode = "smart""#,
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
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["agent", "status"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent: amp"))
        .stdout(predicates::str::contains("mode: smart"))
        .stdout(predicates::str::contains("provider and model unavailable"));
}

#[test]
fn agent_status_reports_all_supported_params_unset() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["agent", "status"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent: opencode"))
        .stdout(predicates::str::contains("provider, model, and mode unset"))
        .stdout(predicates::str::contains("unavailable").not());
}

#[tokio::test(flavor = "multi_thread")]
async fn agent_start_and_stop_call_running_daemon() {
    let harness = AgentCliHarness::spawn().await;
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home_with_socket(
        home.path(),
        &harness.base_url,
        ADMIN_KEY,
        Some(&harness.socket_path),
    );

    acps_command()
        .env("HOME", home.path())
        .args(["agent", "start", "--admin-key", ADMIN_KEY])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent start: running"))
        .stdout(predicates::str::contains("pid: "));

    let output = acps_command()
        .env("HOME", home.path())
        .args([
            "agent",
            "restart",
            "--format",
            "json",
            "--admin-key",
            ADMIN_KEY,
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let body: Value = serde_json::from_slice(&output).expect("restart json parses");
    assert!(body["started_at"].as_str().is_some(), "{body}");
    assert!(body["stopped_at"].as_str().is_some(), "{body}");
    assert!(body["capabilities"].is_object(), "{body}");

    acps_command()
        .env("HOME", home.path())
        .args(["agent", "stop", "--admin-key", ADMIN_KEY])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent stop: stopped"));
}

#[test]
fn agent_switch_noninteractive_requires_admin_key() {
    acps_command()
        .args(["agent", "switch", "opencode"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--admin-key"));
}

#[test]
fn agent_switch_accepts_drop_flag() {
    acps_command()
        .args(["agent", "switch", "opencode", "--drop"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--admin-key"))
        .stderr(predicates::str::contains("unexpected argument").not());
}

#[tokio::test(flavor = "multi_thread")]
async fn security_check_calls_running_daemon_without_auth_key() {
    let harness = AgentCliHarness::spawn().await;
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home_with_socket(
        home.path(),
        &harness.base_url,
        ADMIN_KEY,
        Some(&harness.socket_path),
    );

    acps_command()
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
    write_cli_home_with_socket(
        home.path(),
        &harness.base_url,
        ADMIN_KEY,
        Some(&harness.socket_path),
    );

    acps_command()
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

#[test]
fn security_check_does_not_accept_admin_key_flag() {
    acps_command()
        .args(["security", "check", "--admin-key", SESSION_KEY])
        .assert()
        .failure()
        .stderr(predicates::str::contains("unexpected argument"))
        .stderr(predicates::str::contains("--admin-key"));
}

#[tokio::test(flavor = "multi_thread")]
async fn security_history_renders_table_and_next_page_cursor() {
    let harness = AgentCliHarness::spawn().await;
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home_with_socket(
        home.path(),
        &harness.base_url,
        ADMIN_KEY,
        Some(&harness.socket_path),
    );

    let _first_run_id = run_security_check_and_extract_run_id(home.path());
    let second_run_id = run_security_check_and_extract_run_id(home.path());

    acps_command()
        .env("HOME", home.path())
        .args([
            "security",
            "history",
            "--limit",
            "1",
            "--admin-key",
            ADMIN_KEY,
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("id"))
        .stdout(predicates::str::contains("started_at"))
        .stdout(predicates::str::contains("status"))
        .stdout(predicates::str::contains("crit"))
        .stdout(predicates::str::contains("warn"))
        .stdout(predicates::str::contains("auth"))
        .stdout(predicates::str::contains("srun_"))
        .stdout(predicates::str::contains(second_run_id.as_str()))
        .stdout(predicates::str::contains("failed").or(predicates::str::contains("succeeded")))
        .stdout(predicates::str::contains("next page: --after "));
}

#[tokio::test(flavor = "multi_thread")]
async fn security_history_json_renders_runs_and_cursor() {
    let harness = AgentCliHarness::spawn().await;
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home_with_socket(
        home.path(),
        &harness.base_url,
        ADMIN_KEY,
        Some(&harness.socket_path),
    );

    let _first_run_id = run_security_check_and_extract_run_id(home.path());
    let second_run_id = run_security_check_and_extract_run_id(home.path());

    let output = acps_command()
        .env("HOME", home.path())
        .args([
            "security",
            "history",
            "--limit",
            "1",
            "--json",
            "--admin-key",
            ADMIN_KEY,
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let body: Value = serde_json::from_slice(&output).expect("history json should parse");
    let runs = body["runs"].as_array().expect("runs should be an array");
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["id"], second_run_id);
    assert!(
        body["next_cursor"].as_str().is_some(),
        "full first page should include a next cursor: {body}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn security_history_global_format_json_matches_json_alias() {
    let harness = AgentCliHarness::spawn().await;
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home_with_socket(
        home.path(),
        &harness.base_url,
        ADMIN_KEY,
        Some(&harness.socket_path),
    );

    let run_id = run_security_check_and_extract_run_id(home.path());

    let output = acps_command()
        .env("HOME", home.path())
        .args([
            "security",
            "history",
            "--format",
            "json",
            "--admin-key",
            ADMIN_KEY,
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let body: Value = serde_json::from_slice(&output).expect("history json should parse");
    let runs = body["runs"].as_array().expect("runs should be an array");
    assert!(runs.iter().any(|run| run["id"] == run_id), "{body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn security_history_json_alias_conflicts_with_explicit_text_format() {
    let harness = AgentCliHarness::spawn().await;
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home_with_socket(
        home.path(),
        &harness.base_url,
        ADMIN_KEY,
        Some(&harness.socket_path),
    );

    acps_command()
        .env("HOME", home.path())
        .args(["security", "history", "--json", "--format", "text"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "--json conflicts with --format text",
        ));
}

#[test]
fn security_history_json_alias_conflict_precedes_config_load() {
    let home = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", home.path())
        .args(["security", "history", "--json", "--format", "text"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "--json conflicts with --format text",
        ));
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn security_show_renders_run_findings_hints_and_details() {
    let harness = AgentCliHarness::spawn().await;
    std::fs::set_permissions(&harness.state_path, fs::Permissions::from_mode(0o644))
        .expect("loosen state db mode");
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home_with_socket(
        home.path(),
        &harness.base_url,
        ADMIN_KEY,
        Some(&harness.socket_path),
    );

    let run_id = run_security_check_and_extract_run_id(home.path());

    acps_command()
        .env("HOME", home.path())
        .args(["security", "show", &run_id, "--admin-key", ADMIN_KEY])
        .assert()
        .success()
        .stdout(predicates::str::contains(format!("run_id: {run_id}")))
        .stdout(predicates::str::contains("started_at:"))
        .stdout(predicates::str::contains("finished_at:"))
        .stdout(predicates::str::contains("status:"))
        .stdout(predicates::str::contains("critical:"))
        .stdout(predicates::str::contains("warning:"))
        .stdout(predicates::str::contains("runtime.path_mode_loose"))
        .stdout(predicates::str::contains("    hint: "))
        .stdout(predicates::str::contains("    details: "))
        .stdout(predicates::str::contains("\"path\""))
        .stdout(predicates::str::contains("\"kind\""));
}

#[test]
fn security_show_rejects_invalid_run_id_before_daemon_request() {
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home(home.path(), "http://127.0.0.1:9", ADMIN_KEY);

    acps_command()
        .env("HOME", home.path())
        .args(["security", "show", "srun/not-safe"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("expected an alphanumeric run id"))
        .stderr(predicates::str::contains("--admin-key").not())
        .stderr(predicates::str::contains("/v1/security/history").not());
}

#[test]
fn security_history_rejects_invalid_limit_before_admin_key() {
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home(home.path(), "http://127.0.0.1:9", ADMIN_KEY);

    acps_command()
        .env("HOME", home.path())
        .args(["security", "history", "--limit", "0"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("limit must be"))
        .stderr(predicates::str::contains("--admin-key").not())
        .stderr(predicates::str::contains("/v1/security/history").not());
}

#[tokio::test(flavor = "multi_thread")]
async fn security_history_uses_admin_key_not_session_key() {
    let harness = AgentCliHarness::spawn().await;
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home_with_socket(
        home.path(),
        &harness.base_url,
        ADMIN_KEY,
        Some(&harness.socket_path),
    );

    acps_command()
        .env("HOME", home.path())
        .args(["security", "history", "--admin-key", SESSION_KEY])
        .assert()
        .failure()
        .stderr(predicates::str::contains("/v1/security/history"))
        .stderr(predicates::str::contains("401"));
}

#[tokio::test(flavor = "multi_thread")]
async fn security_show_uses_admin_key_not_session_key() {
    let harness = AgentCliHarness::spawn().await;
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home_with_socket(
        home.path(),
        &harness.base_url,
        ADMIN_KEY,
        Some(&harness.socket_path),
    );

    acps_command()
        .env("HOME", home.path())
        .args([
            "security",
            "show",
            "srun_does_not_exist",
            "--admin-key",
            SESSION_KEY,
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("/v1/security/history/{run_id}"))
        .stderr(predicates::str::contains("401"));
}

fn run_security_check_and_extract_run_id(home: &std::path::Path) -> String {
    let output = acps_command()
        .env("HOME", home)
        .args(["security", "check"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("security check stdout should be utf8");
    stdout
        .lines()
        .find_map(|line| line.strip_prefix("run_id: "))
        .expect("security check should print run_id")
        .trim()
        .to_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn metrics_summary_format_json_returns_summary() {
    let harness = AgentCliHarness::spawn().await;
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home_with_socket(
        home.path(),
        &harness.base_url,
        ADMIN_KEY,
        Some(&harness.socket_path),
    );

    let output = acps_command()
        .env("HOME", home.path())
        .args(["metrics", "summary", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let body: Value = serde_json::from_slice(&output).expect("metrics json parses");
    assert!(body["counts"].is_object(), "{body}");
    assert!(body["window"].is_object(), "{body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn ws_common_commands_format_json() {
    let harness = AgentCliHarness::spawn().await;
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home_with_socket(
        home.path(),
        &harness.base_url,
        ADMIN_KEY,
        Some(&harness.socket_path),
    );

    let connections_output = acps_command()
        .env("HOME", home.path())
        .args(["ws", "connections", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let connections_body: Value =
        serde_json::from_slice(&connections_output).expect("connections json parses");
    assert!(
        connections_body["connections"].as_array().is_some(),
        "{connections_body}",
    );

    let sessions_output = acps_command()
        .env("HOME", home.path())
        .args(["ws", "sessions", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let sessions_body: Value =
        serde_json::from_slice(&sessions_output).expect("sessions json parses");
    assert!(
        sessions_body["sessions"].as_array().is_some(),
        "{sessions_body}"
    );

    let disconnect_output = acps_command()
        .env("HOME", home.path())
        .args([
            "ws",
            "disconnect",
            "--connection-id",
            "missing",
            "--format",
            "json",
            "--admin-key",
            ADMIN_KEY,
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let disconnect_body: Value =
        serde_json::from_slice(&disconnect_output).expect("disconnect json parses");
    assert_eq!(disconnect_body["requested"], 0);
}

#[test]
fn ws_disconnect_requires_target_before_admin_key() {
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home(home.path(), "http://127.0.0.1:9", ADMIN_KEY);

    acps_command()
        .env("HOME", home.path())
        .args(["ws", "disconnect"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--connection-id or --session-id"))
        .stderr(predicates::str::contains("--admin-key").not());
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_new_list_prompt_close_round_trip() {
    let harness = AgentCliHarness::spawn().await;
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home_with_socket(
        home.path(),
        &harness.base_url,
        ADMIN_KEY,
        Some(&harness.socket_path),
    );

    // Start the agent first so /v1/sessions has a live ACP connection.
    acps_command()
        .env("HOME", home.path())
        .args(["agent", "start", "--admin-key", ADMIN_KEY])
        .assert()
        .success();

    let new_output = acps_command()
        .env("HOME", home.path())
        .args(["sessions", "new", "--session-key", SESSION_KEY])
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

    acps_command()
        .env("HOME", home.path())
        .args(["sessions", "list"])
        .assert()
        .success()
        .stdout(predicates::str::contains(session_id.as_str()));

    acps_command()
        .env("HOME", home.path())
        .args([
            "sessions",
            "prompt",
            &session_id,
            "hello",
            "--session-key",
            SESSION_KEY,
        ])
        .timeout(std::time::Duration::from_secs(30))
        .assert()
        .success()
        .stdout(predicates::str::contains("prompt: completed"))
        .stdout(predicates::str::contains("stop_reason: end_turn"));

    acps_command()
        .env("HOME", home.path())
        .args([
            "sessions",
            "close",
            &session_id,
            "--session-key",
            SESSION_KEY,
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("session close: closed"));
}

#[test]
fn sessions_mutating_commands_require_explicit_session_key() {
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home(home.path(), "http://127.0.0.1:9", ADMIN_KEY);

    for args in [
        vec!["sessions", "new"],
        vec!["sessions", "fork", "sess_test"],
        vec!["sessions", "prompt", "sess_test", "hello"],
        vec!["sessions", "cancel", "sess_test"],
        vec!["sessions", "close", "sess_test"],
    ] {
        acps_command()
            .env("HOME", home.path())
            .env_remove("ACP_STACK_SESSION_KEY")
            .args(args)
            .assert()
            .failure()
            .stderr(predicates::str::contains(
                "--session-key or ACP_STACK_SESSION_KEY",
            ));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn auth_local_session_access_enable_and_disable_call_daemon() {
    let harness = AgentCliHarness::spawn().await;
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home_with_socket(
        home.path(),
        &harness.base_url,
        ADMIN_KEY,
        Some(&harness.socket_path),
    );

    acps_command()
        .env("HOME", home.path())
        .args([
            "auth",
            "local-session-access",
            "enable",
            "--admin-key",
            ADMIN_KEY,
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("local session access: keyless"));

    let daemon_config = fs::read_to_string(&harness.config_path).expect("daemon config");
    assert!(daemon_config.contains("session_auth = \"keyless\""));

    acps_command()
        .env("HOME", home.path())
        .args([
            "auth",
            "local-session-access",
            "disable",
            "--admin-key",
            ADMIN_KEY,
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "local session access: session-key",
        ));
}

#[test]
fn auth_local_session_access_status_reports_config() {
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home_with_socket_and_session_auth(
        home.path(),
        "http://127.0.0.1:9",
        ADMIN_KEY,
        None,
        Some("keyless"),
    );

    acps_command()
        .env("HOME", home.path())
        .args(["auth", "local-session-access", "status"])
        .assert()
        .success()
        .stdout(predicates::str::contains("local session access: keyless"));
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_new_uses_local_socket_without_key_when_enabled() {
    let harness = AgentCliHarness::spawn().await;
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home_with_socket_and_session_auth(
        home.path(),
        &harness.base_url,
        ADMIN_KEY,
        Some(&harness.socket_path),
        Some("keyless"),
    );

    acps_command()
        .env("HOME", home.path())
        .args([
            "auth",
            "local-session-access",
            "enable",
            "--admin-key",
            ADMIN_KEY,
        ])
        .assert()
        .success();

    acps_command()
        .env("HOME", home.path())
        .args(["agent", "start", "--admin-key", ADMIN_KEY])
        .assert()
        .success();

    acps_command()
        .env("HOME", home.path())
        .env_remove("ACP_STACK_SESSION_KEY")
        .args(["sessions", "new"])
        .assert()
        .success()
        .stdout(predicates::str::contains("session: "));

    acps_command()
        .env("HOME", home.path())
        .args([
            "auth",
            "local-session-access",
            "disable",
            "--admin-key",
            ADMIN_KEY,
        ])
        .assert()
        .success();
    write_cli_home_with_socket(
        home.path(),
        &harness.base_url,
        ADMIN_KEY,
        Some(&harness.socket_path),
    );

    acps_command()
        .env("HOME", home.path())
        .env_remove("ACP_STACK_SESSION_KEY")
        .args(["sessions", "new"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "--session-key or ACP_STACK_SESSION_KEY",
        ));
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_new_explicit_key_uses_public_api_even_when_local_keyless() {
    let harness = AgentCliHarness::spawn().await;
    let home = tempfile::tempdir().expect("tempdir should be created");
    let missing_socket = home.path().join("missing.sock");
    write_cli_home_with_socket_and_session_auth(
        home.path(),
        &harness.base_url,
        ADMIN_KEY,
        Some(&missing_socket),
        Some("keyless"),
    );

    acps_command()
        .env("HOME", home.path())
        .args(["agent", "start", "--admin-key", ADMIN_KEY])
        .assert()
        .success();

    acps_command()
        .env("HOME", home.path())
        .args(["sessions", "new", "--session-key", SESSION_KEY])
        .assert()
        .success()
        .stdout(predicates::str::contains("session: "));
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_new_format_json_returns_session_object() {
    let harness = AgentCliHarness::spawn().await;
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home_with_socket(
        home.path(),
        &harness.base_url,
        ADMIN_KEY,
        Some(&harness.socket_path),
    );

    acps_command()
        .env("HOME", home.path())
        .args(["agent", "start", "--admin-key", ADMIN_KEY])
        .assert()
        .success();

    let output = acps_command()
        .env("HOME", home.path())
        .args([
            "sessions",
            "new",
            "--format",
            "json",
            "--session-key",
            SESSION_KEY,
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let body: Value = serde_json::from_slice(&output).expect("session json parses");
    assert!(body["id"].as_str().is_some(), "{body}");
    assert!(body["cwd"].as_str().is_some(), "{body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_common_commands_format_json() {
    let harness = AgentCliHarness::spawn().await;
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home_with_socket(
        home.path(),
        &harness.base_url,
        ADMIN_KEY,
        Some(&harness.socket_path),
    );

    acps_command()
        .env("HOME", home.path())
        .args(["agent", "start", "--admin-key", ADMIN_KEY])
        .assert()
        .success();

    let new_output = acps_command()
        .env("HOME", home.path())
        .args([
            "sessions",
            "new",
            "--format",
            "json",
            "--session-key",
            SESSION_KEY,
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let new_body: Value = serde_json::from_slice(&new_output).expect("new json parses");
    let session_id = new_body["id"].as_str().expect("session id").to_owned();

    let list_output = acps_command()
        .env("HOME", home.path())
        .args(["sessions", "list", "--range", "all", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let list_body: Value = serde_json::from_slice(&list_output).expect("list json parses");
    assert_eq!(list_body["truncated"], false);
    assert!(
        list_body["sessions"]
            .as_array()
            .expect("sessions array")
            .iter()
            .any(|session| session["id"] == session_id),
        "{list_body}",
    );

    let status_output = acps_command()
        .env("HOME", home.path())
        .args(["sessions", "status", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let status_body: Value = serde_json::from_slice(&status_output).expect("status json parses");
    assert!(
        status_body["sessions"].as_array().is_some(),
        "{status_body}"
    );

    let prompt_output = acps_command()
        .env("HOME", home.path())
        .arg("sessions")
        .arg("prompt")
        .arg(&session_id)
        .arg("hello")
        .args([
            "--no-wait",
            "--format",
            "json",
            "--session-key",
            SESSION_KEY,
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let prompt_body: Value = serde_json::from_slice(&prompt_output).expect("prompt json parses");
    assert_eq!(prompt_body["status"], "pending");
    assert!(prompt_body["prompt_id"].as_str().is_some(), "{prompt_body}");

    let cancel_output = acps_command()
        .env("HOME", home.path())
        .arg("sessions")
        .arg("cancel")
        .arg(&session_id)
        .args(["--format", "json", "--session-key", SESSION_KEY])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let cancel_body: Value = serde_json::from_slice(&cancel_output).expect("cancel json parses");
    assert_eq!(cancel_body["status"], "requested");
    assert_eq!(cancel_body["session_id"], session_id);

    let close_session_output = acps_command()
        .env("HOME", home.path())
        .args([
            "sessions",
            "new",
            "--format",
            "json",
            "--session-key",
            SESSION_KEY,
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let close_session_body: Value =
        serde_json::from_slice(&close_session_output).expect("close session json parses");
    let close_session_id = close_session_body["id"]
        .as_str()
        .expect("close session id")
        .to_owned();

    let close_output = acps_command()
        .env("HOME", home.path())
        .arg("sessions")
        .arg("close")
        .arg(&close_session_id)
        .args(["--format", "json", "--session-key", SESSION_KEY])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let close_body: Value = serde_json::from_slice(&close_output).expect("close json parses");
    assert_eq!(close_body["id"], close_session_id);
    assert_eq!(close_body["status"], "closed");
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_status_reports_no_active_session() {
    let harness = AgentCliHarness::spawn().await;
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home_with_socket(
        home.path(),
        &harness.base_url,
        ADMIN_KEY,
        Some(&harness.socket_path),
    );

    acps_command()
        .env("HOME", home.path())
        .args(["sessions", "status"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "No session activity in window.\n",
        ));
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_status_renders_recent_active_session() {
    let harness = AgentCliHarness::spawn().await;
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home_with_socket(
        home.path(),
        &harness.base_url,
        ADMIN_KEY,
        Some(&harness.socket_path),
    );

    acps_command()
        .env("HOME", home.path())
        .args(["agent", "start", "--admin-key", ADMIN_KEY])
        .assert()
        .success();

    let new_output = acps_command()
        .env("HOME", home.path())
        .args(["sessions", "new", "--session-key", SESSION_KEY])
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

    acps_command()
        .env("HOME", home.path())
        .args(["sessions", "status"])
        .assert()
        .success()
        .stdout(predicates::str::contains("idle "))
        .stdout(predicates::str::contains("last_activity="))
        .stdout(predicates::str::contains("from=user"))
        .stdout(predicates::str::contains(session_id.as_str()));
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_status_format_json_returns_window() {
    let harness = AgentCliHarness::spawn().await;
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home_with_socket(
        home.path(),
        &harness.base_url,
        ADMIN_KEY,
        Some(&harness.socket_path),
    );

    let output = acps_command()
        .env("HOME", home.path())
        .args(["sessions", "status", "--window", "1m", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let body: Value = serde_json::from_slice(&output).expect("sessions status json parses");
    assert_eq!(body["window"], "1m");
    assert!(body["window_start"].is_string(), "{body}");
    assert!(body["sessions"].as_array().is_some(), "{body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_prompt_no_wait_returns_immediately() {
    let harness = AgentCliHarness::spawn().await;
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home_with_socket(
        home.path(),
        &harness.base_url,
        ADMIN_KEY,
        Some(&harness.socket_path),
    );

    acps_command()
        .env("HOME", home.path())
        .args(["agent", "start", "--admin-key", ADMIN_KEY])
        .assert()
        .success();

    let new_output = acps_command()
        .env("HOME", home.path())
        .args(["sessions", "new", "--session-key", SESSION_KEY])
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

    acps_command()
        .env("HOME", home.path())
        .args([
            "sessions",
            "prompt",
            &session_id,
            "ping",
            "--no-wait",
            "--session-key",
            SESSION_KEY,
        ])
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

    acps_command()
        .env("HOME", home.path())
        .args([
            "agent",
            "start",
            "--admin-key",
            "acps_admin_wrongwrongwrongwrongwrongwrongwrongwrongwrong",
        ])
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
    let config_path = config_dir.join("acps-config.toml");
    fs::write(&config_path, VALID_CONFIG).expect("config should be written");
    fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o755))
        .expect("config dir permissions should be set");
    fs::set_permissions(&config_path, fs::Permissions::from_mode(0o644))
        .expect("config file permissions should be set");

    acps_command()
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
    let config_path = config_dir.join("acps-config.toml");
    fs::write(
        &config_path,
        VALID_CONFIG.replace(r#"bind = "127.0.0.1:7700""#, r#"bind = "bad""#),
    )
    .expect("invalid config should be written");
    fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o755))
        .expect("config dir permissions should be set");
    fs::set_permissions(&config_path, fs::Permissions::from_mode(0o644))
        .expect("config file permissions should be set");

    acps_command()
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

    acps_command()
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .success();

    let mut command = acps_command();
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

    acps_command()
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

    acps_command()
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .success();
    acps_command()
        .env("HOME", tempdir.path())
        .arg("status")
        .assert()
        .success();

    let mut limit_command = acps_command();
    limit_command
        .env("HOME", tempdir.path())
        .args(["logs", "query", "--limit", "1"])
        .assert()
        .success()
        .stdout(predicates::str::contains("status.checked").count(1));

    let mut level_command = acps_command();
    level_command
        .env("HOME", tempdir.path())
        .args(["logs", "query", "--level", "error"])
        .assert()
        .success()
        .stdout("");
}

#[test]
fn logs_query_json_emits_envelope_with_cursor() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .success();
    acps_command()
        .env("HOME", tempdir.path())
        .arg("status")
        .assert()
        .success();

    let output = acps_command()
        .env("HOME", tempdir.path())
        .args(["logs", "query", "--limit", "1", "--json"])
        .output()
        .expect("acps logs query --json should execute");
    assert!(
        output.status.success(),
        "exit status: {:?}, stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("stdout must be valid JSON");
    let events = parsed
        .get("events")
        .and_then(|v| v.as_array())
        .expect("events array present");
    assert_eq!(events.len(), 1, "limit=1 must return exactly one event");
    let event = &events[0];
    for field in [
        "id",
        "created_at",
        "level",
        "kind",
        "message",
        "payload_json",
        "source",
    ] {
        assert!(
            event.get(field).is_some(),
            "event JSON missing field `{field}`: {event}"
        );
    }
    let cursor = parsed
        .get("next_cursor")
        .expect("next_cursor key present even when null")
        .as_str()
        .expect("next_cursor populated when page saturates limit");
    assert!(
        !cursor.is_empty(),
        "next_cursor must be a non-empty id when limit=1 saturates"
    );

    let stderr = String::from_utf8(output.stderr).expect("stderr is utf8");
    assert!(
        !stderr.contains("-- more rows available"),
        "JSON mode must suppress the human cursor hint, got: {stderr}"
    );
}

#[test]
fn logs_query_global_format_json_matches_json_alias() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .success();

    let output = acps_command()
        .env("HOME", tempdir.path())
        .args(["logs", "query", "--limit", "1", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let parsed: Value = serde_json::from_slice(&output).expect("format json should parse");
    assert!(parsed["events"].as_array().is_some(), "{parsed}");
    assert!(parsed.get("next_cursor").is_some(), "{parsed}");
}

#[test]
fn logs_query_json_alias_conflicts_with_explicit_text_format() {
    acps_command()
        .args(["logs", "query", "--json", "--format", "text"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "--json conflicts with --format text",
        ));
}

#[test]
fn logs_tail_rejects_format_json_before_loading_config() {
    acps_command()
        .args(["logs", "tail", "--format", "json"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "logs tail does not support --format json",
        ));
}

#[test]
fn text_only_commands_reject_format_json_before_loading_config() {
    acps_command()
        .args(["subagent", "status", "--format", "json"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "subagent does not support --format json",
        ));
}

#[test]
fn completion_scripts_include_root_and_common_commands() {
    for shell in ["bash", "zsh", "fish", "powershell", "elvish"] {
        let output = acps_command()
            .args(["completion", shell])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let stdout = String::from_utf8(output).expect("completion is utf8");
        assert!(
            stdout.contains("acps"),
            "{shell} completion missing binary name"
        );
        assert!(
            stdout.contains("sessions"),
            "{shell} completion missing sessions"
        );
        assert!(
            stdout.contains("completion"),
            "{shell} completion missing completion command"
        );
    }
}

#[test]
fn completion_rejects_format_json() {
    acps_command()
        .args(["completion", "bash", "--format", "json"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "completion does not support --format json",
        ));
}

#[test]
fn failed_cli_command_records_error_after_state_exists() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .success();

    let config_path = tempdir.path().join(".config/acp-stack/acps-config.toml");
    fs::write(
        config_path,
        VALID_CONFIG.replace(r#"bind = "127.0.0.1:7700""#, r#"bind = "bad""#),
    )
    .expect("invalid config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .arg("status")
        .assert()
        .failure();

    let mut logs_command = acps_command();
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

    acps_command()
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .success();

    acps_command()
        .env("HOME", tempdir.path())
        .arg("unknown-command")
        .assert()
        .failure();

    let mut logs_command = acps_command();
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

    acps_command()
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .success();

    acps_command()
        .env("HOME", tempdir.path())
        .arg("--help")
        .assert()
        .success();

    acps_command()
        .env("HOME", tempdir.path())
        .arg("--version")
        .assert()
        .success();

    acps_command()
        .env("HOME", tempdir.path())
        .args(["logs", "--help"])
        .assert()
        .success();

    acps_command()
        .env("HOME", tempdir.path())
        .args(["logs", "query", "--help"])
        .assert()
        .success();

    acps_command()
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

    acps_command()
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .success();

    // Path that mixes a stray ANSI escape sequence and a bare control byte. The runtime
    // must strip ANSI, encode the remaining bytes via serde_json, and still produce a
    // valid JSON payload that survives json_valid() in SQLite.
    let bad_path = OsString::from_vec(b"/tmp/acp\x1b[31m-missing\x07\x08-file.toml".to_vec());

    acps_command()
        .env("HOME", tempdir.path())
        .args(["config", "validate"])
        .arg(&bad_path)
        .assert()
        .failure();

    acps_command()
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
    acps_command()
        .env("HOME", "")
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
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
    let config_path = config_dir.join("acps-config.toml");
    fs::write(
        &config_path,
        VALID_CONFIG.replace(r#"bind = "127.0.0.1:7700""#, r#"bind = "bad""#),
    )
    .expect("invalid config should be written");
    fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o755))
        .expect("config dir perms should set");
    fs::set_permissions(&config_path, fs::Permissions::from_mode(0o644))
        .expect("config file perms should set");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
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

    acps_command()
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
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
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG)
        .expect("valid config should be written");

    let state_dir = tempdir.path().join(".local/share/acp-stack");
    fs::create_dir_all(&state_dir).expect("state dir should be created");
    let state_path = state_dir.join("state.sqlite");
    fs::write(&state_path, b"").expect("placeholder state file should be written");
    fs::set_permissions(&state_path, fs::Permissions::from_mode(0o644))
        .expect("permissive perms should set");

    acps_command()
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

    acps_command()
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

    acps_command()
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .success();

    let state_path = tempdir.path().join(".local/share/acp-stack/state.sqlite");
    fs::set_permissions(&state_path, fs::Permissions::from_mode(0o644))
        .expect("permissive perms should set");
    assert_eq!(mode(&state_path), 0o644);

    // Corrupt the config so the next invocation fails through the error-recording path.
    let config_path = tempdir.path().join(".config/acp-stack/acps-config.toml");
    fs::write(
        &config_path,
        VALID_CONFIG.replace(r#"bind = "127.0.0.1:7700""#, r#"bind = "bad""#),
    )
    .expect("invalid config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .arg("status")
        .assert()
        .failure();

    assert_eq!(
        mode(&state_path),
        0o600,
        "record_cli_error_message must repair permissive perms before writing the error row",
    );

    acps_command()
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

fn codex_config() -> String {
    VALID_CONFIG
        .replace(r#"id = "opencode""#, r#"id = "codex""#)
        .replace(r#"name = "OpenCode""#, r#"name = "Codex""#)
        .replace(r#"command = "opencode""#, r#"command = "codex-acp""#)
        .replace(r#"args = ["acp"]"#, r#"args = []"#)
        .replace(r#"env = ["OPENCODE_API_KEY"]"#, r#"env = []"#)
        .replace(
            r#"
[agent.install]
type = "shell"
shell = "curl -fsSL https://opencode.ai/install | bash"
creates = "opencode"
"#,
            "",
        )
}

fn goose_config() -> String {
    VALID_CONFIG
        .replace(r#"id = "opencode""#, r#"id = "goose""#)
        .replace(r#"name = "OpenCode""#, r#"name = "Goose""#)
        .replace(r#"command = "opencode""#, r#"command = "goose""#)
        .replace(
            r#"env = ["OPENCODE_API_KEY"]"#,
            r#"env = ["OPENROUTER_API_KEY"]"#,
        )
        .replace(
            r#"
[agent.provider]
id = "opencode-go"
model = "opencode-go/deepseek-v4-flash"
api_key_ref = "OPENCODE_API_KEY"
"#,
            r#"
[agent.provider]
id = "openrouter"
model = "deepseek/deepseek-v4-flash"
api_key_ref = "OPENROUTER_API_KEY"
"#,
        )
        .replace(
            r#"
[agent.install]
type = "shell"
shell = "curl -fsSL https://opencode.ai/install | bash"
creates = "opencode"
"#,
            "",
        )
}

fn amp_config() -> String {
    VALID_CONFIG
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
        )
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

fn run_operator_init_with_home(home: &std::path::Path, extra: &[&str]) {
    write_supabase_init_registry(home);
    let workspace = home.join("workspace");
    fs::create_dir_all(&workspace).expect("workspace dir");
    let workspace = workspace.to_str().expect("workspace path utf8");
    let mut args = vec![
        "init",
        "--non-interactive",
        "--agent",
        "supabase-test",
        "--workspace-root",
        workspace,
    ];
    args.extend_from_slice(extra);
    acps_command()
        .env("HOME", home)
        .args(args)
        .assert()
        .success();
}

fn write_supabase_init_registry(home: &std::path::Path) {
    let config_dir = home.join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir");
    fs::write(
        config_dir.join("agents.toml"),
        r#"
[[agents]]
id = "supabase-test"
name = "Supabase Test"
kind = "native"
headless_compatible = true
support_doc = "docs/agents/supabase-test.md"

[agents.harness]
id = "true"

[agents.harness.install.shell]
script = "true"
creates = "true"
"#,
    )
    .expect("agents override");
}

#[test]
fn init_agent_flag_updates_config_non_interactively() {
    let tempdir = tempfile::tempdir().expect("tempdir");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--agent", "cursor", "--skip-workspace-init"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent: Cursor CLI (cursor)"));

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config readable");
    assert!(written.contains(r#"id = "cursor""#));
    assert!(written.contains(&format!(
        r#"command = "{}""#,
        env!("CARGO_BIN_EXE_placebo-agent")
    )));
    assert!(written.contains(r#""acp""#));
    assert!(written.contains(r#""--model-config-option""#));
    assert!(written.contains(r#""placebo-model""#));
    assert!(written.contains(r#"env = ["CURSOR_API_KEY"]"#));
    assert!(written.contains("[agent.auto_update]"));
    assert!(written.contains("enabled = true"));
    assert!(written.contains(r#"frequency = "1d""#));
    assert!(!written.contains("[agent.install]"));
}

#[test]
fn agent_update_set_edits_auto_update_config() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["agent", "update", "set", "--auto-on", "--frequency", "3d"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent update auto: enabled"))
        .stdout(predicates::str::contains("frequency: 3d"));

    let config_text =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config readable");
    let config = load_config_from_str(&config_text).expect("config parses after update set");
    let auto_update = config.agent.auto_update.expect("auto-update written");
    assert!(auto_update.enabled);
    assert_eq!(auto_update.frequency, "3d");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["agent", "update", "set", "--auto-off"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent update auto: disabled"));

    let config_text =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config readable");
    let config = load_config_from_str(&config_text).expect("config parses after auto-off");
    let auto_update = config.agent.auto_update.expect("auto-update retained");
    assert!(!auto_update.enabled);
    assert_eq!(auto_update.frequency, "3d");
}

#[test]
fn agent_update_set_rejects_invalid_frequency() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["agent", "update", "set", "--frequency", "0d"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("agent.auto_update.frequency"));
}

#[test]
fn stack_update_set_edits_update_config() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "update",
            "set",
            "--policy",
            "compatible",
            "--frequency",
            "3d",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "acp-stack update policy: compatible",
        ))
        .stdout(predicates::str::contains("frequency: 3d"));

    let config_text =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config readable");
    let config = load_config_from_str(&config_text).expect("config parses after update set");
    assert_eq!(
        config.updates.acp_stack.policy,
        acp_stack::config::StackUpdatePolicy::Compatible
    );
    assert_eq!(config.updates.acp_stack.frequency, "3d");
}

#[test]
fn stack_update_set_rejects_sub_day_frequency() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["update", "set", "--frequency", "12h"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("updates.acp_stack.frequency"));

    let config_text =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config readable");
    let config = load_config_from_str(&config_text).expect("config still parses after failed set");
    assert_eq!(config.updates.acp_stack.frequency, "1d");
}

#[test]
fn agent_update_set_auto_on_rejects_non_registry_agent() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    // An escape-hatch agent id that the embedded registry does not resolve:
    // enabling auto-update would leave the daemon loop failing every cycle.
    let escape_hatch = VALID_CONFIG.replace(r#"id = "opencode""#, r#"id = "custom-private-agent""#);
    fs::write(config_dir.join("acps-config.toml"), escape_hatch).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["agent", "update", "set", "--auto-on"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("not a managed registry agent"));
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
    fs::write(config_dir.join("acps-config.toml"), config).expect("config");
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

    acps_command()
        .env_remove(TEST_SKIP_AGENT_INSTALL_ENV)
        .env("HOME", tempdir.path())
        .args(["init", "--agent", "init-test"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent install: installed"));

    assert!(managed_binary.is_file());
    let written = fs::read_to_string(config_dir.join("acps-config.toml")).expect("config readable");
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
    let output = acps_command()
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("utf8");
    assert!(stdout.contains("session key: acps_"));
    assert!(stdout.contains("admin key: acps_"));
    assert!(stdout.contains("save the admin key now"));
}

#[test]
fn init_is_idempotent_and_preserves_keys() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    let store = tempdir.path().join(".local/share/acp-stack/secrets.age");
    let first = fs::read(&store).expect("ciphertext readable");

    let stdout = acps_command()
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
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
fn init_backfills_legacy_auth_refs_without_reprinting_keys() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir");
    let legacy_config = VALID_PLACEBO_CONFIG.replace(
        "[security.http]",
        r#"[auth]
session_key_ref = "ACP_STACK_SESSION_KEY"
admin_key_ref = "ACP_STACK_ADMIN_KEY"

[security.http]"#,
    );
    fs::write(config_dir.join("acps-config.toml"), legacy_config).expect("legacy config");
    let mut secret_store = SecretStore::open_or_create(tempdir.path()).expect("secret store");
    secret_store
        .set_many([
            ("ACP_STACK_SESSION_KEY", SESSION_KEY),
            ("ACP_STACK_ADMIN_KEY", ADMIN_KEY),
        ])
        .expect("legacy auth secrets");

    let output = acps_command()
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("utf8");
    assert!(
        stdout.contains("preserved existing API keys"),
        "legacy init must preserve old keys, got: {stdout}",
    );
    assert!(!stdout.contains("session key: acps_"));
    assert!(!stdout.contains("admin key: acps_"));
    assert!(!stdout.contains("save the admin key now"));

    let state_path = default_state_path(tempdir.path());
    let store = StateStore::open(&state_path).expect("state store");
    let verifiers = store.load_auth_verifier_pair().expect("auth verifiers");
    assert_eq!(verifiers.verify(SESSION_KEY), Some(KeyKind::Session));
    assert_eq!(verifiers.verify(ADMIN_KEY), Some(KeyKind::Admin));
}

#[test]
fn init_fails_fast_when_only_one_auth_verifier_exists() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    let state_path = default_state_path(tempdir.path());
    fs::remove_file(&state_path).expect("state db should be removable");
    let store = StateStore::open(&state_path).expect("state store should open");
    store.migrate().expect("state schema should migrate");
    store
        .upsert_auth_key(
            KeyKind::Admin,
            &AuthVerifierSet::create(SESSION_KEY, ADMIN_KEY).admin,
        )
        .expect("admin verifier should replace pair");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("auth_keys.session"));
}

#[test]
fn init_fails_fast_when_auth_verifier_is_malformed() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    let state_path = default_state_path(tempdir.path());
    let connection = rusqlite::Connection::open(&state_path).expect("state db should open");
    connection
        .execute(
            "UPDATE auth_keys SET algorithm = 'sha256-v0' WHERE key_kind = 'session'",
            [],
        )
        .expect("auth verifier should be corruptible");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("auth_keys.algorithm"));
}

#[test]
fn secrets_set_only_captures_first_line_of_stdin() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let (_, admin_key) = run_init_with_home(tempdir.path());

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "secrets",
            "set",
            "MULTILINE_TEST",
            "--admin-key",
            admin_key.as_str(),
        ])
        .write_stdin("first-line\nsecond-line\n")
        .assert()
        .success();

    let store = acp_stack::secrets::SecretStore::open(tempdir.path()).expect("open store");
    assert_eq!(store.get("MULTILINE_TEST").expect("get"), "first-line");
}

#[test]
fn init_supabase_url_enables_config_and_env_secret() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    write_supabase_init_registry(tempdir.path());
    let workspace = tempdir.path().join("workspace");
    fs::create_dir_all(&workspace).expect("workspace dir");
    let workspace = workspace.to_str().expect("workspace path utf8");

    acps_command()
        .env("HOME", tempdir.path())
        .env("ACP_STACK_SUPABASE_SECRET_KEY", "sb_secret_cli_test")
        .args([
            "init",
            "--non-interactive",
            "--agent",
            "supabase-test",
            "--workspace-root",
            workspace,
            "--supabase-url",
            "https://project-ref.supabase.co",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "supabase secret: set (SUPABASE_SECRET_KEY)",
        ));

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config readable");
    let config = load_config_from_str(&written).expect("config parses");
    let supabase = config.logging.supabase.expect("supabase configured");
    assert!(supabase.enabled);
    assert_eq!(supabase.url, "https://project-ref.supabase.co");
    assert_eq!(supabase.schema, "acp_stack");
    assert_eq!(supabase.api_key_ref, "SUPABASE_SECRET_KEY");
    let store = SecretStore::open(tempdir.path()).expect("store opens");
    assert_eq!(
        store.get("SUPABASE_SECRET_KEY").expect("supabase secret"),
        "sb_secret_cli_test"
    );
    assert!(!written.contains("sb_secret_cli_test"));
}

#[test]
fn init_supabase_env_bootstrap_matches_init_flags() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    write_supabase_init_registry(tempdir.path());
    let workspace = tempdir.path().join("workspace");
    fs::create_dir_all(&workspace).expect("workspace dir");
    let workspace = workspace.to_str().expect("workspace path utf8");

    acps_command()
        .env("HOME", tempdir.path())
        .env("ACP_STACK_SUPABASE_URL", "https://env-project.supabase.co")
        .env("ACP_STACK_SUPABASE_SCHEMA", "analytics")
        .env("ACP_STACK_SUPABASE_API_KEY_REF", "ENV_SUPABASE_SECRET")
        .env("ACP_STACK_SUPABASE_SECRET_KEY", "sb_secret_from_env")
        .args([
            "init",
            "--non-interactive",
            "--agent",
            "supabase-test",
            "--workspace-root",
            workspace,
        ])
        .assert()
        .success();

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config readable");
    let config = load_config_from_str(&written).expect("config parses");
    let supabase = config.logging.supabase.expect("supabase configured");
    assert!(supabase.enabled);
    assert_eq!(supabase.url, "https://env-project.supabase.co");
    assert_eq!(supabase.schema, "analytics");
    assert_eq!(supabase.api_key_ref, "ENV_SUPABASE_SECRET");
    let store = SecretStore::open(tempdir.path()).expect("store opens");
    assert_eq!(
        store.get("ENV_SUPABASE_SECRET").expect("supabase secret"),
        "sb_secret_from_env"
    );
}

#[test]
fn init_supabase_non_interactive_requires_secret() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    write_supabase_init_registry(tempdir.path());
    let workspace = tempdir.path().join("workspace");
    fs::create_dir_all(&workspace).expect("workspace dir");
    let workspace = workspace.to_str().expect("workspace path utf8");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "init",
            "--non-interactive",
            "--agent",
            "supabase-test",
            "--workspace-root",
            workspace,
            "--supabase-url",
            "https://project-ref.supabase.co",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "does not contain the Supabase secret API key reference",
        ));
    let state_path = tempdir.path().join(".local/share/acp-stack/state.sqlite");
    let store = StateStore::open(&state_path).expect("state opens");
    let runs = store.query_init_runs(1).expect("query runs");
    assert_eq!(runs[0].status, acp_stack::state::INIT_RUN_FAILED);
}

#[test]
fn logging_supabase_cli_edits_config_and_secret_store() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_operator_init_with_home(tempdir.path(), &[]);

    let enable_output = acps_command()
        .env("HOME", tempdir.path())
        .args([
            "logging",
            "supabase",
            "enable",
            "--url",
            "https://cli-project.supabase.co",
            "--schema",
            "analytics",
            "--api-key-ref",
            "CLI_SUPABASE_SECRET",
            "--format",
            "json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let enable_body: Value = serde_json::from_slice(&enable_output).expect("enable json parses");
    assert_eq!(enable_body["action"], "enabled");
    assert_eq!(enable_body["api_key_ref"], "CLI_SUPABASE_SECRET");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "logging",
            "supabase",
            "set-secret",
            "--api-key-ref",
            "CLI_SUPABASE_SECRET",
        ])
        .write_stdin("sb_secret_cli_value\nignored\n")
        .assert()
        .success()
        .stdout(predicates::str::contains("sb_secret_cli_value").not());

    let status_output = acps_command()
        .env("HOME", tempdir.path())
        .args(["logging", "supabase", "status", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let status_body: Value = serde_json::from_slice(&status_output).expect("status json parses");
    assert_eq!(status_body["enabled"], true);
    assert_eq!(status_body["schema"], "analytics");
    assert_eq!(status_body["secret_present"], true);
    assert!(!String::from_utf8_lossy(&status_output).contains("sb_secret_cli_value"));

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config readable");
    let config = load_config_from_str(&written).expect("config parses");
    let supabase = config.logging.supabase.expect("supabase configured");
    assert!(supabase.enabled);
    assert_eq!(supabase.url, "https://cli-project.supabase.co");
    assert_eq!(supabase.schema, "analytics");
    assert_eq!(supabase.api_key_ref, "CLI_SUPABASE_SECRET");
    assert!(!written.contains("sb_secret_cli_value"));
    let store = SecretStore::open(tempdir.path()).expect("store opens");
    assert_eq!(
        store.get("CLI_SUPABASE_SECRET").expect("supabase secret"),
        "sb_secret_cli_value"
    );

    acps_command()
        .env("HOME", tempdir.path())
        .args(["logging", "supabase", "disable"])
        .assert()
        .success();
    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config readable");
    let config = load_config_from_str(&written).expect("config parses");
    let supabase = config.logging.supabase.expect("supabase configured");
    assert!(!supabase.enabled);
    assert_eq!(supabase.url, "https://cli-project.supabase.co");
    assert_eq!(supabase.schema, "analytics");
}

#[test]
fn logging_supabase_setup_uses_cli_and_stores_writer_db_url() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_operator_init_with_home(tempdir.path(), &[]);
    let fake_bin = tempdir.path().join("bin");
    fs::create_dir_all(&fake_bin).expect("fake bin");
    let fake_log = tempdir.path().join("supabase.log");
    let fake_supabase = fake_bin.join("supabase");
    fs::write(
        &fake_supabase,
        "#!/bin/sh\nprintf '%s|%s\\n' \"$PWD\" \"$*\" >> \"$FAKE_SUPABASE_LOG\"\nexit 0\n",
    )
    .expect("write fake supabase");
    #[cfg(unix)]
    fs::set_permissions(&fake_supabase, fs::Permissions::from_mode(0o755))
        .expect("chmod fake supabase");
    let path = format!(
        "{}:{}",
        fake_bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let setup_output = acps_command()
        .env("HOME", tempdir.path())
        .env("PATH", path)
        .env("FAKE_SUPABASE_LOG", &fake_log)
        .args([
            "logging",
            "supabase",
            "setup",
            "--url",
            "https://psklvkrmvqqwzryiawgn.supabase.co/",
            "--yes",
            "--format",
            "json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let setup_body: Value = serde_json::from_slice(&setup_output).expect("setup json parses");
    assert_eq!(setup_body["backend"], "postgres");
    assert_eq!(setup_body["db_url_ref"], "SUPABASE_LOG_DB_URL");
    assert!(!String::from_utf8_lossy(&setup_output).contains("postgresql://"));

    let fake_log = fs::read_to_string(fake_log).expect("read fake log");
    assert!(fake_log.contains("|init\n"), "{fake_log}");
    assert!(
        fake_log.contains("|link --project-ref psklvkrmvqqwzryiawgn\n"),
        "{fake_log}"
    );
    assert!(fake_log.contains("|db push --yes\n"), "{fake_log}");

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config readable");
    let config = load_config_from_str(&written).expect("config parses");
    let supabase = config.logging.supabase.expect("supabase configured");
    assert!(supabase.enabled);
    assert_eq!(supabase.url, "https://psklvkrmvqqwzryiawgn.supabase.co");
    assert_eq!(
        supabase.backend,
        acp_stack::config::SupabaseLoggingBackend::Postgres
    );
    assert_eq!(supabase.schema, "public");
    assert_eq!(supabase.table_prefix, "acp_stack_");
    assert_eq!(supabase.db_url_ref.as_deref(), Some("SUPABASE_LOG_DB_URL"));
    assert!(!written.contains("postgresql://"));

    let store = SecretStore::open(tempdir.path()).expect("store opens");
    let db_url = store.get("SUPABASE_LOG_DB_URL").expect("db url");
    assert!(db_url.starts_with("postgresql://acp_stack_logger:"));
    assert!(db_url.contains("@db.psklvkrmvqqwzryiawgn.supabase.co:5432/postgres?sslmode=require"));
}

#[test]
fn logging_supabase_sql_prints_prefixed_public_ddl() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_operator_init_with_home(tempdir.path(), &[]);

    let output = acps_command()
        .env("HOME", tempdir.path())
        .args([
            "logging",
            "supabase",
            "sql",
            "--writer-password",
            "test_writer_password",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let sql = String::from_utf8(output).expect("sql utf8");
    assert!(sql.contains("CREATE TABLE IF NOT EXISTS \"public\".\"acp_stack_events\""));
    assert!(sql.contains("CREATE ROLE \"acp_stack_logger\" LOGIN PASSWORD 'test_writer_password'"));
    assert!(sql.contains("SECURITY DEFINER"));
    assert!(sql.contains(
        "GRANT EXECUTE ON FUNCTION \"public\".\"acp_stack_ingest_batch\"(text, jsonb) TO \"acp_stack_logger\""
    ));
    assert!(sql.contains("REVOKE ALL ON TABLE"));
    for table in [
        "schema_migrations",
        "events",
        "sessions",
        "prompts",
        "commands",
        "permission_requests",
        "permission_decisions",
        "auth_failures",
        "agent_lifecycle",
    ] {
        assert!(
            sql.contains(&format!(
                "ALTER TABLE \"public\".\"acp_stack_{table}\" ENABLE ROW LEVEL SECURITY"
            )),
            "missing RLS enablement for {table}"
        );
    }
    for view in [
        "session_turns",
        "permissions",
        "agent_events",
        "security_events",
        "connection_events",
        "usage_metrics",
    ] {
        assert!(
            sql.contains(&format!(
                "CREATE OR REPLACE VIEW \"public\".\"acp_stack_{view}\"\nWITH (security_invoker = true) AS"
            )),
            "missing security_invoker for {view}"
        );
    }
    // PUBLIC is revoked unconditionally; anon/authenticated are revoked only
    // behind a pg_roles existence guard (so the SQL is safe on a non-Supabase
    // Postgres), never as an unconditional `FROM PUBLIC, "anon", "authenticated"`.
    assert!(sql.contains("FROM PUBLIC;"));
    assert!(sql.contains("IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = api_role_name)"));
    assert!(sql.contains("EXECUTE format('REVOKE ALL ON TABLE"));
    assert!(sql.contains("EXECUTE format('REVOKE ALL ON FUNCTION"));
    assert!(!sql.contains("FROM PUBLIC, \"anon\", \"authenticated\""));
    // Writes go through the SECURITY DEFINER ingest function, so the writer role
    // gets no direct table access and no per-table RLS policies are emitted.
    assert!(!sql.contains("CREATE POLICY"));
    assert!(!sql.contains("FOR INSERT TO \"acp_stack_logger\""));
    assert!(!sql.contains("FOR UPDATE TO \"acp_stack_logger\""));
    assert!(!sql.contains("GRANT INSERT, UPDATE, SELECT ON TABLE"));
    assert!(!sql.contains(" TO PUBLIC"));
    assert!(!sql.contains(" TO \"anon\""));
    assert!(!sql.contains(" TO \"authenticated\""));
    assert!(!sql.contains("FOR SELECT TO \"acp_stack_logger\""));
    assert!(sql.contains("failure_detail_json jsonb"));
    assert!(sql.contains("message_id_acknowledged boolean NOT NULL DEFAULT false"));
    assert!(sql.contains("output_bytes bigint NOT NULL DEFAULT 0"));
}

#[test]
fn logging_supabase_sql_rejects_unsafe_schema() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_operator_init_with_home(tempdir.path(), &[]);

    // A schema with a single quote would break out of the PL/pgSQL `format()`
    // string literal in the generated revoke statements; reject it up front.
    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "logging",
            "supabase",
            "sql",
            "--schema",
            "pub'lic",
            "--writer-password",
            "test_writer_password",
        ])
        .assert()
        .failure();
}

#[test]
fn init_supabase_env_does_not_rewrite_existing_config() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_operator_init_with_home(tempdir.path(), &[]);
    let workspace = tempdir.path().join("workspace");
    let workspace = workspace.to_str().expect("workspace path utf8");

    acps_command()
        .env("HOME", tempdir.path())
        .env("ACP_STACK_SUPABASE_URL", "https://ambient.supabase.co")
        .env("ACP_STACK_SUPABASE_SECRET_KEY", "sb_secret_ambient")
        .args([
            "init",
            "--non-interactive",
            "--agent",
            "supabase-test",
            "--workspace-root",
            workspace,
        ])
        .assert()
        .success();

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config readable");
    let config = load_config_from_str(&written).expect("config parses");
    let supabase = config.logging.supabase.expect("supabase configured");
    assert!(!supabase.enabled);
    assert_eq!(supabase.url, "https://example.supabase.co");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "init",
            "--non-interactive",
            "--agent",
            "supabase-test",
            "--workspace-root",
            workspace,
            "--supabase-url",
            "https://explicit.supabase.co",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "use `acps logging supabase` for initialized instances",
        ));
}

#[test]
fn logging_supabase_enable_rejects_invalid_url_before_writing() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_operator_init_with_home(tempdir.path(), &[]);
    let before = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config readable");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "logging",
            "supabase",
            "enable",
            "--url",
            "http://cli-project.supabase.co",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("must start with `https://`"));

    let after = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config readable");
    assert_eq!(before, after);
}

#[test]
fn init_fails_fast_when_admin_verifier_missing() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    let state_path = default_state_path(tempdir.path());
    fs::remove_file(&state_path).expect("state db should be removable");
    let store = StateStore::open(&state_path).expect("state store should open");
    store.migrate().expect("state schema should migrate");
    store
        .upsert_auth_key(
            KeyKind::Session,
            &AuthVerifierSet::create(SESSION_KEY, ADMIN_KEY).session,
        )
        .expect("session verifier should be stored");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("auth_keys.admin"));
}

#[test]
fn secrets_set_requires_admin_key() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    acps_command()
        .env("HOME", tempdir.path())
        .args(["secrets", "set", "OPENCODE_API_KEY"])
        .write_stdin("attacker-supplied")
        .assert()
        .failure()
        .stderr(predicates::str::contains("--admin-key"));
}

#[test]
fn secrets_set_allows_old_auth_ref_names_with_admin_key() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let (_, admin_key) = run_init_with_home(tempdir.path());

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "secrets",
            "set",
            "ACP_STACK_SESSION_KEY",
            "--admin-key",
            admin_key.as_str(),
        ])
        .write_stdin("ordinary-secret")
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "set secret: ACP_STACK_SESSION_KEY",
        ));
}

#[test]
fn secrets_delete_requires_admin_key() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let (_, admin_key) = run_init_with_home(tempdir.path());

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "secrets",
            "set",
            "TEMP_VALUE",
            "--admin-key",
            admin_key.as_str(),
        ])
        .write_stdin("abc")
        .assert()
        .success();

    acps_command()
        .env("HOME", tempdir.path())
        .args(["secrets", "delete", "TEMP_VALUE"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--admin-key"));
}

#[test]
fn secrets_list_shows_session_and_admin_names_only_after_init() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    acps_command()
        .env("HOME", tempdir.path())
        .args(["secrets", "list"])
        .assert()
        .success()
        .stdout(predicates::str::contains("ACP_STACK_ADMIN_KEY").not())
        .stdout(predicates::str::contains("ACP_STACK_SESSION_KEY").not())
        .stdout(predicates::str::contains("acps_").not());
}

#[test]
fn secrets_commands_format_json_never_print_values() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let (_, admin_key) = run_init_with_home(tempdir.path());

    let set_output = acps_command()
        .env("HOME", tempdir.path())
        .args([
            "secrets",
            "set",
            "OPENCODE_API_KEY",
            "--format",
            "json",
            "--admin-key",
            admin_key.as_str(),
        ])
        .write_stdin("super-secret-value\n")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let set_body: Value = serde_json::from_slice(&set_output).expect("set json parses");
    assert_eq!(set_body["action"], "set");
    assert_eq!(set_body["name"], "OPENCODE_API_KEY");
    assert!(!String::from_utf8_lossy(&set_output).contains("super-secret-value"));

    let list_output = acps_command()
        .env("HOME", tempdir.path())
        .args(["secrets", "list", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let list_body: Value = serde_json::from_slice(&list_output).expect("list json parses");
    let names = list_body["secrets"]
        .as_array()
        .expect("secrets should be an array");
    assert!(names.iter().any(|name| name == "OPENCODE_API_KEY"));
    assert!(!String::from_utf8_lossy(&list_output).contains("super-secret-value"));

    let delete_output = acps_command()
        .env("HOME", tempdir.path())
        .args([
            "secrets",
            "delete",
            "OPENCODE_API_KEY",
            "--format",
            "json",
            "--admin-key",
            admin_key.as_str(),
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let delete_body: Value = serde_json::from_slice(&delete_output).expect("delete json parses");
    assert_eq!(delete_body["action"], "delete");
    assert_eq!(delete_body["name"], "OPENCODE_API_KEY");
}

#[test]
fn secrets_set_reads_value_from_stdin() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let (_, admin_key) = run_init_with_home(tempdir.path());

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "secrets",
            "set",
            "OPENCODE_API_KEY",
            "--admin-key",
            admin_key.as_str(),
        ])
        .write_stdin("super-secret-value\n")
        .assert()
        .success()
        .stdout(predicates::str::contains("set secret: OPENCODE_API_KEY"));

    acps_command()
        .env("HOME", tempdir.path())
        .args(["secrets", "list"])
        .assert()
        .success()
        .stdout(predicates::str::contains("OPENCODE_API_KEY"));
}

#[test]
fn secrets_delete_removes_named_secret() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let (_, admin_key) = run_init_with_home(tempdir.path());

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "secrets",
            "set",
            "TEMP_VALUE",
            "--admin-key",
            admin_key.as_str(),
        ])
        .write_stdin("abc")
        .assert()
        .success();

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "secrets",
            "delete",
            "TEMP_VALUE",
            "--admin-key",
            admin_key.as_str(),
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("deleted secret: TEMP_VALUE"));

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "secrets",
            "delete",
            "TEMP_VALUE",
            "--admin-key",
            admin_key.as_str(),
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("was not found"));
}

#[test]
fn auth_regenerate_session_key_requires_admin_key() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    acps_command()
        .env("HOME", tempdir.path())
        .args(["auth", "regenerate-session-key"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--admin-key"));
}

#[test]
fn reset_without_yes_lists_targets_and_keeps_files() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    acps_command()
        .env("HOME", tempdir.path())
        .arg("reset")
        .assert()
        .failure()
        .stdout(predicates::str::contains("acps reset would delete:"))
        .stdout(predicates::str::contains("acps-config.toml"))
        .stdout(predicates::str::contains("state.sqlite"))
        .stdout(predicates::str::contains("age.key"))
        .stdout(predicates::str::contains("secrets.age"))
        .stdout(predicates::str::contains("re-run with --yes"));

    assert!(
        tempdir
            .path()
            .join(".config/acp-stack/acps-config.toml")
            .exists(),
        "dry-run must NOT remove files",
    );
}

#[test]
fn reset_dry_run_does_not_write_cli_error_event() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    acps_command()
        .env("HOME", tempdir.path())
        .arg("reset")
        .assert()
        .failure();

    // The dry-run contract is "exits without touching the filesystem".
    // Recording a `cli.error` event row would touch state.sqlite, so the
    // event log must show no error rows after a dry-run reset.
    acps_command()
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

    acps_command()
        .env("HOME", tempdir.path())
        .args(["reset", "--yes"])
        .assert()
        .success()
        .stdout(predicates::str::contains("reset acp-stack"));

    assert!(
        !tempdir
            .path()
            .join(".config/acp-stack/acps-config.toml")
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
    acps_command()
        .env("HOME", tempdir.path())
        .args(["reset", "--yes"])
        .assert()
        .success();

    // Fresh init after reset produces a different admin key than the first.
    let init_after = acps_command()
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(init_after).expect("utf8");
    assert!(stdout.contains("admin key: acps_"));
}

#[test]
fn config_import_refuses_without_force_when_config_exists() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    let exported = acps_command()
        .env("HOME", tempdir.path())
        .args(["config", "export"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let import_path = tempdir.path().join("exported.toml");
    fs::write(&import_path, exported).expect("write export");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["config", "import", import_path.to_str().unwrap()])
        .assert()
        .failure()
        .stdout("")
        .stderr(predicates::str::contains("config already exists"))
        .stderr(predicates::str::contains("--admin-key").not());
}

#[test]
fn config_import_with_force_replaces_existing_config() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let (_, admin_key) = run_init_with_home(tempdir.path());

    // Build an alternate config with a recognizable bind addr.
    let modified =
        VALID_PLACEBO_CONFIG.replace(r#"bind = "127.0.0.1:7700""#, r#"bind = "127.0.0.1:7777""#);
    let import_path = tempdir.path().join("alt.toml");
    fs::write(&import_path, &modified).expect("write alt");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "config",
            "import",
            import_path.to_str().unwrap(),
            "--force",
            "--admin-key",
            admin_key.as_str(),
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("imported config (replaced)"));

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config readable");
    assert!(written.contains("127.0.0.1:7777"));
}

#[test]
fn config_import_force_replaces_invalid_existing_config() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let (_, admin_key) = run_init_with_home(tempdir.path());
    let config_path = tempdir.path().join(".config/acp-stack/acps-config.toml");
    fs::write(&config_path, "not valid toml").expect("write invalid config");

    let modified =
        VALID_PLACEBO_CONFIG.replace(r#"bind = "127.0.0.1:7700""#, r#"bind = "127.0.0.1:7778""#);
    let import_path = tempdir.path().join("replacement.toml");
    fs::write(&import_path, &modified).expect("write replacement");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "config",
            "import",
            import_path.to_str().unwrap(),
            "--force",
            "--admin-key",
            admin_key.as_str(),
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("imported config (replaced)"))
        .stdout(predicates::str::contains(
            "local session access will apply on next daemon start",
        ));

    let written = fs::read_to_string(config_path).expect("config readable");
    assert!(written.contains("127.0.0.1:7778"));
}

#[test]
fn config_validate_and_import_dry_run_format_json() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let (_, admin_key) = run_init_with_home(tempdir.path());
    let config_path = tempdir.path().join(".config/acp-stack/acps-config.toml");

    let validate_output = acps_command()
        .env("HOME", tempdir.path())
        .args(["config", "validate", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let validate_body: Value =
        serde_json::from_slice(&validate_output).expect("validate json parses");
    assert_eq!(validate_body["valid"], true);
    assert!(validate_body["path"].is_null(), "{validate_body}");

    let import_output = acps_command()
        .env("HOME", tempdir.path())
        .arg("config")
        .arg("import")
        .arg(&config_path)
        .args([
            "--dry-run",
            "--format",
            "json",
            "--admin-key",
            admin_key.as_str(),
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let import_body: Value = serde_json::from_slice(&import_output).expect("import json parses");
    assert_eq!(import_body["dry_run"], true);
    assert_eq!(import_body["target_exists"], true);
    assert!(import_body.get("auth_refs_unchanged").is_none());
}

#[test]
fn config_export_format_json_wraps_toml_without_leaking_secret_values() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    let output = acps_command()
        .env("HOME", tempdir.path())
        .args(["config", "export", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let body: Value = serde_json::from_slice(&output).expect("config export json parses");
    assert_eq!(body["format"], "toml");
    assert!(body["bytes"].as_u64().unwrap_or(0) > 0);
    let value = body["value"].as_str().expect("exported value is string");
    assert!(!value.contains("ACP_STACK_SESSION_KEY"));
    assert!(!value.contains("ACP_STACK_ADMIN_KEY"));
    assert!(!value.contains(SESSION_KEY));
    assert!(!value.contains(ADMIN_KEY));
}

#[test]
fn config_export_to_output_reports_progress() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());
    let output_path = tempdir.path().join("exported.toml");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["config", "export", "--output"])
        .arg(&output_path)
        .assert()
        .success()
        .stdout(predicates::str::contains("progress: loading config"))
        .stdout(predicates::str::contains(
            "progress: rendering config export",
        ))
        .stdout(predicates::str::contains("progress: writing config export"));

    let exported = fs::read_to_string(output_path).expect("export should be written");
    assert!(!exported.contains("ACP_STACK_SESSION_KEY"));
    assert!(!exported.contains("ACP_STACK_ADMIN_KEY"));
}

#[test]
fn config_import_supports_base64_input() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let (_, admin_key) = run_init_with_home(tempdir.path());
    let modified =
        VALID_PLACEBO_CONFIG.replace(r#"bind = "127.0.0.1:7700""#, r#"bind = "127.0.0.1:7788""#);
    let encoded = base64::engine::general_purpose::STANDARD.encode(modified);

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "config",
            "import",
            "--base64",
            &encoded,
            "--force",
            "--admin-key",
            admin_key.as_str(),
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("progress: reading config import"))
        .stdout(predicates::str::contains(
            "progress: validating config import",
        ))
        .stdout(predicates::str::contains("progress: writing config import"))
        .stdout(predicates::str::contains("imported config"));

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config readable");
    assert!(written.contains("127.0.0.1:7788"));
}

#[test]
fn init_from_base64_imports_config_and_continues() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let modified =
        VALID_PLACEBO_CONFIG.replace(r#"bind = "127.0.0.1:7700""#, r#"bind = "127.0.0.1:7791""#);
    let encoded = base64::engine::general_purpose::STANDARD.encode(modified);

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--from-base64",
            &encoded,
            "--non-interactive",
            "--skip-workspace-init",
            "--skip-testflight",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("progress: reading config import"))
        .stdout(predicates::str::contains("imported config:"))
        .stdout(predicates::str::contains("initialized acp-stack"));

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config readable");
    assert!(written.contains("127.0.0.1:7791"));
}

#[test]
fn init_from_file_imports_config_and_continues() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let modified =
        VALID_PLACEBO_CONFIG.replace(r#"bind = "127.0.0.1:7700""#, r#"bind = "127.0.0.1:7792""#);
    let import_path = tempdir.path().join("import-acps-config.toml");
    fs::write(&import_path, modified).expect("import config");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--from-file",
            import_path.to_str().expect("path utf8"),
            "--non-interactive",
            "--skip-workspace-init",
            "--skip-testflight",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("progress: reading config import"))
        .stdout(predicates::str::contains("initialized acp-stack"));

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config readable");
    assert!(written.contains("127.0.0.1:7792"));
}

#[test]
fn init_from_toml_imports_config_and_continues() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let modified =
        VALID_PLACEBO_CONFIG.replace(r#"bind = "127.0.0.1:7700""#, r#"bind = "127.0.0.1:7793""#);

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--from-toml",
            &modified,
            "--non-interactive",
            "--skip-workspace-init",
            "--skip-testflight",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("progress: reading config import"))
        .stdout(predicates::str::contains("initialized acp-stack"));

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config readable");
    assert!(written.contains("127.0.0.1:7793"));
}

#[test]
fn init_from_base64_rejects_invalid_base64() {
    let tempdir = tempfile::tempdir().expect("tempdir");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--from-base64",
            "!!!not-base64!!!",
            "--non-interactive",
            "--skip-workspace-init",
            "--skip-testflight",
        ])
        .assert()
        .failure()
        .stdout("")
        .stderr(predicates::str::contains("not valid base64"));

    assert!(
        !tempdir
            .path()
            .join(".config/acp-stack/acps-config.toml")
            .exists(),
        "invalid base64 must not create a config file"
    );
}

#[test]
fn config_import_requires_admin_key() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    let modified =
        VALID_PLACEBO_CONFIG.replace(r#"bind = "127.0.0.1:7700""#, r#"bind = "127.0.0.1:7781""#);
    let import_path = tempdir.path().join("rotated.toml");
    fs::write(&import_path, &modified).expect("write rotated");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["config", "import", import_path.to_str().unwrap(), "--force"])
        .assert()
        .failure()
        .stdout("")
        .stderr(predicates::str::contains("--admin-key"));
}

#[test]
fn config_import_strips_legacy_auth_section() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let (_, admin_key) = run_init_with_home(tempdir.path());

    let modified = VALID_PLACEBO_CONFIG.replace(
        "[security.http]",
        r#"[auth]
session_key_ref = "ACP_STACK_SESSION_KEY"
admin_key_ref = "ACP_STACK_ADMIN_KEY"

[security.http]"#,
    );
    let import_path = tempdir.path().join("rotated-session.toml");
    fs::write(&import_path, &modified).expect("write rotated session");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "config",
            "import",
            import_path.to_str().unwrap(),
            "--force",
            "--admin-key",
            admin_key.as_str(),
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("imported config (replaced)"));

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config");
    assert!(!written.contains("[auth]"));
    assert!(!written.contains("session_key_ref"));
    assert!(!written.contains("admin_key_ref"));
}

#[test]
fn config_import_rejects_invalid_base64() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    acps_command()
        .env("HOME", tempdir.path())
        .args(["config", "import", "--base64", "!!!not-base64!!!"])
        .assert()
        .failure()
        .stdout("")
        .stderr(predicates::str::contains("not valid base64"));
}

#[test]
fn config_import_dry_run_with_path() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());
    let config_path = tempdir.path().join(".config/acp-stack/acps-config.toml");
    let original_config = fs::read_to_string(&config_path).expect("config readable");

    let import_path = tempdir.path().join("import.toml");
    fs::write(&import_path, VALID_PLACEBO_CONFIG).expect("write config");

    let output = acps_command()
        .env("HOME", tempdir.path())
        .args([
            "config",
            "import",
            import_path.to_str().unwrap(),
            "--dry-run",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("utf8");
    assert!(stdout.contains("import dry-run complete"));
    assert!(stdout.contains("config_version:"));
    assert!(stdout.contains("canonical TOML size:"));
    assert!(stdout.contains("would write to:"));
    let current_config = fs::read_to_string(&config_path).expect("config readable");
    assert_eq!(current_config, original_config);
}

#[test]
fn config_import_dry_run_with_base64() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());
    let config_path = tempdir.path().join(".config/acp-stack/acps-config.toml");
    let original_config = fs::read_to_string(&config_path).expect("config readable");

    let encoded = base64::engine::general_purpose::STANDARD.encode(VALID_PLACEBO_CONFIG);

    let output = acps_command()
        .env("HOME", tempdir.path())
        .args(["config", "import", "--base64", &encoded, "--dry-run"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("utf8");
    assert!(stdout.contains("import dry-run complete"));
    assert!(stdout.contains("config_version:"));
    assert!(stdout.contains("would write to:"));
    let current_config = fs::read_to_string(&config_path).expect("config readable");
    assert_eq!(current_config, original_config);
}

#[test]
fn config_import_rejects_oversized_path_input() {
    let tempdir = tempfile::tempdir().expect("tempdir");

    let big_config = "x".repeat(2 * 1024 * 1024); // 2 MiB
    let import_path = tempdir.path().join("big.toml");
    fs::write(&import_path, &big_config).expect("write big config");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["config", "import", import_path.to_str().unwrap()])
        .assert()
        .failure()
        .stdout("")
        .stderr(predicates::str::contains("exceeds 1048576-byte size limit"));
}

#[test]
fn init_records_run_with_succeeded_steps() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    let state_path = tempdir.path().join(".local/share/acp-stack/state.sqlite");
    let store = acp_stack::state::StateStore::open(&state_path).expect("state opens");

    let runs = store.query_init_runs(10).expect("query runs");
    assert_eq!(runs.len(), 1, "first init must record exactly one run");
    let run = &runs[0];
    assert_eq!(run.status, acp_stack::state::INIT_RUN_SUCCEEDED);

    let steps = store.query_init_steps(&run.id).expect("query steps");
    assert!(!steps.is_empty(), "run must record at least one step");
    let kinds: Vec<&str> = steps.iter().map(|s| s.kind.as_str()).collect();
    assert!(
        kinds.contains(&"secrets_init"),
        "expected secrets_init in {kinds:?}",
    );
    assert!(
        kinds.contains(&"init_complete"),
        "expected init_complete in {kinds:?}",
    );
    for step in &steps {
        assert!(
            matches!(step.status.as_str(), "succeeded" | "skipped"),
            "step `{}` settled with unexpected status `{}`",
            step.kind,
            step.status,
        );
    }
}

#[test]
fn init_records_workspace_before_provider_configure() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let workspace = tempdir.path().join("workspace");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--workspace-root",
            workspace.to_str().expect("workspace path should be UTF-8"),
        ])
        .assert()
        .success();

    let state_path = tempdir.path().join(".local/share/acp-stack/state.sqlite");
    let store = acp_stack::state::StateStore::open(&state_path).expect("state opens");
    let run = store
        .query_init_runs(1)
        .expect("query runs")
        .into_iter()
        .next()
        .expect("init run");
    let steps = store.query_init_steps(&run.id).expect("query steps");
    let workspace_step = steps
        .iter()
        .find(|step| step.kind == "workspace_materialize")
        .expect("workspace step");
    let provider_step = steps
        .iter()
        .find(|step| step.kind == "provider_configure")
        .expect("provider step");

    assert!(
        workspace_step.ordinal < provider_step.ordinal,
        "workspace materialization must run before provider/model discovery: {steps:?}",
    );
}

#[test]
fn init_resume_targets_specific_pending_run_by_id() {
    // Simulate the post-crash shape: a prior init created the run but
    // never reached `init_complete`, so the row stays `pending`.
    // `acps init --resume --run-id <id>` must pick it up, run any
    // remaining steps, and finalize it `succeeded`.
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    let state_path = tempdir.path().join(".local/share/acp-stack/state.sqlite");
    let store = acp_stack::state::StateStore::open(&state_path).expect("state opens");
    // Inject a synthetic pending run that resume will discover. Use the
    // public state API so this test exercises the same code path the
    // orchestrator would on a real crash mid-init.
    let pending = store
        .create_init_run(acp_stack::state::NewInitRun {
            runtime_user: None,
            agent_id: None,
            args_json: "{}",
        })
        .expect("synth pending run");
    let pending_id = pending.id.clone();
    drop(store);

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--resume",
            "--run-id",
            &pending_id,
            "--skip-workspace-init",
        ])
        .assert()
        .success();

    let store = acp_stack::state::StateStore::open(&state_path).expect("state opens");
    let reloaded = store
        .lookup_init_run(&pending_id)
        .expect("lookup")
        .expect("pending row should still exist");
    assert_eq!(reloaded.status, acp_stack::state::INIT_RUN_SUCCEEDED);
    let steps = store.query_init_steps(&pending_id).expect("steps");
    assert!(
        !steps.is_empty(),
        "resume should have populated steps for the pending run",
    );
    for step in &steps {
        assert!(
            matches!(step.status.as_str(), "succeeded" | "skipped"),
            "step `{}` settled with unexpected status `{}`",
            step.kind,
            step.status,
        );
    }
}

#[test]
fn init_resume_retries_failed_agent_install_even_without_install_flag() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    let workspace = tempdir.path().join("workspace");
    fs::create_dir_all(&workspace).expect("workspace dir should be created");
    let missing_creates = tempdir.path().join("missing-resume-install-marker");
    let config_path = tempdir.path().join(".config/acp-stack/acps-config.toml");
    let mut config =
        load_config_from_str(&fs::read_to_string(&config_path).expect("config should be readable"))
            .expect("config should validate");
    config.workspace.root = workspace.to_string_lossy().into_owned();
    config.agent.id = "resume-install-test".to_owned();
    config.agent.name = "Resume Install Test".to_owned();
    config.agent.command = "resume-install-test-agent".to_owned();
    config.agent.args.clear();
    config.agent.install = Some(AgentInstallConfig {
        install_type: "shell".to_owned(),
        creates: missing_creates.to_string_lossy().into_owned(),
        shell: Some("true".to_owned()),
    });
    fs::write(
        &config_path,
        config.to_canonical_toml().expect("canonical config"),
    )
    .expect("config should be written");

    let state_path = tempdir.path().join(".local/share/acp-stack/state.sqlite");
    let store = acp_stack::state::StateStore::open(&state_path).expect("state opens");
    let failed = store
        .create_init_run(acp_stack::state::NewInitRun {
            runtime_user: None,
            agent_id: Some("placeholder"),
            args_json: "{}",
        })
        .expect("failed run");
    let step = store
        .append_init_step(acp_stack::state::NewInitStep {
            run_id: &failed.id,
            ordinal: 2,
            kind: "agent_install",
            payload_json: "{}",
        })
        .expect("agent install step");
    store.mark_init_step_running(&step.id).expect("running");
    store
        .mark_init_step_failed(
            &step.id,
            None,
            "agent.installer_creates_missing",
            "missing",
            "{}",
        )
        .expect("failed step");
    store
        .finalize_init_run(&failed.id, acp_stack::state::INIT_RUN_FAILED)
        .expect("failed run finalize");
    let failed_id = failed.id.clone();
    drop(store);

    acps_command()
        .env("HOME", tempdir.path())
        .args(["init", "--resume", "--run-id", &failed_id])
        .assert()
        .failure();

    let store = acp_stack::state::StateStore::open(&state_path).expect("state opens");
    let reloaded = store
        .lookup_init_run(&failed_id)
        .expect("lookup")
        .expect("failed row should still exist");
    assert_eq!(reloaded.status, acp_stack::state::INIT_RUN_FAILED);
    let steps = store.query_init_steps(&failed_id).expect("steps");
    let install_step = steps
        .iter()
        .find(|step| step.kind == "agent_install")
        .expect("agent install step");
    assert_eq!(install_step.status, acp_stack::state::INIT_STEP_FAILED);
}

#[test]
fn init_resume_restores_recorded_agent_after_provider_secret_failure() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let workspace = tempdir.path().join("workspace");
    fs::create_dir_all(&workspace).expect("workspace");

    acps_with_empty_path(tempdir.path())
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "opencode",
            "--provider",
            "openai",
            "--api-key-ref",
            "CUSTOM_OPENAI_API_KEY",
            "--workspace-root",
            workspace.to_str().expect("workspace UTF-8"),
            "--skip-workspace-init",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("CUSTOM_OPENAI_API_KEY"));

    let config_before =
        fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
            .expect("config should be readable");
    assert!(config_before.contains(r#"id = "opencode""#));

    seed_init_secrets(
        tempdir.path(),
        &[("CUSTOM_OPENAI_API_KEY", "test-openai-key")],
    );

    acps_with_empty_path(tempdir.path())
        .env("HOME", tempdir.path())
        .args(["init", "--resume"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent: OpenCode (opencode)"));

    let config_after =
        fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
            .expect("config should be readable");
    assert!(config_after.contains(r#"id = "opencode""#));
    assert!(config_after.contains(r#"id = "openai""#));
    assert!(config_after.contains(r#"api_key_ref = "CUSTOM_OPENAI_API_KEY""#));
    assert!(!config_after.contains(r#"api_key_ref = "OPENAI_API_KEY""#));
}

#[test]
fn init_resume_restores_recorded_custom_provider_args_after_secret_failure() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let workspace = tempdir.path().join("workspace");
    fs::create_dir_all(&workspace).expect("workspace");

    acps_with_empty_path(tempdir.path())
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "opencode",
            "--provider",
            "myprovider",
            "--custom-provider",
            "--provider-name",
            "My Provider",
            "--base-url",
            "https://api.myprovider.example/v1",
            "--provider-api",
            "chat-completions",
            "--api-key-ref",
            "MY_PROVIDER_API_KEY",
            "--model",
            "my-model",
            "--model-name",
            "My Model",
            "--context",
            "123456",
            "--output-max-tokens",
            "12345",
            "--workspace-root",
            workspace.to_str().expect("workspace UTF-8"),
            "--skip-workspace-init",
            "--skip-testflight",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("MY_PROVIDER_API_KEY"));

    seed_init_secrets(
        tempdir.path(),
        &[("MY_PROVIDER_API_KEY", "test-provider-key")],
    );

    acps_with_empty_path(tempdir.path())
        .env("HOME", tempdir.path())
        .args(["init", "--resume"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent: OpenCode (opencode)"));

    let config_after =
        fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
            .expect("config should be readable");
    assert!(config_after.contains(r#"id = "myprovider""#));
    assert!(config_after.contains("[agent.provider.custom]"));
    assert!(config_after.contains(r#"name = "My Provider""#));
    assert!(config_after.contains(r#"api_key_ref = "MY_PROVIDER_API_KEY""#));
    assert!(config_after.contains(r#"base_url = "https://api.myprovider.example/v1""#));
    assert!(config_after.contains(r#"api = "chat-completions""#));
    assert!(config_after.contains(r#"model_name = "My Model""#));
    assert!(config_after.contains("context = 123456"));
    assert!(config_after.contains("output_max_tokens = 12345"));
}

#[test]
fn init_resume_without_prior_run_errors_clearly() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    // No prior `acps init` — the resume target doesn't exist.
    acps_command()
        .env("HOME", tempdir.path())
        .args(["init", "--resume"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("no resumable init run"));
}
