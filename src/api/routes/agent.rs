use std::time::Duration;

use axum::Json;
use axum::extract::{Path, Query, State};
use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

use super::super::core::{AgentTargetRuntime, AppState, load_active_registry};
use crate::config::{AgentAdapterConfig, Config, LocalSessionAuth};
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
use crate::runtime::agent::provider_keys::{
    ResolvedAgentEnvironment, resolve_agent_environment, resolve_agent_environment_without_secrets,
};
use crate::runtime::agent::supervisor::{
    AGENT_RESTART_NEVER, AgentSnapshot, AgentStartReadiness, AgentStartRequest,
};
use crate::runtime::agent::switch::{
    AgentSwitchRequest as PlannedAgentSwitchRequest, AgentSwitchSecretMigration,
    adapter_from_registry_entry, plan_agent_switch,
};
use crate::runtime::install::agent_installer::{
    InstallProgress, InstallerSequenceResult, SharedInstallerSink, install_resolved_capture,
    persist_untracked_installer_row, run_installer_capture,
};
use crate::runtime::install::agent_registry::RegistryCatalog;
use crate::runtime::install::skill_installer::{
    SkillLinkReport, SkillPortReport, link_agent_skills_best_effort, port_agent_skills,
};
use crate::runtime::workspace_sources::workspace_init::prepare_workspace_base_dirs;
use crate::secrets::SecretStore;

mod lifecycle;
mod switch;
mod update;

// Cross-seam helpers keep their visibility; re-import them here so each
// sibling's `use super::*;` resolves items defined in the other siblings, and
// re-export the handlers/helpers the router and other routes reference by path.
pub(crate) use self::lifecycle::{
    agent_restart_blockers_handler, agent_restart_handler, agent_start_handler, agent_stop_handler,
    array_agent_restart_handler, array_agent_start_handler, array_agent_stop_handler,
    cancel_pending_acp_permissions_for_target, ensure_agent_started,
};
pub(crate) use self::switch::agent_switch_handler;
pub(crate) use self::update::{agent_update_handler, agent_update_status_handler};

#[derive(Serialize)]
pub(crate) struct AgentInstallResponse {
    outcome: &'static str,
    path: String,
    sha256: String,
}

pub(crate) async fn agent_install_handler(
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<AgentInstallResponse>, StackError> {
    let target_id = state.default_target_id().await?;
    install_agent_target(&state, &target_id).await
}

#[derive(Serialize)]
pub(crate) struct ArrayStatusResponse {
    enabled: bool,
    primary_target: String,
    delegation: ArrayDelegationStatusResponse,
    targets: Vec<ArrayTargetStatusResponse>,
}

#[derive(Serialize)]
struct ArrayDelegationStatusResponse {
    ready: bool,
    local_session_auth: &'static str,
}

#[derive(Serialize)]
struct ArrayTargetStatusResponse {
    id: String,
    agent_id: String,
    name: String,
    primary: bool,
    process_state: String,
    pid: Option<u32>,
    configured_providers: Vec<super::status::ProviderStatusJson>,
    loaded_providers: Option<Vec<super::status::ProviderStatusJson>>,
    provider_restart_required: bool,
    /// Set when this target's provider resolution fails; other targets keep
    /// reporting so one broken credential never aborts the whole fleet status.
    provider_error: Option<String>,
}

pub(crate) async fn array_status_handler(
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<ArrayStatusResponse>, StackError> {
    let config = state.refresh_array_runtime_from_disk().await?;
    let local_session_auth = state.local_session_auth().await;
    let mut targets = Vec::with_capacity(config.array.targets.len());
    for target_config in &config.array.targets {
        let target = state.agent_target(&target_config.id)?;
        let snapshot = target.supervisor.snapshot().await;
        let mut resolved_config = config.clone();
        resolved_config.agent = target_config.agent.clone();
        let (configured_provider_snapshot, provider_error) =
            super::status::configured_providers_or_error(open_agent_environment(&resolved_config));
        let provider_restart_required = super::status::provider_restart_required_for_status(
            provider_error.is_some(),
            snapshot.state,
            snapshot.loaded_providers.as_deref(),
            &configured_provider_snapshot,
        );
        targets.push(ArrayTargetStatusResponse {
            id: target_config.id.clone(),
            agent_id: target_config.agent.id.clone(),
            name: target_config.agent.name.clone(),
            primary: target_config.id == config.array.primary_target,
            process_state: snapshot.state.as_wire_str().to_owned(),
            pid: snapshot.pid,
            configured_providers: configured_provider_snapshot
                .iter()
                .map(super::status::ProviderStatusJson::from)
                .collect(),
            loaded_providers: snapshot.loaded_providers.as_ref().map(|providers| {
                providers
                    .iter()
                    .map(super::status::ProviderStatusJson::from)
                    .collect()
            }),
            provider_restart_required,
            provider_error,
        });
    }
    Ok(ApiSuccess::new(ArrayStatusResponse {
        enabled: config.array.enabled,
        primary_target: config.array.primary_target,
        delegation: ArrayDelegationStatusResponse {
            ready: local_session_auth == LocalSessionAuth::Keyless,
            local_session_auth: local_session_auth.as_str(),
        },
        targets,
    }))
}

pub(crate) async fn array_agent_install_handler(
    State(state): State<AppState>,
    Path(target_id): Path<String>,
) -> std::result::Result<ApiSuccess<AgentInstallResponse>, StackError> {
    install_agent_target(&state, &target_id).await
}

async fn install_agent_target(
    state: &AppState,
    target_id: &str,
) -> std::result::Result<ApiSuccess<AgentInstallResponse>, StackError> {
    let (config, _) = load_fresh_config_for_target(state, target_id).await?;
    install_agent_for_config(state, &config)
        .await
        .map(ApiSuccess::new)
}

async fn install_agent_for_config(
    state: &AppState,
    config: &Config,
) -> Result<AgentInstallResponse> {
    prepare_workspace_base_dirs(&config.workspace)?;
    let workspace_root = std::path::PathBuf::from(config.workspace.root.clone());
    let home = home_dir()?;
    let local_bin = home.join(".local").join("bin");
    let log_base = crate::state::default_installer_log_base(&home);

    let outcome = if let Some(install) = config.agent.install.clone() {
        // Escape-hatch shell recipe. The step's `running` row is inserted when
        // the shell starts and finalized in place when it exits, so polling
        // readers see the in-flight install; anything the sink did not
        // finalize (skipped rows) is appended after the run.
        let env = open_agent_env(config)?;
        let expected_sha256 = config.agent.expected_sha256.clone();
        let agent_id = config.agent.id.clone();
        let store_handle = state.state.clone();
        let step_log_base = log_base.clone();
        let mut result = tokio::task::spawn_blocking(move || {
            let sink = SharedInstallerSink::new(store_handle);
            let progress = InstallProgress {
                sink: &sink,
                agent_id: &agent_id,
                operation: crate::state::INSTALLER_OPERATION_INSTALL,
                log_base: Some(&step_log_base),
            };
            run_installer_capture(
                &install,
                expected_sha256.as_deref(),
                env,
                &workspace_root,
                Some(&progress),
            )
        })
        .await
        .map_err(|err| StackError::AgentInitializeFailed {
            reason: format!("installer thread join failed: {err}"),
        })?;
        {
            let store = state.state.lock().await;
            persist_untracked_installer_row(
                &store,
                &mut result.row,
                &config.agent.id,
                crate::state::INSTALLER_OPERATION_INSTALL,
                Some(&log_base),
            )?;
        }
        result.outcome?
    } else {
        // Registry-resolved install: one row for native, two for adapter-backed.
        let override_path = home.join(".config").join("acp-stack").join("agents.toml");
        let registry = RegistryCatalog::load_with_override(&override_path)?;
        let entry = registry.lookup_required(&config.agent.id)?.clone();
        let agent = config.agent.clone();
        let agent_id = config.agent.id.clone();
        let store_handle = state.state.clone();
        let step_log_base = log_base.clone();
        let mut result: InstallerSequenceResult = tokio::task::spawn_blocking(move || {
            let sink = SharedInstallerSink::new(store_handle);
            let progress = InstallProgress {
                sink: &sink,
                agent_id: &agent_id,
                operation: crate::state::INSTALLER_OPERATION_INSTALL,
                log_base: Some(&step_log_base),
            };
            install_resolved_capture(
                &agent,
                &entry,
                Default::default(),
                &workspace_root,
                &local_bin,
                Some(&progress),
            )
        })
        .await
        .map_err(|err| StackError::AgentInitializeFailed {
            reason: format!("installer thread join failed: {err}"),
        })?;
        {
            let store = state.state.lock().await;
            for row in result.rows.iter_mut() {
                persist_untracked_installer_row(
                    &store,
                    row,
                    &config.agent.id,
                    crate::state::INSTALLER_OPERATION_INSTALL,
                    Some(&log_base),
                )?;
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

pub(crate) fn open_agent_env(config: &Config) -> Result<std::collections::HashMap<String, String>> {
    Ok(open_agent_environment(config)?.env)
}

pub(crate) fn open_agent_environment(config: &Config) -> Result<ResolvedAgentEnvironment> {
    if let Some(environment) = resolve_agent_environment_without_secrets(config) {
        return Ok(environment);
    }
    let home = home_dir()?;
    let store = SecretStore::open(&home)?;
    resolve_agent_environment(config, &store)
}

async fn load_fresh_config_for_target(
    state: &AppState,
    target_id: &str,
) -> Result<(Config, AgentTargetRuntime)> {
    let mut config = state.refresh_array_runtime_from_disk().await?;
    let target = state.agent_target(target_id)?;
    let live_agent = target.live_agent_config.lock().await.clone();
    let Some(target_config) = config.array.target_mut(target_id) else {
        return Err(StackError::InvalidParam {
            field: "target",
            reason: format!("unknown Array target `{target_id}`"),
        });
    };
    if target_config.agent.id == live_agent.id && target_config.agent.adapter.is_none() {
        target_config.agent.adapter = live_agent.adapter;
    }
    config.agent = target_config.agent.clone();
    Ok((config, target))
}

/// Resolve every configured `[mcp.servers]` entry into the SDK `McpServer`
/// type. Returns an empty Vec when no MCP servers are configured, so the
/// secret store is only opened when there's something to resolve.
pub(super) fn open_mcp_servers(
    config: &Config,
) -> Result<Vec<agent_client_protocol::schema::v1::McpServer>> {
    if config.mcp.servers.is_empty() {
        return Ok(Vec::new());
    }
    let home = home_dir()?;
    let store = SecretStore::open(&home)?;
    crate::runtime::agent::mcp::resolve_mcp_servers(&config.mcp, &store)
}

pub(crate) fn ensure_array_process_start_allowed(config: &Config, target_id: &str) -> Result<()> {
    if config.array.enabled || target_id == config.array.primary_target {
        return Ok(());
    }
    Err(StackError::InvalidParam {
        field: "target",
        reason: format!(
            "Array mode is off; only default target `{}` can be started",
            config.array.primary_target
        ),
    })
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
    let target_id = state.default_target_id().await?;
    capabilities_agent_target(&state, &target_id).await
}

pub(crate) async fn array_agent_capabilities_handler(
    State(state): State<AppState>,
    Path(target_id): Path<String>,
) -> std::result::Result<ApiSuccess<AgentCapabilitiesResponseBody>, StackError> {
    capabilities_agent_target(&state, &target_id).await
}

async fn capabilities_agent_target(
    state: &AppState,
    target_id: &str,
) -> std::result::Result<ApiSuccess<AgentCapabilitiesResponseBody>, StackError> {
    state.refresh_array_runtime_from_disk().await?;
    let target = state.agent_target(target_id)?;
    let agent = target.live_agent_config.lock().await.clone();
    let agent_id = agent.id.clone();
    let snapshot: AgentSnapshot = target.supervisor.snapshot().await;
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
