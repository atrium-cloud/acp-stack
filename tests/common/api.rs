//! Shared fixtures for the `api_*_tests` binaries: an in-process server
//! harness plus the SQLite seeding helpers its tests assert against.

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use acp_stack::api::{self, AppState, RuntimePaths};
use acp_stack::auth::AuthVerifierSet;
use acp_stack::config::{AgentAdapterConfig, Config, LocalSessionAuth, load_config_from_str};
use acp_stack::secrets::SecretStore;
use acp_stack::state::{AuthFailureFilter, StateStore};
use rusqlite::Connection;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::Mutex as TokioMutex;
use tokio::task::JoinHandle;

pub const SESSION_KEY: &str = "acps_session_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
pub const ADMIN_KEY: &str = "acps_admin_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

pub struct ServerHarness {
    pub base_url: String,
    pub state: Arc<TokioMutex<StateStore>>,
    pub local_session_auth: Arc<tokio::sync::RwLock<LocalSessionAuth>>,
    pub config_path: PathBuf,
    pub state_path: PathBuf,
    // Public so a test binary that needs a bespoke `AppState` (e.g. an
    // effective bind that differs from `config.api.bind`) can still build a
    // harness by struct literal rather than through the `spawn*` helpers.
    pub join: JoinHandle<acp_stack::error::Result<()>>,
    pub _tempdir: TempDir,
}

impl ServerHarness {
    pub async fn spawn() -> Self {
        Self::spawn_with_config(test_config()).await
    }

    pub async fn spawn_with_config(mut config: Config) -> Self {
        let tempdir = tempfile::tempdir().expect("tempdir");
        // Repoint workspace.root at the tempdir so the security-check route's
        // workspace-writability probe (Phase 4: runtime.workspace_not_writable)
        // sees a real, writable directory rather than the fixture's
        // "/workspace" placeholder.
        let workspace_root = tempdir.path().join("workspace");
        std::fs::create_dir_all(&workspace_root).expect("create workspace");
        config.workspace.root = workspace_root.to_string_lossy().into_owned();
        config.workspace.uploads = workspace_root
            .join("uploads")
            .to_string_lossy()
            .into_owned();
        if let Some(user) = acp_stack::ownership::current_username()
            .expect("resolve current username for security fixture")
        {
            config.workspace.runtime_user = user;
        }
        std::fs::create_dir_all(workspace_root.join("uploads")).expect("create uploads");
        Self::spawn_with_prepared_config(config, tempdir).await
    }

    /// Like `spawn_with_config` but does not rewrite `workspace.root`. Use this
    /// when a test deliberately needs the workspace path to come from the
    /// passed-in `Config` — e.g. exercising the "workspace not writable" path
    /// in `/v1/health/ready`.
    pub async fn spawn_with_unmodified_workspace(config: Config) -> Self {
        let tempdir = tempfile::tempdir().expect("tempdir");
        Self::spawn_with_prepared_config(config, tempdir).await
    }

    pub async fn spawn_with_prepared_config(config: Config, tempdir: TempDir) -> Self {
        let path = tempdir.path().join("state.sqlite");
        let store = StateStore::open(&path).expect("state open");
        store.migrate().expect("migrate");
        store
            .insert_auth_key_pair(&AuthVerifierSet::create(SESSION_KEY, ADMIN_KEY))
            .expect("seed auth verifiers");
        let config_path = create_runtime_files(tempdir.path(), &path);
        std::fs::write(
            &config_path,
            config.to_canonical_toml().expect("canonical test config"),
        )
        .expect("write runtime config");
        let runtime_paths = RuntimePaths::new(config_path.clone(), path.clone());
        let effective_bind = config.api.bind.clone();
        let app_state = AppState::with_effective_bind_and_runtime_paths(
            config,
            store,
            SESSION_KEY.to_owned(),
            ADMIN_KEY.to_owned(),
            effective_bind,
            runtime_paths,
        );
        let state = app_state.state.clone();
        let local_session_auth = app_state.local_session_auth.clone();
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let local = listener.local_addr().expect("local addr");
        let join = tokio::spawn(async move { api::serve(app_state, listener).await });
        Self {
            base_url: format!("http://{local}"),
            state,
            local_session_auth,
            config_path,
            state_path: path,
            join,
            _tempdir: tempdir,
        }
    }

    pub async fn auth_failure_count(&self) -> usize {
        let guard = self.state.lock().await;
        guard
            .query_auth_failures(AuthFailureFilter {
                limit: 100,
                ..AuthFailureFilter::default()
            })
            .expect("query auth failures")
            .len()
    }

    pub async fn latest_auth_failure(&self) -> (String, String) {
        let guard = self.state.lock().await;
        let rows = guard
            .query_auth_failures(AuthFailureFilter {
                limit: 1,
                ..AuthFailureFilter::default()
            })
            .expect("query auth failures");
        let row = rows.into_iter().next().expect("at least one auth failure");
        (row.key_kind, row.reason)
    }

    pub async fn latest_auth_failure_client_ip(&self) -> Option<String> {
        let guard = self.state.lock().await;
        let rows = guard
            .query_auth_failures(AuthFailureFilter {
                limit: 1,
                ..AuthFailureFilter::default()
            })
            .expect("query auth failures");
        rows.into_iter()
            .next()
            .expect("at least one auth failure")
            .client_ip
    }
}

impl Drop for ServerHarness {
    fn drop(&mut self) {
        self.join.abort();
    }
}

pub fn create_runtime_files(root: &Path, state_path: &Path) -> PathBuf {
    let config_dir = root.join(".config/acp-stack");
    let state_dir = state_path.parent().expect("state parent").to_path_buf();
    std::fs::create_dir_all(&config_dir).expect("create config dir");
    std::fs::create_dir_all(&state_dir).expect("create state dir");

    let config_path = config_dir.join("acps-config.toml");
    let age_key_path = config_dir.join("age.key");
    let secret_store_path = state_dir.join("secrets.age");
    std::fs::write(&config_path, "test config").expect("write config file");
    SecretStore::open_or_create_at_paths(&age_key_path, &secret_store_path)
        .expect("create secret store");

    #[cfg(unix)]
    {
        std::fs::set_permissions(&config_dir, std::fs::Permissions::from_mode(0o700))
            .expect("chmod config dir");
        std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o700))
            .expect("chmod state dir");
        for file in [&config_path, &age_key_path, state_path, &secret_store_path] {
            std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o600))
                .expect("chmod runtime file");
        }
    }

    config_path
}

pub fn test_config() -> Config {
    let toml_text = include_str!("../fixtures/valid-placebo-stack.toml");
    load_config_from_str(toml_text).expect("config parses")
}

pub fn codex_adapter() -> AgentAdapterConfig {
    AgentAdapterConfig {
        id: "codex-acp".to_owned(),
        name: "Codex ACP Adapter".to_owned(),
        upstream_agent: "codex-cli".to_owned(),
        source_url: Some("https://github.com/agentclientprotocol/codex-acp".to_owned()),
    }
}

pub fn seed_session(path: &Path, id: &str, status: &str, created_at: &str, updated_at: &str) {
    let connection = Connection::open(path).expect("open sqlite for seed");
    connection
        .execute(
            r#"
            INSERT INTO sessions (id, target_id, agent_session_id, created_at, updated_at, status)
            VALUES (?1, 'opencode', ?1, ?2, ?3, ?4)
            "#,
            (id, created_at, updated_at, status),
        )
        .expect("insert session");
}

pub fn seed_command(
    path: &Path,
    id: &str,
    status: &str,
    command: &str,
    exit_status: Option<i64>,
    created_at: &str,
    updated_at: &str,
) {
    let connection = Connection::open(path).expect("open sqlite for seed");
    connection
        .execute(
            r#"
            INSERT INTO commands (id, created_at, updated_at, status, command, exit_status)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            "#,
            (id, created_at, updated_at, status, command, exit_status),
        )
        .expect("insert command");
}

pub fn seed_auth_failure(path: &Path, id: &str, created_at: &str, reason: &str) {
    let connection = Connection::open(path).expect("open sqlite for seed");
    connection
        .execute(
            r#"
            INSERT INTO auth_failures
                (id, created_at, key_kind, reason, client_ip, route, payload_json)
            VALUES (?1, ?2, 'unknown', ?3, NULL, '/v1/status', '{}')
            "#,
            (id, created_at, reason),
        )
        .expect("insert auth failure");
}
