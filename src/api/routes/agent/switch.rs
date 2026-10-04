//! Agent switch: repoint the default target to a different harness.

use super::*;
use crate::api::routes::providers::ModelJson;
use crate::runtime::agent::model_discovery::{
    discovery_is_blocked_without_a_model, model_value_is_explicit_without_discovery,
};
use crate::runtime::agent::switch_journal::{
    SwitchJournal, SwitchJournalPhase, candidate_fingerprint, load_switch_journal,
    persist_switch_journal, remove_switch_journal,
};
use crate::runtime::install::agent_registry::RegistryEntry;

mod model;

use self::model::{
    committed_model, ensure_entry_sets_model, model_follow_up, probe_advertisement,
    resolve_advertised_switch_model, restore_committed_agent_config, set_agent_model,
    validate_requested_model_value,
};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct AgentSwitchRequest {
    agent_id: String,
    #[serde(default, rename = "drop")]
    drop_configs: bool,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    api_key_ref: Option<String>,
    #[serde(default)]
    model: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct AgentSwitchResponse {
    old_agent_id: String,
    agent_id: String,
    #[schemars(extend("enum" = ["not_applicable", "reused", "set", "selected", "resumed", "no_op", "unchanged"]))]
    provider_status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    api_key_ref: Option<String>,
    /// The committed model: the provider slot, else the root.
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    required_env_refs: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    secret_migrations: Vec<AgentSwitchSecretMigrationJson>,
    /// Absent on resumed/no-op retries: install is a pre-commit step that the
    /// interrupted attempt already ran, and re-running it on every retry
    /// would re-burn minutes for no state change.
    #[serde(skip_serializing_if = "Option::is_none")]
    install: Option<AgentInstallResponse>,
    restarted: bool,
    restart_started: bool,
    set_model: bool,
    models: Vec<ModelJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    follow_up: Option<&'static str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    provisioned: Vec<ProvisionedAgentConfigJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    skills_port: Option<SkillPortReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    skills_link: Option<SkillLinkReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    skills_link_error: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    cleaned_configs: Vec<CleanedAgentConfigJson>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    cleanup_errors: Vec<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct ProvisionedAgentConfigJson {
    label: &'static str,
    path: String,
}

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct CleanedAgentConfigJson {
    label: &'static str,
    path: String,
}

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct AgentSwitchSecretMigrationJson {
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
    validate_requested_model_value(body.model.as_deref())?;
    let _mutation = state.lock_agent_config_mutation().await?;
    let home = state.runtime_paths.home.clone();
    let fresh_config = Config::load_from_path(&state.runtime_paths.config_path)?;
    let registry = RegistryCatalog::load_with_override(
        &home.join(".config").join("acp-stack").join("agents.toml"),
    )?;
    // The journal must gate dispatch first, or the fresh-path validation below rejects a
    // same-target retry of an interrupted switch as "already configured".
    let reconfigure_requested =
        body.provider.is_some() || body.api_key_ref.is_some() || body.model.is_some();
    let resume_journal = match load_switch_journal(&state.runtime_paths.config_path)? {
        Some(journal) => match classify_switch_journal(
            &journal,
            &body.agent_id,
            &fresh_config,
            body.provider.as_deref(),
            body.api_key_ref.as_deref(),
            body.model.as_deref(),
        )? {
            SwitchJournalAction::NoOp => {
                return Ok(completed_switch_response(
                    &fresh_config,
                    &journal.target_agent_id,
                    registry.lookup(&fresh_config.agent.id),
                ));
            }
            SwitchJournalAction::ResumeCommitted => {
                return resume_committed_switch(
                    &state,
                    &registry,
                    fresh_config,
                    &journal,
                    body.drop_configs,
                )
                .await;
            }
            SwitchJournalAction::ResumeFromCommitBoundary => Some(journal),
            SwitchJournalAction::FreshStart => None,
        },
        None => None,
    };
    // A bare switch naming the target that is already the default must converge as a
    // side-effect-free success; flagged bodies keep their rejections in the existing-target path below.
    if resume_journal.is_none()
        && fresh_config.array.primary_target == body.agent_id
        && !body.drop_configs
        && !reconfigure_requested
    {
        return Ok(completed_switch_response(
            &fresh_config,
            &fresh_config.agent.id,
            registry.lookup(&fresh_config.agent.id),
        ));
    }
    // Harness, provider, and model are separate choices, bounded only by the per-agent support
    // matrix, so a body naming the current primary target with any of them reconfigures it in place.
    if fresh_config.array.primary_target == body.agent_id && reconfigure_requested {
        return reconfigure_primary_target(
            &state,
            &home,
            &registry,
            fresh_config,
            body,
            resume_journal,
        )
        .await;
    }
    if fresh_config.array.target(&body.agent_id).is_some() {
        return switch_to_existing_array_target(
            &state,
            &home,
            &registry,
            fresh_config,
            body,
            resume_journal,
        )
        .await;
    }
    let plan = plan_agent_switch(
        &home,
        &fresh_config,
        &registry,
        PlannedAgentSwitchRequest {
            target_agent: body.agent_id.clone(),
            provider_id: body.provider.clone(),
            api_key_ref: body.api_key_ref.clone(),
        },
    )?;
    let target_entry = registry.lookup_required(&plan.target_agent_id)?;
    if body.model.is_some() {
        ensure_entry_sets_model(target_entry)?;
    }
    let mut candidate_config = plan.config.clone();
    candidate_config.agent.adapter = adapter_from_registry_entry(target_entry);
    rename_default_target_config(
        &mut candidate_config,
        &plan.target_agent_id,
        plan.config.agent.clone(),
    )?;
    // A harness that takes its model verbatim gets it before the only provisioning; every other
    // harness resolves it against the discovery probe below.
    let explicit_model = body
        .model
        .clone()
        .filter(|_| model_value_is_explicit_without_discovery(&candidate_config.agent));
    if let Some(model) = explicit_model.clone() {
        set_agent_model(&mut candidate_config.agent, model);
    }

    let mut canonical = candidate_config.to_canonical_toml()?;
    let mut candidate_config = reload_candidate_config(&canonical, target_entry)?;
    let secret_migrations = apply_switch_secret_migrations(&home, &plan.secret_migrations)?;
    let _env = open_agent_env(&state.runtime_paths.home, &candidate_config)?;

    let install = install_agent_for_config(&state, &candidate_config).await?;
    crate::runtime::agent::provider_model_catalog::refresh_provider_models_best_effort(
        &home,
        &candidate_config,
    )
    .await;
    let mut provisioned = provision_agent_config_for_response(&candidate_config, &home)?;

    // A target that resolves its model while starting a session cannot answer a
    // discovery session yet, so a model-less switch reports no models rather than failing.
    // Where the provider publishes a catalog, `GET /v1/models` serves the list
    // once the switch lands; where it does not, no route can list models until a
    // model is named explicitly.
    let discovered = if explicit_model.is_none()
        && (body.model.is_some()
            || (target_entry.set_model
                && !discovery_is_blocked_without_a_model(&candidate_config.agent)))
    {
        Some(probe_advertisement(&home, &candidate_config).await?)
    } else {
        None
    };
    let models = match discovered.as_ref() {
        Some(response) => {
            advertised_values_for_category(response, AgentSessionConfigCategory::Model)?
                .into_iter()
                // ACP advertises bare values with no separate label, so there is no display name to carry.
                .map(|value| ModelJson {
                    value,
                    display_name: None,
                    efforts: Vec::new(),
                })
                .collect()
        }
        None => Vec::new(),
    };
    // The model is resolved against the probe of the model-free candidate, so a value the target
    // does not list is refused before any harness runs with it.
    if explicit_model.is_none()
        && let (Some(model), Some(response)) = (body.model.as_deref(), discovered.as_ref())
    {
        let resolved = resolve_advertised_switch_model(&candidate_config.agent, response, model)?;
        set_agent_model(&mut candidate_config.agent, resolved);
        canonical = candidate_config.to_canonical_toml()?;
        candidate_config = reload_candidate_config(&canonical, target_entry)?;
        provisioned = provision_agent_config_for_response(&candidate_config, &home)?;
    }
    let skills_port = port_agent_skills(
        &home,
        &registry,
        &fresh_config.agent.id,
        &candidate_config.agent.id,
    )?;
    let link_outcome = link_agent_skills_best_effort(&home, target_entry);

    let old_target_id = fresh_config.array.primary_target.clone();
    let old_target = state.agent_target(&old_target_id)?;
    let was_running = old_target.supervisor.snapshot().await.state.as_wire_str() == "running";
    let mut journal = SwitchJournal {
        old_target_id: old_target_id.clone(),
        new_target_id: body.agent_id.clone(),
        target_agent_id: plan.target_agent_id.clone(),
        candidate_fingerprint: candidate_fingerprint(&canonical),
        was_running,
        phase: SwitchJournalPhase::Planned,
        requested_model: body.model.clone(),
    };
    // The target's newly provisioned files stay after a failure here: they are inert while the
    // committed config names the source harness, and the next switch to this target rewrites them.
    let restart_started = commit_switch_and_apply_runtime(
        SwitchCommit {
            state: &state,
            old_target_id: &old_target_id,
            candidate_config: &candidate_config,
            canonical: &canonical,
            was_running,
            resume_journal: resume_journal.as_ref(),
            rename_sessions: true,
        },
        &mut journal,
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
    journal.phase = SwitchJournalPhase::Completed;
    persist_switch_journal(&state.runtime_paths.config_path, &journal)?;

    let (set_model, follow_up) = model_follow_up(Some(target_entry), &candidate_config.agent);
    let response = AgentSwitchResponse {
        old_agent_id: plan.old_agent_id,
        agent_id: plan.target_agent_id,
        provider_status: plan.provider_status.label(),
        provider: plan.provider_status.provider_id().map(str::to_owned),
        api_key_ref: plan.provider_status.api_key_ref().map(str::to_owned),
        model: committed_model(&candidate_config.agent),
        required_env_refs: plan.required_env_refs,
        secret_migrations,
        install: Some(install),
        restarted: was_running,
        restart_started,
        set_model,
        models,
        follow_up,
        provisioned,
        skills_port,
        skills_link: link_outcome.report,
        skills_link_error: link_outcome.error,
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
    resume_journal: Option<SwitchJournal>,
) -> std::result::Result<ApiSuccess<AgentSwitchResponse>, StackError> {
    if body.provider.is_some() || body.api_key_ref.is_some() {
        return Err(StackError::InvalidParam {
            field: "provider",
            reason: "provider flags are ignored when switching to an existing Array target; use `acps array set --target ...` first".to_owned(),
        });
    }
    if body.model.is_some() {
        return Err(StackError::InvalidParam {
            field: "model",
            reason: "a model cannot be set while selecting an existing Array target; select the target first, then switch to it with `model` alone".to_owned(),
        });
    }
    if body.drop_configs {
        return Err(StackError::InvalidParam {
            field: "drop",
            reason: "--drop is not supported when selecting an existing Array target".to_owned(),
        });
    }
    if fresh_config.array.primary_target == body.agent_id {
        return Err(StackError::InvalidParam {
            field: "agent_id",
            reason: format!("agent `{}` is already the default target", body.agent_id),
        });
    }
    let target_agent = fresh_config
        .array
        .target(&body.agent_id)
        .ok_or_else(|| StackError::InvalidParam {
            field: "agent_id",
            reason: format!("unknown Array target `{}`", body.agent_id),
        })?
        .agent
        .clone();
    let target_entry = registry.lookup_required(&target_agent.id)?;
    let mut candidate_config = fresh_config.clone();
    candidate_config.array.primary_target = body.agent_id.clone();
    candidate_config.agent = target_agent;
    let canonical = candidate_config.to_canonical_toml()?;
    let candidate_config = reload_candidate_config(&canonical, target_entry)?;
    // Selecting an existing target repoints the native config the override lives in, so it faces the same survival check as a planned switch.
    crate::runtime::agent::switch::ensure_endpoint_override_survives_target(
        &state.runtime_paths.home,
        &target_entry.id,
        target_entry.set_provider_base_url,
        candidate_config
            .agent
            .provider
            .as_ref()
            .map(|provider| provider.id.as_str()),
    )?;
    let _env = open_agent_env(&state.runtime_paths.home, &candidate_config)?;
    let required_env_refs = candidate_config.agent.env.clone();

    let install = install_agent_for_config(state, &candidate_config).await?;
    crate::runtime::agent::provider_model_catalog::refresh_provider_models_best_effort(
        home,
        &candidate_config,
    )
    .await;
    let provisioned = provision_agent_config_for_response(&candidate_config, home)?;
    let skills_port = port_agent_skills(
        home,
        registry,
        &fresh_config.agent.id,
        &candidate_config.agent.id,
    )?;
    let link_outcome = link_agent_skills_best_effort(home, target_entry);

    let old_target_id = fresh_config.array.primary_target.clone();
    let old_target = state.agent_target(&old_target_id)?;
    let was_running = old_target.supervisor.snapshot().await.state.as_wire_str() == "running";
    let mut journal = SwitchJournal {
        old_target_id: old_target_id.clone(),
        new_target_id: body.agent_id.clone(),
        target_agent_id: candidate_config.agent.id.clone(),
        candidate_fingerprint: candidate_fingerprint(&canonical),
        was_running,
        phase: SwitchJournalPhase::Planned,
        requested_model: None,
    };
    let restart_started = commit_switch_and_apply_runtime(
        SwitchCommit {
            state,
            old_target_id: &old_target_id,
            candidate_config: &candidate_config,
            canonical: &canonical,
            was_running,
            resume_journal: resume_journal.as_ref(),
            rename_sessions: false,
        },
        &mut journal,
    )
    .await?;
    journal.phase = SwitchJournalPhase::Completed;
    persist_switch_journal(&state.runtime_paths.config_path, &journal)?;

    let (set_model, follow_up) = model_follow_up(Some(target_entry), &candidate_config.agent);
    Ok(ApiSuccess::new(AgentSwitchResponse {
        old_agent_id: fresh_config.agent.id,
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
        model: committed_model(&candidate_config.agent),
        agent_id: candidate_config.agent.id,
        required_env_refs,
        secret_migrations: Vec::new(),
        install: Some(install),
        restarted: was_running,
        restart_started,
        set_model,
        models: Vec::new(),
        follow_up,
        provisioned,
        skills_port,
        skills_link: link_outcome.report,
        skills_link_error: link_outcome.error,
        cleaned_configs: Vec::new(),
        cleanup_errors: Vec::new(),
    }))
}

/// Reconfigure the target that is already primary: its provider, its model, or both. The
/// harness does not move, so this arm runs neither the installer nor skills porting: only
/// provider and model resolution, the config commit, and the runtime re-apply.
async fn reconfigure_primary_target(
    state: &AppState,
    home: &std::path::Path,
    registry: &RegistryCatalog,
    fresh_config: Config,
    body: AgentSwitchRequest,
    resume_journal: Option<SwitchJournal>,
) -> std::result::Result<ApiSuccess<AgentSwitchResponse>, StackError> {
    if body.drop_configs {
        return Err(StackError::InvalidParam {
            field: "drop",
            reason: "--drop is not supported when reconfiguring the current target's provider"
                .to_owned(),
        });
    }
    let target_entry = registry.lookup_required(&fresh_config.agent.id)?;
    if body.model.is_some() {
        ensure_entry_sets_model(target_entry)?;
    }
    let mut candidate_config = fresh_config.clone();
    let (provider_status, required_env_refs) = match body.provider.as_deref() {
        Some(provider_id) => {
            // Naming the provider already configured keeps its model, including a hand-written root
            // model, which moves into the provider slot.
            let kept_model = fresh_config
                .agent
                .provider
                .as_ref()
                .filter(|provider| body.model.is_none() && provider.id == provider_id)
                .and_then(|_| committed_model(&fresh_config.agent));
            let required_env_refs =
                crate::runtime::agent::provider_keys::apply_mapped_agent_provider(
                    &mut candidate_config,
                    provider_id,
                    body.api_key_ref.clone(),
                )?;
            candidate_config
                .agent
                .provider
                .as_mut()
                .ok_or(StackError::MissingField {
                    field: "agent.provider",
                })?
                .model = kept_model;
            // MUST run after provider resolution so the pair-level refusal sees the provider the target would actually run.
            crate::runtime::agent::switch::ensure_endpoint_override_survives_target(
                home,
                &target_entry.id,
                target_entry.set_provider_base_url,
                Some(provider_id),
            )?;
            ("set", required_env_refs)
        }
        None if body.api_key_ref.is_some() => {
            return Err(StackError::InvalidParam {
                field: "api_key_ref",
                reason:
                    "an API-key ref for the current target needs the provider it belongs to; pass `provider` as well"
                        .to_owned(),
            });
        }
        // A model-only body leaves the provider selection and env as committed.
        None => {
            if target_entry.set_provider && fresh_config.agent.provider.is_none() {
                return Err(StackError::InvalidParam {
                    field: "model",
                    reason: format!(
                        "{} has no provider configured; pass `provider` with `model`",
                        target_entry.name
                    ),
                });
            }
            ("unchanged", fresh_config.agent.env.clone())
        }
    };
    let provider_changed =
        provider_selection(&fresh_config.agent) != provider_selection(&candidate_config.agent);
    let mut canonical = candidate_config.to_canonical_toml()?;
    let mut candidate_config = reload_candidate_config(&canonical, target_entry)?;
    let mut deferred_model = None;
    if let Some(model) = body.model.as_deref() {
        if model_value_is_explicit_without_discovery(&candidate_config.agent) {
            set_agent_model(&mut candidate_config.agent, model.to_owned());
        } else if provider_changed {
            // The probe reads the new provider's agent-owned config, so it waits for the
            // provisioning below.
            deferred_model = Some(model);
        } else {
            // The committed agent-owned config already serves this provider, so the probe runs
            // before anything is written and an identical selection stays a no-op.
            let response = probe_advertisement(home, &candidate_config).await?;
            let resolved =
                resolve_advertised_switch_model(&candidate_config.agent, &response, model)?;
            set_agent_model(&mut candidate_config.agent, resolved);
        }
        canonical = candidate_config.to_canonical_toml()?;
        candidate_config = reload_candidate_config(&canonical, target_entry)?;
    }
    // A retry that resolves to the bytes already committed has nothing to write and nothing to restart.
    if resume_journal.is_none() && canonical == fresh_config.to_canonical_toml()? {
        return Ok(completed_switch_response(
            &fresh_config,
            &fresh_config.agent.id,
            Some(target_entry),
        ));
    }
    // The credential the provider resolves through must exist before anything is written.
    let _env = open_agent_env(&state.runtime_paths.home, &candidate_config)?;

    crate::runtime::agent::provider_model_catalog::refresh_provider_models_best_effort(
        home,
        &candidate_config,
    )
    .await;
    let target_id = fresh_config.array.primary_target.clone();
    let target = state.agent_target(&target_id)?;
    // From the first provisioning on, a failure before the config write puts the committed
    // agent-owned config back.
    let prepared = match provision_reconfigure_candidate(
        home,
        target_entry,
        candidate_config,
        canonical,
        deferred_model,
    )
    .await
    {
        Ok(prepared) => prepared,
        Err(error) => {
            restore_committed_agent_config(&fresh_config, home);
            return Err(error);
        }
    };
    let PreparedCandidate {
        config: candidate_config,
        canonical,
        provisioned,
    } = prepared;

    let was_running = target.supervisor.snapshot().await.state.as_wire_str() == "running";
    let mut journal = SwitchJournal {
        old_target_id: target_id.clone(),
        new_target_id: target_id.clone(),
        target_agent_id: candidate_config.agent.id.clone(),
        candidate_fingerprint: candidate_fingerprint(&canonical),
        was_running,
        phase: SwitchJournalPhase::Planned,
        requested_model: body.model.clone(),
    };
    let restart_started = match commit_switch_and_apply_runtime(
        SwitchCommit {
            state,
            old_target_id: &target_id,
            candidate_config: &candidate_config,
            canonical: &canonical,
            was_running,
            resume_journal: resume_journal.as_ref(),
            rename_sessions: false,
        },
        &mut journal,
    )
    .await
    {
        Ok(restart_started) => restart_started,
        Err(SwitchCommitError::BeforeConfigWrite(error)) => {
            restore_committed_agent_config(&fresh_config, home);
            return Err(error);
        }
        // The agent-owned files already match the written config, and a retry resumes the switch.
        Err(SwitchCommitError::AfterConfigWrite(error)) => return Err(error),
    };
    journal.phase = SwitchJournalPhase::Completed;
    persist_switch_journal(&state.runtime_paths.config_path, &journal)?;

    let (set_model, follow_up) = model_follow_up(Some(target_entry), &candidate_config.agent);
    Ok(ApiSuccess::new(AgentSwitchResponse {
        old_agent_id: candidate_config.agent.id.clone(),
        agent_id: candidate_config.agent.id.clone(),
        provider_status,
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
        model: committed_model(&candidate_config.agent),
        required_env_refs,
        secret_migrations: Vec::new(),
        install: None,
        restarted: was_running,
        restart_started,
        set_model,
        models: Vec::new(),
        follow_up,
        provisioned,
        skills_port: None,
        skills_link: None,
        skills_link_error: None,
        cleaned_configs: Vec::new(),
        cleanup_errors: Vec::new(),
    }))
}

/// A reconfigure candidate whose agent-owned config is on disk, ready to commit.
struct PreparedCandidate {
    config: Config,
    canonical: String,
    provisioned: Vec<ProvisionedAgentConfigJson>,
}

/// Provision the reconfigure candidate. A model that waits on the new provider's agent-owned
/// config is resolved against a probe of that model-free candidate, then provisioned again.
async fn provision_reconfigure_candidate(
    home: &std::path::Path,
    target_entry: &RegistryEntry,
    mut config: Config,
    mut canonical: String,
    deferred_model: Option<&str>,
) -> Result<PreparedCandidate> {
    let mut provisioned = provision_agent_config_for_response(&config, home)?;
    if let Some(model) = deferred_model {
        let response = probe_advertisement(home, &config).await?;
        let resolved = resolve_advertised_switch_model(&config.agent, &response, model)?;
        set_agent_model(&mut config.agent, resolved);
        canonical = config.to_canonical_toml()?;
        config = reload_candidate_config(&canonical, target_entry)?;
        provisioned = provision_agent_config_for_response(&config, home)?;
    }
    Ok(PreparedCandidate {
        config,
        canonical,
        provisioned,
    })
}

/// The provider id and API-key ref a reconfigure compares to decide whether the committed
/// agent-owned config already serves the candidate.
fn provider_selection(agent: &crate::config::AgentConfig) -> Option<(&str, Option<&str>)> {
    agent
        .provider
        .as_ref()
        .map(|provider| (provider.id.as_str(), provider.api_key_ref.as_deref()))
}

/// Load a candidate from its canonical TOML and set the registry-derived adapter again, which
/// the TOML round trip does not carry.
fn reload_candidate_config(canonical: &str, entry: &RegistryEntry) -> Result<Config> {
    let mut config = crate::config::load_config_from_str(canonical)?;
    config.agent.adapter = adapter_from_registry_entry(entry);
    Ok(config)
}

fn provision_agent_config_for_response(
    config: &Config,
    home: &std::path::Path,
) -> Result<Vec<ProvisionedAgentConfigJson>> {
    Ok(
        crate::runtime::agent::agent_headless_config::provision_agent_headless_config(
            config, home,
        )?
        .into_iter()
        .map(ProvisionedAgentConfigJson::from)
        .collect(),
    )
}

/// How a pending-switch journal entry steers the current request.
enum SwitchJournalAction {
    /// Journal Completed, same target, disk agrees: the retry is a provably
    /// side-effect-free no-op.
    NoOp,
    /// Journal incomplete and the committed candidate is already on disk:
    /// resume at the runtime re-apply with the journaled `was_running`.
    ResumeCommitted,
    /// Journal incomplete and disk still shows the old primary: the
    /// interrupted attempt never crossed the commit boundary, so re-run the
    /// full (idempotent) pre-commit pipeline and commit again.
    ResumeFromCommitBoundary,
    /// Journal Completed but stale for this request: proceed as a fresh
    /// switch, overwriting the journal at Planned.
    FreshStart,
}

fn classify_switch_journal(
    journal: &SwitchJournal,
    requested: &str,
    fresh_config: &Config,
    requested_provider: Option<&str>,
    requested_api_key_ref: Option<&str>,
    requested_model: Option<&str>,
) -> Result<SwitchJournalAction> {
    let reconfigure_requested = requested_provider.is_some()
        || requested_api_key_ref.is_some()
        || requested_model.is_some();
    let same_target = journal.requested_target_matches(requested);
    let candidate_on_disk =
        candidate_fingerprint(&fresh_config.to_canonical_toml()?) == journal.candidate_fingerprint;
    // Config canonicalization rewrites the on-disk primary target id to the agent id, so the committed marker is the agent id.
    // A provider reconfigure keeps that id either way, so its only commit marker is the candidate's bytes.
    let committed_on_disk = if journal.is_same_target_reconfigure() {
        candidate_on_disk
    } else {
        fresh_config.agent.id == journal.target_agent_id
    };
    if journal.phase == SwitchJournalPhase::Completed {
        // A body carrying provider or model fields asks for a selection this journal cannot
        // vouch for, so it is planned afresh and converges on its own byte comparison.
        if same_target && committed_on_disk && !reconfigure_requested {
            return Ok(SwitchJournalAction::NoOp);
        }
        return Ok(SwitchJournalAction::FreshStart);
    }
    if !same_target {
        return Err(StackError::AgentSwitchConflict {
            reason: format!(
                "an earlier switch to `{}` did not finish (phase `{}`); retry that target so the switch can resume, or repair the switch journal before switching elsewhere",
                journal.new_target_id,
                journal.phase.as_str()
            ),
        });
    }
    if committed_on_disk {
        // The config write is the commit marker, so verify the bytes match the journaled candidate:
        // an operator edit between attempts must not be adopted as the in-flight switch's outcome.
        if !candidate_on_disk {
            return Err(StackError::AgentSwitchConflict {
                reason: format!(
                    "the on-disk config for `{}` does not match the interrupted switch's candidate; repair the config or the switch journal before retrying",
                    journal.new_target_id
                ),
            });
        }
        // The committed model is the resolved form, so only the journaled request spelling can
        // vouch for a retry naming a model, whichever kind of switch was interrupted.
        if let Some(model) = requested_model
            && journal.requested_model.as_deref() != Some(model)
        {
            return Err(StackError::AgentSwitchConflict {
                reason: format!(
                    "the interrupted switch to `{}` already committed a different model selection; retry without `model`, or with the model the interrupted request named, to converge it",
                    journal.new_target_id
                ),
            });
        }
        // A reconfigure's provider flags are the whole intent of the request, so a post-commit
        // retry carrying a different selection must conflict the way the pre-commit fingerprint
        // check does rather than silently converge on the journaled provider.
        if journal.is_same_target_reconfigure()
            && let Some(provider_id) = requested_provider
        {
            let (requested_id, requested_ref) = requested_provider_selection(
                &fresh_config.agent.id,
                provider_id,
                requested_api_key_ref,
            );
            let committed = fresh_config.agent.provider.as_ref();
            let committed_id = committed.map(|provider| provider.id.as_str());
            let committed_ref = committed.and_then(|provider| provider.api_key_ref.as_deref());
            if committed_id != Some(requested_id.as_str())
                || committed_ref != requested_ref.as_deref()
            {
                return Err(StackError::AgentSwitchConflict {
                    reason: format!(
                        "the interrupted reconfigure of `{}` already committed provider `{}` (api-key ref `{}`); retry with that selection to converge it, or repair the switch journal before changing providers",
                        journal.new_target_id,
                        committed_id.unwrap_or("none"),
                        committed_ref.unwrap_or("none"),
                    ),
                });
            }
        }
        return Ok(SwitchJournalAction::ResumeCommitted);
    }
    Ok(SwitchJournalAction::ResumeFromCommitBoundary)
}

/// The provider selection a reconfigure retry asks for, resolved the way
/// `apply_mapped_agent_provider` commits it: an explicit ref wins, a
/// native-auth provider stores none, anything else falls back to the
/// provider's default env mapping.
fn requested_provider_selection(
    agent_id: &str,
    provider_id: &str,
    api_key_ref: Option<&str>,
) -> (String, Option<String>) {
    let resolved_ref = api_key_ref.map(str::to_owned).or_else(|| {
        (!crate::runtime::agent::provider_keys::provider_uses_agent_native_auth(
            agent_id,
            provider_id,
        ))
        .then(|| {
            crate::runtime::agent::provider_keys::env_var_for_agent_provider_id(
                agent_id,
                provider_id,
            )
        })
        .flatten()
        .map(str::to_owned)
    });
    (provider_id.to_owned(), resolved_ref)
}

/// Inputs to the shared switch commit boundary.
struct SwitchCommit<'a> {
    state: &'a AppState,
    old_target_id: &'a str,
    candidate_config: &'a Config,
    canonical: &'a str,
    was_running: bool,
    resume_journal: Option<&'a SwitchJournal>,
    /// Distinguishes the paths: the primary-switch path replaces the old
    /// target id, so its session rows must move; the existing-array-target
    /// path keeps both targets addressable and leaves session rows alone.
    rename_sessions: bool,
}

/// Which side of the canonical config write a commit failure came from. Before it the old config
/// is still on disk, so a same-target caller restores the agent-owned config it provisioned;
/// after it the agent-owned files already match the written config and a retry resumes it.
enum SwitchCommitError {
    BeforeConfigWrite(StackError),
    AfterConfigWrite(StackError),
}

impl From<SwitchCommitError> for StackError {
    fn from(error: SwitchCommitError) -> Self {
        match error {
            SwitchCommitError::BeforeConfigWrite(error)
            | SwitchCommitError::AfterConfigWrite(error) => error,
        }
    }
}

/// Shared commit boundary for both switch paths: journal the plan, apply the
/// commit (session rename + canonical config write), re-apply the runtime,
/// and advance the journal to RuntimeApplied. The caller runs its own
/// post-commit cleanup and then drives the journal to Completed.
async fn commit_switch_and_apply_runtime(
    commit: SwitchCommit<'_>,
    journal: &mut SwitchJournal,
) -> std::result::Result<bool, SwitchCommitError> {
    use SwitchCommitError::{AfterConfigWrite, BeforeConfigWrite};

    let state = commit.state;
    // A same-target resume must reproduce the journaled candidate byte for byte, or this retry
    // converges on a different switch than the one that was interrupted.
    if let Some(prior) = commit.resume_journal
        && prior.candidate_fingerprint != journal.candidate_fingerprint
    {
        return Err(BeforeConfigWrite(StackError::AgentSwitchConflict {
            reason: format!(
                "the recomputed switch to `{}` no longer matches the interrupted attempt's candidate; restore the previous config or repair the switch journal before retrying",
                prior.new_target_id
            ),
        }));
    }
    persist_switch_journal(&state.runtime_paths.config_path, journal).map_err(BeforeConfigWrite)?;
    if commit.rename_sessions {
        // The session rename MUST precede the config write: it can fail on a UNIQUE collision, and
        // if it does the on-disk config must stay untouched so config and DB never diverge.
        let rename_result = {
            let store = state.state.lock().await;
            store.rename_session_target_id(
                commit.old_target_id,
                &commit.candidate_config.array.primary_target,
            )
        };
        if let Err(rename_error) = rename_result {
            // Nothing durable changed, so drop the Planned journal rather than strand a record that
            // would 409 every later switch.
            if matches!(rename_error, StackError::SessionTargetRenameConflict { .. }) {
                remove_switch_journal(&state.runtime_paths.config_path)
                    .map_err(BeforeConfigWrite)?;
            }
            return Err(BeforeConfigWrite(rename_error));
        }
    }
    if let Err(error) = crate::fs_util::atomic_write_owner_only(
        &state.runtime_paths.config_path,
        commit.canonical.as_bytes(),
    ) {
        // The rename into place is the commit point; a failure after it (permissions, fsync)
        // still left the new bytes on disk.
        let landed = std::fs::read(&state.runtime_paths.config_path)
            .is_ok_and(|on_disk| on_disk == commit.canonical.as_bytes());
        return Err(if landed {
            AfterConfigWrite(error)
        } else {
            BeforeConfigWrite(error)
        });
    }
    state
        .refresh_array_runtime_from_disk()
        .await
        .map_err(AfterConfigWrite)?;
    journal.phase = SwitchJournalPhase::Committed;
    persist_switch_journal(&state.runtime_paths.config_path, journal).map_err(AfterConfigWrite)?;
    let restart_started = apply_switch_runtime(
        state,
        commit.old_target_id,
        &commit.candidate_config.array.primary_target,
        commit.candidate_config,
        commit.was_running,
    )
    .await
    .map_err(AfterConfigWrite)?;
    journal.phase = SwitchJournalPhase::RuntimeApplied;
    persist_switch_journal(&state.runtime_paths.config_path, journal).map_err(AfterConfigWrite)?;
    Ok(restart_started)
}

/// Post-commit resume. The candidate config is already on disk (the write is
/// the commit marker), so planning, secret migration, install, provisioning,
/// model discovery, and skills porting are NOT re-run: they are pre-commit,
/// idempotent, and slow (install plus model discovery burn minutes), and the
/// only step a retry must converge is the post-commit runtime re-apply. The
/// response therefore reports those pre-commit fields as empty/skipped and
/// uses the journaled `was_running`, which a process restart could not
/// re-observe.
async fn resume_committed_switch(
    state: &AppState,
    registry: &RegistryCatalog,
    fresh_config: Config,
    journal: &SwitchJournal,
    drop_requested: bool,
) -> Result<ApiSuccess<AgentSwitchResponse>> {
    let target_entry = registry.lookup_required(&journal.target_agent_id)?;
    let mut candidate_config = fresh_config.clone();
    candidate_config.agent.adapter = adapter_from_registry_entry(target_entry);
    state.refresh_array_runtime_from_disk().await?;
    let restart_started = apply_switch_runtime(
        state,
        &journal.old_target_id,
        &fresh_config.array.primary_target,
        &candidate_config,
        journal.was_running,
    )
    .await?;
    let mut journal = journal.clone();
    journal.phase = SwitchJournalPhase::RuntimeApplied;
    persist_switch_journal(&state.runtime_paths.config_path, &journal)?;

    // `--drop` cleanup cannot be reconstructed here: the source agent's identity was renamed away
    // with its target, so there is no trustworthy config left to clean against.
    let cleanup_errors = if drop_requested {
        let message = format!(
            "source agent config cleanup was skipped because the switch to `{}` was already committed before this retry; remove the old agent's config manually",
            journal.target_agent_id
        );
        tracing::warn!(%message);
        vec![message]
    } else {
        Vec::new()
    };

    journal.phase = SwitchJournalPhase::Completed;
    persist_switch_journal(&state.runtime_paths.config_path, &journal)?;

    let (set_model, follow_up) = model_follow_up(Some(target_entry), &candidate_config.agent);
    Ok(ApiSuccess::new(AgentSwitchResponse {
        old_agent_id: old_agent_label(&candidate_config, &journal.old_target_id),
        agent_id: journal.target_agent_id.clone(),
        provider_status: "resumed",
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
        model: committed_model(&candidate_config.agent),
        required_env_refs: candidate_config.agent.env.clone(),
        secret_migrations: Vec::new(),
        install: None,
        restarted: journal.was_running,
        restart_started,
        set_model,
        models: Vec::new(),
        follow_up,
        provisioned: Vec::new(),
        skills_port: None,
        skills_link: None,
        skills_link_error: None,
        cleaned_configs: Vec::new(),
        cleanup_errors,
    }))
}

/// Switch whose target is already in place: either a retry of a switch that
/// Completed (the journal plus the on-disk primary prove convergence) or a
/// bare request naming the target that is already the default. Either way the
/// response is a pure no-op, with no rewrite, stop/start, or install re-run.
/// `old_agent_id` reports the current agent (which is the target) because
/// nothing changed.
fn completed_switch_response(
    fresh_config: &Config,
    target_agent_id: &str,
    target_entry: Option<&RegistryEntry>,
) -> ApiSuccess<AgentSwitchResponse> {
    let (set_model, follow_up) = model_follow_up(target_entry, &fresh_config.agent);
    ApiSuccess::new(AgentSwitchResponse {
        old_agent_id: fresh_config.agent.id.clone(),
        agent_id: target_agent_id.to_owned(),
        provider_status: "no_op",
        provider: fresh_config
            .agent
            .provider
            .as_ref()
            .map(|provider| provider.id.clone()),
        api_key_ref: fresh_config
            .agent
            .provider
            .as_ref()
            .and_then(|provider| provider.api_key_ref.clone()),
        model: committed_model(&fresh_config.agent),
        required_env_refs: fresh_config.agent.env.clone(),
        secret_migrations: Vec::new(),
        install: None,
        restarted: false,
        restart_started: false,
        set_model,
        models: Vec::new(),
        follow_up,
        provisioned: Vec::new(),
        skills_port: None,
        skills_link: None,
        skills_link_error: None,
        cleaned_configs: Vec::new(),
        cleanup_errors: Vec::new(),
    })
}

/// Post-commit the old primary target is still present in the config only on
/// the existing-array-target path; the primary-switch path renames it to the
/// new agent id. In the renamed-away case the old target id already IS the
/// old agent id (config loading rewrites the primary target id to the agent
/// id), so falling back to it reports the right thing.
fn old_agent_label(config: &Config, old_target_id: &str) -> String {
    config
        .array
        .target(old_target_id)
        .map(|target| target.agent.id.clone())
        .unwrap_or_else(|| old_target_id.to_owned())
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
    // The stop's own demotion scopes to the old target id, but the session
    // rename already moved the rows to the new id, so it matches nothing.
    // Demote here under the new id, before the new agent starts, so a fast
    // load/resume cannot race the sweep.
    crate::runtime::agent::supervisor::demote_sessions_on_agent_teardown(
        &state.state,
        new_target_id,
        "agent_stopped",
    )
    .await;
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
    let environment = open_agent_environment(&state.runtime_paths.home, config)?;
    target
        .supervisor
        .start(AgentStartRequest {
            target_id: &target.target_id,
            agent: &config.agent,
            workspace_root: &config.workspace.root,
            home: state.runtime_paths.home.clone(),
            env: environment.env,
            providers: environment.providers,
            state: &state.state,
            session_changes: &state.session_changes,
            event_hub: state.event_hub.clone(),
            permissions: Some(state.permissions.clone()),
            sandbox: config.workspace.sandbox.clone(),
            shell: config.workspace.default_shell.clone(),
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
