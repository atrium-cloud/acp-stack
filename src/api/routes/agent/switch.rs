//! Agent switch: repoint the default target to a different harness.

use super::*;

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
    let _mutation = state.lock_agent_config_mutation().await?;
    let home = home_dir()?;
    let fresh_config = Config::load_from_path(&state.runtime_paths.config_path)?;
    let registry = RegistryCatalog::load_with_override(
        &home.join(".config").join("acp-stack").join("agents.toml"),
    )?;
    if fresh_config.array.target(&body.agent).is_some() {
        return switch_to_existing_array_target(&state, &home, &registry, fresh_config, body).await;
    }
    let plan = plan_agent_switch(
        &fresh_config,
        &registry,
        PlannedAgentSwitchRequest {
            target_agent: body.agent.clone(),
            provider_id: body.provider.clone(),
            api_key_ref: body.api_key_ref.clone(),
        },
    )?;
    let target_entry = registry.lookup_required(&plan.target_agent_id)?;
    let mut candidate_config = plan.config.clone();
    candidate_config.agent.adapter = adapter_from_registry_entry(target_entry);
    rename_default_target_config(
        &mut candidate_config,
        &plan.target_agent_id,
        plan.config.agent.clone(),
    )?;

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

    let old_target_id = fresh_config.array.primary_target.clone();
    let old_target = state.agent_target(&old_target_id)?;
    let was_running = old_target.supervisor.snapshot().await.state.as_wire_str() == "running";
    // Rename sessions to the new primary target BEFORE writing the new config.
    // The rename can fail (e.g. a UNIQUE(target_id, agent_session_id) collision
    // is detected up front), and if it does the on-disk config must stay
    // untouched so config and DB never diverge.
    {
        let store = state.state.lock().await;
        store.rename_session_target_id(&old_target_id, &candidate_config.array.primary_target)?;
    }
    crate::fs_util::atomic_write_owner_only(
        &state.runtime_paths.config_path,
        canonical.as_bytes(),
    )?;
    state.refresh_array_runtime_from_disk().await?;
    let restart_started = apply_switch_runtime(
        &state,
        &old_target_id,
        &candidate_config.array.primary_target,
        &candidate_config,
        was_running,
    )
    .await?;
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

async fn switch_to_existing_array_target(
    state: &AppState,
    home: &std::path::Path,
    registry: &RegistryCatalog,
    fresh_config: Config,
    body: AgentSwitchRequest,
) -> std::result::Result<ApiSuccess<AgentSwitchResponse>, StackError> {
    if body.provider.is_some() || body.api_key_ref.is_some() {
        return Err(StackError::InvalidParam {
            field: "provider",
            reason: "provider flags are ignored when switching to an existing Array target; use `acps array set --target ...` first".to_owned(),
        });
    }
    if body.drop_configs {
        return Err(StackError::InvalidParam {
            field: "drop",
            reason: "--drop is not supported when selecting an existing Array target".to_owned(),
        });
    }
    if fresh_config.array.primary_target == body.agent {
        return Err(StackError::InvalidParam {
            field: "agent",
            reason: format!("agent `{}` is already the default target", body.agent),
        });
    }
    let target_agent = fresh_config
        .array
        .target(&body.agent)
        .ok_or_else(|| StackError::InvalidParam {
            field: "agent",
            reason: format!("unknown Array target `{}`", body.agent),
        })?
        .agent
        .clone();
    let target_entry = registry.lookup_required(&target_agent.id)?;
    let mut candidate_config = fresh_config.clone();
    candidate_config.array.primary_target = body.agent.clone();
    candidate_config.agent = target_agent;
    let canonical = candidate_config.to_canonical_toml()?;
    let mut candidate_config = crate::config::load_config_from_str(&canonical)?;
    candidate_config.agent.adapter = adapter_from_registry_entry(target_entry);
    let _env = open_agent_env(&candidate_config)?;
    let required_env_refs = candidate_config.agent.env.clone();

    let install = install_agent_for_config(state, &candidate_config).await?;
    let provisioned =
        crate::runtime::agent::agent_headless_config::provision_agent_headless_config(
            &candidate_config,
            home,
        )?
        .into_iter()
        .map(ProvisionedAgentConfigJson::from)
        .collect::<Vec<_>>();
    let skills_port = port_agent_skills(
        home,
        registry,
        &fresh_config.agent.id,
        &candidate_config.agent.id,
    )?;

    let old_target_id = fresh_config.array.primary_target.clone();
    let old_target = state.agent_target(&old_target_id)?;
    let was_running = old_target.supervisor.snapshot().await.state.as_wire_str() == "running";
    crate::fs_util::atomic_write_owner_only(
        &state.runtime_paths.config_path,
        canonical.as_bytes(),
    )?;
    state.refresh_array_runtime_from_disk().await?;
    let restart_started = apply_switch_runtime(
        state,
        &old_target_id,
        &candidate_config.array.primary_target,
        &candidate_config,
        was_running,
    )
    .await?;

    Ok(ApiSuccess::new(AgentSwitchResponse {
        old_agent_id: fresh_config.agent.id,
        agent_id: candidate_config.agent.id,
        provider_status: "selected",
        provider: candidate_config
            .agent
            .provider
            .as_ref()
            .map(|provider| provider.id.clone()),
        api_key_ref: candidate_config
            .agent
            .provider
            .as_ref()
            .and_then(|provider| provider.api_key_ref.clone()),
        required_env_refs,
        secret_migrations: Vec::new(),
        install,
        restarted: was_running,
        restart_started,
        set_model: false,
        models: Vec::new(),
        follow_up: None,
        provisioned,
        skills_port,
        cleaned_configs: Vec::new(),
        cleanup_errors: Vec::new(),
    }))
}

fn rename_default_target_config(
    config: &mut Config,
    target_id: &str,
    agent: crate::config::AgentConfig,
) -> Result<()> {
    let old_primary = config.array.primary_target.clone();
    let target = config
        .array
        .target_mut(&old_primary)
        .ok_or_else(|| StackError::InvalidParam {
            field: "array.primary_target",
            reason: "must reference an entry in array.targets".to_owned(),
        })?;
    target.id = target_id.to_owned();
    target.agent = agent.clone();
    config.array.primary_target = target_id.to_owned();
    config.agent = agent;
    Ok(())
}

async fn apply_switch_runtime(
    state: &AppState,
    old_target_id: &str,
    new_target_id: &str,
    config: &Config,
    was_running: bool,
) -> Result<bool> {
    let target = state.agent_target(new_target_id)?;
    {
        let mut live = target.live_agent_config.lock().await;
        *live = config.agent.clone();
    }
    if !was_running {
        return Ok(false);
    }
    if let Ok(old_target) = state.agent_target(old_target_id) {
        match old_target
            .supervisor
            .stop(&old_target.target_id, &state.state, &state.event_hub)
            .await
        {
            Ok(_) | Err(StackError::AgentNotRunning) => {}
            Err(err) => return Err(err),
        }
    }
    let target_state = target.supervisor.snapshot().await.state;
    if target_state.as_wire_str() != "stopped" {
        return Ok(false);
    }
    start_agent_with_config(state, &target, config).await?;
    Ok(true)
}

async fn start_agent_with_config(
    state: &AppState,
    target: &AgentTargetRuntime,
    config: &Config,
) -> Result<()> {
    let environment = open_agent_environment(config)?;
    target
        .supervisor
        .start(AgentStartRequest {
            target_id: &target.target_id,
            agent: &config.agent,
            workspace_root: &config.workspace.root,
            env: environment.env,
            providers: environment.providers,
            state: &state.state,
            session_changes: &state.session_changes,
            event_hub: state.event_hub.clone(),
            permissions: Some(state.permissions.clone()),
            sandbox: config.workspace.sandbox.clone(),
            network_provider: crate::extensions::resolve_network_provider(config),
        })
        .await?;
    Ok(())
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
