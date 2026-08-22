#![cfg(all(feature = "dev-tools", feature = "test-fixtures"))]

//! Shared helpers for the `cli_*_tests` binaries.

use acp_stack::api::{self, AppState, RuntimePaths};
use acp_stack::auth::{AuthVerifierSet, KeyKind};
use acp_stack::config::load_config_from_str;
use acp_stack::dev_gates::TEST_SKIP_AGENT_INSTALL_ENV;
use acp_stack::secrets::{ProviderCredential, ProviderCredentialSet, SecretStore};
use acp_stack::state::{StateStore, default_state_path};
use assert_cmd::Command;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

pub const VALID_CONFIG: &str = include_str!("../fixtures/valid-opencode-stack.toml");

pub const VALID_PLACEBO_CONFIG: &str = include_str!("../fixtures/valid-placebo-stack.toml");

pub const SESSION_KEY: &str = "acps_session_cccccccccccccccccccccccccccccccccccccccccccc";

pub const ADMIN_KEY: &str = "acps_admin_dddddddddddddddddddddddddddddddddddddddddddd";

pub fn acps_command() -> Command {
    let mut command = Command::cargo_bin("acps").expect("binary should build");
    command.env(
        "ACP_STACK_DEV_PLACEBO_REGISTRY",
        env!("CARGO_BIN_EXE_placebo-agent"),
    );
    command.env(TEST_SKIP_AGENT_INSTALL_ENV, "1");
    command
}

pub fn acps_command_without_placebo() -> Command {
    Command::cargo_bin("acps").expect("binary should build")
}

pub fn primary_array_agent_value(config: &toml::Value) -> &toml::Value {
    &config["array"]["targets"][0]["agent"]
}

pub struct AgentCliHarness {
    pub base_url: String,
    pub socket_path: std::path::PathBuf,
    pub config_path: std::path::PathBuf,
    pub state_path: std::path::PathBuf,
    pub join: JoinHandle<acp_stack::error::Result<()>>,
    pub local_join: JoinHandle<acp_stack::error::Result<()>>,
    _tempdir: TempDir,
}

impl AgentCliHarness {
    pub async fn spawn() -> Self {
        Self::spawn_inner(None).await
    }

    /// Spawn a harness that reports a custom `effective_bind` to the security
    /// check. Used to drive findings like `api.public_bind` from the CLI side
    /// without rewriting the actual TCP bind (we always bind to `127.0.0.1:0`
    /// for the test listener).
    pub async fn spawn_with_effective_bind(effective_bind: &str) -> Self {
        Self::spawn_inner(Some(effective_bind.to_owned())).await
    }

    pub async fn spawn_inner(effective_bind: Option<String>) -> Self {
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

pub fn create_runtime_files(
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

pub fn write_cli_home(home: &std::path::Path, base_url: &str, admin_key: &str) {
    write_cli_home_with_socket(home, base_url, admin_key, None);
}

pub fn write_cli_home_with_socket(
    home: &std::path::Path,
    base_url: &str,
    admin_key: &str,
    socket_path: Option<&std::path::Path>,
) {
    write_cli_home_with_socket_and_session_auth(home, base_url, admin_key, socket_path, None);
}

pub fn write_cli_home_with_socket_and_session_auth(
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

pub fn seed_auth_verifiers(home: &std::path::Path, session_key: &str, admin_key: &str) {
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

pub fn seed_init_secrets(home: &std::path::Path, extra: &[(&str, &str)]) {
    seed_auth_verifiers(home, SESSION_KEY, ADMIN_KEY);
    let mut store = SecretStore::open_or_create(home).expect("secret store should open");
    store
        .set_many(extra.iter().copied())
        .expect("secrets should be stored");
}

pub fn seed_provider_credential(home: &std::path::Path, provider_id: &str, env_names: &[&str]) {
    let mut store = SecretStore::open_or_create(home).expect("secret store should open");
    let values = env_names
        .iter()
        .map(|name| ((*name).to_owned(), format!("test-{name}")))
        .collect::<BTreeMap<_, _>>();
    store
        .set_many(
            values
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str())),
        )
        .expect("flat test secrets should be stored");
    let mut catalog = store.provider_credentials().clone();
    catalog.insert(
        provider_id.to_owned(),
        ProviderCredentialSet::aliasless(ProviderCredential::new(values, BTreeMap::new())),
    );
    store
        .replace_provider_credentials(catalog, &[])
        .expect("provider credential should be stored");
}

/// Seed a catalog credential WITHOUT the flat-store copy, mirroring what a
/// managed-state apply leaves behind: the value lives only in the structured
/// catalog.
pub fn seed_catalog_only_provider_credential(
    home: &std::path::Path,
    provider_id: &str,
    values: &[(&str, &str)],
) {
    let mut store = SecretStore::open_or_create(home).expect("secret store should open");
    let values = values
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect::<BTreeMap<_, _>>();
    let mut catalog = store.provider_credentials().clone();
    catalog.insert(
        provider_id.to_owned(),
        ProviderCredentialSet::aliasless(ProviderCredential::new(values, BTreeMap::new())),
    );
    store
        .replace_provider_credentials(catalog, &[])
        .expect("provider credential should be stored");
}

pub fn toml_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

pub fn acps_with_empty_path(home: &std::path::Path) -> Command {
    let empty_bin = home.join("empty-bin");
    fs::create_dir_all(&empty_bin).expect("empty PATH dir");
    let mut command = acps_command();
    command.env("PATH", empty_bin);
    command
}

#[cfg(unix)]
pub fn mode(path: &std::path::Path) -> u32 {
    fs::metadata(path)
        .expect("metadata should be readable")
        .permissions()
        .mode()
        & 0o777
}

pub fn shell_quote_path(path: &std::path::Path) -> String {
    let text = path.to_string_lossy();
    format!("'{}'", text.replace('\'', "'\\''"))
}

pub fn codex_config() -> String {
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

pub fn claude_settings(home: &std::path::Path) -> Value {
    serde_json::from_str(
        &fs::read_to_string(home.join(".claude").join("settings.json"))
            .expect("Claude settings should be readable"),
    )
    .expect("Claude settings should parse")
}

pub fn write_acp_config_options(
    root: &std::path::Path,
    models: &[&str],
    modes: &[&str],
) -> std::path::PathBuf {
    write_acp_config_options_with_efforts(root, models, modes, &[])
}

pub fn write_acp_config_options_with_efforts(
    root: &std::path::Path,
    models: &[&str],
    modes: &[&str],
    efforts: &[&str],
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
    if !efforts.is_empty() {
        // codex-acp's shape: a non-"effort" option id under the reserved
        // `thought_level` category, so the fixture exercises the category
        // match rather than the id fallback.
        options.push(serde_json::json!({
            "id": "reasoning_effort",
            "name": "Reasoning Effort",
            "category": "thought_level",
            "type": "select",
            "currentValue": efforts[0],
            "options": efforts
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
