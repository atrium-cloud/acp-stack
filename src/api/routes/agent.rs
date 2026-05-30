use axum::Json;
use axum::extract::State;
use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

use super::super::core::{AppState, load_active_registry, populate_agent_adapter_from_registry};
use crate::config::{AgentAdapterConfig, Config};
use crate::envelope::ApiSuccess;
use crate::error::{Result, StackError};
use crate::fs_util::home_dir;
use crate::runtime::agent::acp_bridge::{AgentCapabilitiesDto, AgentSessionConfigCategory};
use crate::runtime::agent::agent_headless_config::{
    CleanedAgentConfig, ProvisionedAgentConfig, cleanup_agent_headless_config,
};
use crate::runtime::agent::model_discovery::{
    DEFAULT_MODELS_DISCOVERY_TIMEOUT, advertised_values_for_category,
    fetch_session_config_with_timeout,
};
use crate::runtime::agent::supervisor::AgentSnapshot;
use crate::runtime::agent::switch::{
    AgentSwitchRequest as PlannedAgentSwitchRequest, AgentSwitchSecretMigration,
    adapter_from_registry_entry, plan_agent_switch,
};
use crate::runtime::install::agent_installer::{
    InstallerSequenceResult, install_resolved_capture, run_installer_capture,
};
use crate::runtime::install::agent_registry::RegistryCatalog;
use crate::runtime::install::skill_installer::{SkillPortReport, port_agent_skills};
use crate::secrets::SecretStore;
use crate::state::InstallerRunInput;

#[derive(Serialize)]
pub(crate) struct AgentInstallResponse {
    outcome: &'static str,
    path: String,
    sha256: String,
}

pub(crate) async fn agent_install_handler(
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<AgentInstallResponse>, StackError> {
    let agent = state.live_agent_config.lock().await.clone();
    let mut config = (*state.config).clone();
    config.agent = agent;
    install_agent_for_config(&state, &config)
        .await
        .map(ApiSuccess::new)
}

async fn install_agent_for_config(
    state: &AppState,
    config: &Config,
) -> Result<AgentInstallResponse> {
    let workspace_root = std::path::PathBuf::from(config.workspace.root.clone());
    let home = home_dir()?;
    let local_bin = home.join(".local").join("bin");
    let log_base = crate::state::default_installer_log_base(&home);

    let outcome = if let Some(install) = config.agent.install.clone() {
        // Escape-hatch shell recipe. One row, persisted after the shell runs.
        let env = open_agent_env(config)?;
        let expected_sha256 = config.agent.expected_sha256.clone();
        let mut result = tokio::task::spawn_blocking(move || {
            run_installer_capture(&install, expected_sha256.as_deref(), env, &workspace_root)
        })
        .await
        .map_err(|err| StackError::AgentInitializeFailed {
            reason: format!("installer thread join failed: {err}"),
        })?;
        crate::runtime::install::agent_installer::persist_step_logs_to_disk(
            &mut result.row,
            &config.agent.id,
            Some(&log_base),
        )?;
        {
            let store = state.state.lock().await;
            store.append_installer_run(InstallerRunInput {
                agent_id: &config.agent.id,
                started_at: &result.row.started_at,
                finished_at: result.row.finished_at.as_deref(),
                status: &result.row.status,
                stdout: &result.row.stdout,
                stderr: &result.row.stderr,
                exit_status: result.row.exit_status,
                step: &result.row.step,
                version: result.row.version.as_deref(),
                log_dir: result.row.log_dir.as_deref(),
                apply_run_id: None,
            })?;
        }
        result.outcome?
    } else {
        // Registry-resolved install: one row for native, two for adapter-backed.
        let override_path = home.join(".config").join("acp-stack").join("agents.toml");
        let registry = RegistryCatalog::load_with_override(&override_path)?;
        let entry = registry.lookup_required(&config.agent.id)?.clone();
        let agent = config.agent.clone();
        let mut result: InstallerSequenceResult = tokio::task::spawn_blocking(move || {
            install_resolved_capture(
                &agent,
                &entry,
                Default::default(),
                &workspace_root,
                &local_bin,
            )
        })
        .await
        .map_err(|err| StackError::AgentInitializeFailed {
            reason: format!("installer thread join failed: {err}"),
        })?;
        for row in result.rows.iter_mut() {
            crate::runtime::install::agent_installer::persist_step_logs_to_disk(
                row,
                &config.agent.id,
                Some(&log_base),
            )?;
        }
        {
            let store = state.state.lock().await;
            for row in &result.rows {
                store.append_installer_run(InstallerRunInput {
                    agent_id: &config.agent.id,
                    started_at: &row.started_at,
                    finished_at: row.finished_at.as_deref(),
                    status: &row.status,
                    stdout: &row.stdout,
                    stderr: &row.stderr,
                    exit_status: row.exit_status,
                    step: &row.step,
                    version: row.version.as_deref(),
                    log_dir: row.log_dir.as_deref(),
                    apply_run_id: None,
                })?;
            }
        }
        result.outcome?
    };

    let outcome_label = outcome.label();
    let path = outcome.path().to_string_lossy().into_owned();
    let sha256 = outcome.sha256().to_owned();
    Ok(AgentInstallResponse {
        outcome: outcome_label,
        path,
        sha256,
    })
}

pub(super) fn open_agent_env(config: &Config) -> Result<std::collections::HashMap<String, String>> {
    if config.agent.env.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let home = home_dir()?;
    let store = SecretStore::open(&home)?;
    let mut env = std::collections::HashMap::with_capacity(config.agent.env.len());
    for name in &config.agent.env {
        let value = store.get(name)?;
        env.insert(name.clone(), value.to_owned());
    }
    Ok(env)
}

async fn load_fresh_config_with_runtime_agent_metadata(state: &AppState) -> Result<Config> {
    let mut config = Config::load_from_path(&state.runtime_paths.config_path)?;
    let live_agent = state.live_agent_config.lock().await.clone();
    if config.agent.id == live_agent.id && config.agent.adapter.is_none() {
        config.agent.adapter = live_agent.adapter;
    }
    if config.agent.adapter.is_none()
        && let Ok(registry) = load_active_registry()
    {
        populate_agent_adapter_from_registry(&mut config, &registry);
    }
    Ok(config)
}

/// Resolve every configured `[mcp.servers]` entry into the SDK `McpServer`
/// type. Returns an empty Vec when no MCP servers are configured, so the
/// secret store is only opened when there's something to resolve.
pub(super) fn open_mcp_servers(
    config: &Config,
) -> Result<Vec<agent_client_protocol::schema::McpServer>> {
    if config.mcp.servers.is_empty() {
        return Ok(Vec::new());
    }
    let home = home_dir()?;
    let store = SecretStore::open(&home)?;
    crate::runtime::agent::mcp::resolve_mcp_servers(&config.mcp, &store)
}

#[derive(Serialize)]
pub(crate) struct AgentStartResponse {
    started_at: String,
    capabilities: AgentCapabilitiesDto,
    pid: Option<u32>,
}

pub(crate) async fn agent_start_handler(
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<AgentStartResponse>, StackError> {
    // Re-read disk config and resolve env BEFORE invoking the supervisor so
    // `acps agent set` changes made while the daemon is running are honored
    // by the next start. open_agent_env enforces the same allowlist semantics
    // (security.md:91) regardless of caller.
    let config = load_fresh_config_with_runtime_agent_metadata(&state).await?;
    let env = open_agent_env(&config)?;
    let capabilities = state
        .agent_supervisor
        .start(
            &config.agent,
            &config.workspace.root,
            env,
            &state.state,
            state.event_hub.clone(),
            Some(state.permissions.clone()),
        )
        .await?;
    {
        let mut live = state.live_agent_config.lock().await;
        *live = config.agent.clone();
    }
    let started_at = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
    let pid = state.agent_supervisor.snapshot().await.pid;
    Ok(ApiSuccess::new(AgentStartResponse {
        started_at,
        capabilities,
        pid,
    }))
}

#[derive(Serialize)]
pub(crate) struct AgentStopResponse {
    stopped_at: String,
    exit_status: Option<i32>,
}

pub(crate) async fn agent_stop_handler(
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<AgentStopResponse>, StackError> {
    let exit_status = state
        .agent_supervisor
        .stop(&state.state, &state.event_hub)
        .await?;
    let stopped_at = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
    Ok(ApiSuccess::new(AgentStopResponse {
        stopped_at,
        exit_status,
    }))
}

#[derive(Serialize)]
pub(crate) struct AgentRestartResponse {
    stopped_at: String,
    started_at: String,
    /// Exit status of the prior process. `None` when the supervisor
    /// was not running (the restart degenerated into a plain start).
    prior_exit_status: Option<i32>,
    capabilities: AgentCapabilitiesDto,
    pid: Option<u32>,
}

/// Stop the supervised agent (if running) and start it again, reading
/// the freshly-on-disk `[agent]` block instead of the daemon's
/// in-memory `Arc<Config>` snapshot. Used by operators after
/// `acps agent set` writes provider/model changes that require a
/// process-level config reload — agents that read provider/model from
/// their on-disk config at process start can only see updated values
/// after a restart. Goose model changes do NOT need this endpoint;
/// clients can switch live via `session/set_config_option`.
///
/// This endpoint also refreshes the daemon's live agent cache so
/// status, capabilities, and subsequent session creation observe the
/// same `[agent]` block used to spawn the supervised process.
pub(crate) async fn agent_restart_handler(
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<AgentRestartResponse>, StackError> {
    // Load + validate the fresh on-disk config AND resolve env BEFORE
    // stopping the currently running agent. A malformed config or a
    // missing required secret should fail this call cleanly and leave
    // the running agent alone, rather than taking it down and
    // returning an error with no agent running at all.
    let fresh_config = load_fresh_config_with_runtime_agent_metadata(&state).await?;
    let env = open_agent_env(&fresh_config)?;

    // Now safe to stop the prior process. `stop` returns
    // `Result<Option<i32>, _>`: outer `Err(AgentNotRunning)` means
    // there was nothing to stop (acceptable — a "restart" against a
    // stopped agent degenerates into a plain start); inner
    // `Option<i32>` is the optional exit status of the prior process.
    let prior_exit_status = match state
        .agent_supervisor
        .stop(&state.state, &state.event_hub)
        .await
    {
        Ok(code) => code,
        Err(StackError::AgentNotRunning) => None,
        Err(err) => return Err(err),
    };
    let stopped_at = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);

    // Update the live agent-config cache so post-restart session
    // creation (which reads `state.live_agent_config` for
    // `agent.mode`/`agent.model`/`agent.provider`) sees the new
    // values too. Without this, the supervised process would be on
    // the new binary/command but `/v1/sessions` would still apply
    // the stale model — silently giving operators the wrong agent
    // behavior after a `acps agent set`.
    {
        let mut live = state.live_agent_config.lock().await;
        *live = fresh_config.agent.clone();
    }
    let capabilities = state
        .agent_supervisor
        .start(
            &fresh_config.agent,
            &fresh_config.workspace.root,
            env,
            &state.state,
            state.event_hub.clone(),
            Some(state.permissions.clone()),
        )
        .await?;
    let started_at = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
    let pid = state.agent_supervisor.snapshot().await.pid;

    Ok(ApiSuccess::new(AgentRestartResponse {
        stopped_at,
        started_at,
        prior_exit_status,
        capabilities,
        pid,
    }))
}

#[derive(Debug, Deserialize)]
pub(crate) struct AgentSwitchRequest {
    agent: String,
    #[serde(default, rename = "drop")]
    drop_configs: bool,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    api_key_ref: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct AgentSwitchResponse {
    old_agent_id: String,
    agent_id: String,
    provider_status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    api_key_ref: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    required_env_refs: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    secret_migrations: Vec<AgentSwitchSecretMigrationJson>,
    install: AgentInstallResponse,
    restarted: bool,
    restart_started: bool,
    set_model: bool,
    models: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    follow_up: Option<&'static str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    provisioned: Vec<ProvisionedAgentConfigJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    skills_port: Option<SkillPortReport>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    cleaned_configs: Vec<CleanedAgentConfigJson>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    cleanup_errors: Vec<String>,
}

#[derive(Serialize)]
struct ProvisionedAgentConfigJson {
    label: &'static str,
    path: String,
}

#[derive(Serialize)]
struct CleanedAgentConfigJson {
    label: &'static str,
    path: String,
}

#[derive(Serialize)]
struct AgentSwitchSecretMigrationJson {
    from_ref: String,
    to_ref: String,
}

impl From<ProvisionedAgentConfig> for ProvisionedAgentConfigJson {
    fn from(value: ProvisionedAgentConfig) -> Self {
        Self {
            label: value.label,
            path: value.path.to_string_lossy().into_owned(),
        }
    }
}

impl From<CleanedAgentConfig> for CleanedAgentConfigJson {
    fn from(value: CleanedAgentConfig) -> Self {
        Self {
            label: value.label,
            path: value.path.to_string_lossy().into_owned(),
        }
    }
}

pub(crate) async fn agent_switch_handler(
    State(state): State<AppState>,
    Json(body): Json<AgentSwitchRequest>,
) -> std::result::Result<ApiSuccess<AgentSwitchResponse>, StackError> {
    let home = home_dir()?;
    let fresh_config = Config::load_from_path(&state.runtime_paths.config_path)?;
    let registry = RegistryCatalog::load_with_override(
        &home.join(".config").join("acp-stack").join("agents.toml"),
    )?;
    let plan = plan_agent_switch(
        &fresh_config,
        &registry,
        PlannedAgentSwitchRequest {
            target_agent: body.agent,
            provider_id: body.provider,
            api_key_ref: body.api_key_ref,
        },
    )?;
    let target_entry = registry.lookup_required(&plan.target_agent_id)?;
    let mut candidate_config = plan.config.clone();
    candidate_config.agent.adapter = adapter_from_registry_entry(target_entry);

    let canonical = candidate_config.to_canonical_toml()?;
    let mut candidate_config = crate::config::load_config_from_str(&canonical)?;
    candidate_config.agent.adapter = adapter_from_registry_entry(target_entry);
    let secret_migrations = apply_switch_secret_migrations(&home, &plan.secret_migrations)?;
    let _env = open_agent_env(&candidate_config)?;

    let install = install_agent_for_config(&state, &candidate_config).await?;
    let provisioned =
        crate::runtime::agent::agent_headless_config::provision_agent_headless_config(
            &candidate_config,
            &home,
        )?
        .into_iter()
        .map(ProvisionedAgentConfigJson::from)
        .collect::<Vec<_>>();

    let models = if target_entry.set_model {
        let response = fetch_session_config_with_timeout(
            &home,
            &candidate_config,
            DEFAULT_MODELS_DISCOVERY_TIMEOUT,
        )
        .await?;
        advertised_values_for_category(&response, AgentSessionConfigCategory::Model)?
    } else {
        Vec::new()
    };
    let skills_port = port_agent_skills(
        &home,
        &registry,
        &fresh_config.agent.id,
        &candidate_config.agent.id,
    )?;

    let was_running = state.agent_supervisor.snapshot().await.state.as_wire_str() == "running";
    crate::fs_util::atomic_write_owner_only(
        &state.runtime_paths.config_path,
        canonical.as_bytes(),
    )?;
    {
        let mut live = state.live_agent_config.lock().await;
        *live = candidate_config.agent.clone();
    }
    let mut restart_started = false;
    if was_running {
        restart_agent_with_config(&state, &candidate_config).await?;
        restart_started = true;
    }
    let (cleaned_configs, cleanup_errors) = if body.drop_configs {
        match cleanup_agent_headless_config(&fresh_config, &home) {
            Ok(cleaned) => (
                cleaned
                    .into_iter()
                    .map(CleanedAgentConfigJson::from)
                    .collect(),
                Vec::new(),
            ),
            Err(err) => {
                tracing::warn!(error = %err, "source agent config cleanup failed after switch");
                (Vec::new(), vec![err.to_string()])
            }
        }
    } else {
        (Vec::new(), Vec::new())
    };

    let response = AgentSwitchResponse {
        old_agent_id: plan.old_agent_id,
        agent_id: plan.target_agent_id,
        provider_status: plan.provider_status.label(),
        provider: plan.provider_status.provider_id().map(str::to_owned),
        api_key_ref: plan.provider_status.api_key_ref().map(str::to_owned),
        required_env_refs: plan.required_env_refs,
        secret_migrations,
        install,
        restarted: was_running,
        restart_started,
        set_model: target_entry.set_model,
        models,
        follow_up: target_entry
            .set_model
            .then_some("acps agent set --model <model-id>"),
        provisioned,
        skills_port,
        cleaned_configs,
        cleanup_errors,
    };
    Ok(ApiSuccess::new(response))
}

fn apply_switch_secret_migrations(
    home: &std::path::Path,
    migrations: &[AgentSwitchSecretMigration],
) -> Result<Vec<AgentSwitchSecretMigrationJson>> {
    if migrations.is_empty() {
        return Ok(Vec::new());
    }
    let mut store = SecretStore::open(home)?;
    let mut applied = Vec::with_capacity(migrations.len());
    for migration in migrations {
        let value = store.get(&migration.from_ref)?.to_owned();
        if !store.contains(&migration.to_ref) {
            store.set(&migration.to_ref, &value)?;
        }
        applied.push(AgentSwitchSecretMigrationJson {
            from_ref: migration.from_ref.clone(),
            to_ref: migration.to_ref.clone(),
        });
    }
    Ok(applied)
}

async fn restart_agent_with_config(state: &AppState, config: &Config) -> Result<()> {
    let env = open_agent_env(config)?;
    match state
        .agent_supervisor
        .stop(&state.state, &state.event_hub)
        .await
    {
        Ok(_) | Err(StackError::AgentNotRunning) => {}
        Err(err) => return Err(err),
    }
    state
        .agent_supervisor
        .start(
            &config.agent,
            &config.workspace.root,
            env,
            &state.state,
            state.event_hub.clone(),
            Some(state.permissions.clone()),
        )
        .await?;
    Ok(())
}

#[derive(Serialize)]
pub(crate) struct AgentCapabilitiesResponseBody {
    agent_id: String,
    adapter: Option<AgentAdapterConfig>,
    captured_at: String,
    capabilities: serde_json::Value,
    process_state: String,
}

pub(crate) async fn agent_capabilities_handler(
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<AgentCapabilitiesResponseBody>, StackError> {
    let agent = state.live_agent_config.lock().await.clone();
    let agent_id = agent.id.clone();
    let snapshot: AgentSnapshot = state.agent_supervisor.snapshot().await;
    let store = state.state.lock().await;
    let record = store.latest_agent_capabilities(&agent_id)?;
    drop(store);
    let record = record.ok_or(StackError::AgentNotInitialized)?;
    let capabilities = serde_json::from_str(&record.capabilities_json).map_err(|err| {
        StackError::AgentInitializeFailed {
            reason: format!("stored capabilities are unparseable: {err}"),
        }
    })?;
    Ok(ApiSuccess::new(AgentCapabilitiesResponseBody {
        agent_id: record.agent_id,
        adapter: agent.adapter,
        captured_at: record.captured_at,
        capabilities,
        process_state: format!("{:?}", snapshot.state).to_lowercase(),
    }))
}
