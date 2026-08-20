//! Agent switch: repoint the default target to a different harness.

use super::*;
use crate::runtime::agent::switch_journal::{
    SwitchJournal, SwitchJournalPhase, candidate_fingerprint, load_switch_journal,
    persist_switch_journal, remove_switch_journal,
};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct AgentSwitchRequest {
    agent: String,
    #[serde(default, rename = "drop")]
    drop_configs: bool,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    api_key_ref: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
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
    /// Absent on resumed/no-op retries: install is a pre-commit step that the
    /// interrupted attempt already ran, and re-running it on every retry
    /// would re-burn minutes for no state change.
    #[serde(skip_serializing_if = "Option::is_none")]
    install: Option<AgentInstallResponse>,
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
    let _mutation = state.lock_agent_config_mutation().await?;
    let home = home_dir()?;
    let fresh_config = Config::load_from_path(&state.runtime_paths.config_path)?;
    let registry = RegistryCatalog::load_with_override(
        &home.join(".config").join("acp-stack").join("agents.toml"),
    )?;
    // The journal gates dispatch: a same-target retry of an interrupted or
    // completed switch must be recognized before the fresh-path validation
    // below rejects it as "already configured".
    let resume_journal = match load_switch_journal(&state.runtime_paths.config_path)? {
        Some(journal) => match classify_switch_journal(&journal, &body.agent, &fresh_config)? {
            SwitchJournalAction::NoOp => {
                return Ok(completed_switch_response(
                    &fresh_config,
                    &journal.target_agent_id,
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
    // A client that re-delivers the stored harness on every agent-config PATCH
    // names the current target, so a bare switch to the target that is already
    // the default must converge as a side-effect-free success — the
    // never-switched twin of the completed-journal retry above. Flagged bodies
    // keep their explicit-intent rejections in the existing-target path below.
    if resume_journal.is_none()
        && fresh_config.array.primary_target == body.agent
        && !body.drop_configs
        && body.provider.is_none()
        && body.api_key_ref.is_none()
    {
        return Ok(completed_switch_response(
            &fresh_config,
            &fresh_config.agent.id,
        ));
    }
    if fresh_config.array.target(&body.agent).is_some() {
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
    crate::runtime::agent::provider_model_catalog::refresh_provider_models_best_effort(
        &home,
        &candidate_config,
    )
    .await;
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
    let link_outcome = link_agent_skills_best_effort(&home, target_entry);

    let old_target_id = fresh_config.array.primary_target.clone();
    let old_target = state.agent_target(&old_target_id)?;
    let was_running = old_target.supervisor.snapshot().await.state.as_wire_str() == "running";
    let mut journal = SwitchJournal {
        old_target_id: old_target_id.clone(),
        new_target_id: body.agent.clone(),
        target_agent_id: plan.target_agent_id.clone(),
        candidate_fingerprint: candidate_fingerprint(&canonical),
        was_running,
        phase: SwitchJournalPhase::Planned,
    };
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

    let response = AgentSwitchResponse {
        old_agent_id: plan.old_agent_id,
        agent_id: plan.target_agent_id,
        provider_status: plan.provider_status.label(),
        provider: plan.provider_status.provider_id().map(str::to_owned),
        api_key_ref: plan.provider_status.api_key_ref().map(str::to_owned),
        required_env_refs: plan.required_env_refs,
        secret_migrations,
        install: Some(install),
        restarted: was_running,
        restart_started,
        set_model: target_entry.set_model,
        models,
        follow_up: target_entry
            .set_model
            .then_some("acps agent set --model <model-id>"),
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
    // Selecting an existing target repoints the native config the override
    // lives in, so it faces the same survival check as a planned switch.
    crate::runtime::agent::switch::ensure_endpoint_override_survives_target(
        &target_entry.id,
        target_entry.set_provider_base_url,
        candidate_config
            .agent
            .provider
            .as_ref()
            .map(|provider| provider.id.as_str()),
    )?;
    let _env = open_agent_env(&candidate_config)?;
    let required_env_refs = candidate_config.agent.env.clone();

    let install = install_agent_for_config(state, &candidate_config).await?;
    crate::runtime::agent::provider_model_catalog::refresh_provider_models_best_effort(
        home,
        &candidate_config,
    )
    .await;
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
    let link_outcome = link_agent_skills_best_effort(home, target_entry);

    let old_target_id = fresh_config.array.primary_target.clone();
    let old_target = state.agent_target(&old_target_id)?;
    let was_running = old_target.supervisor.snapshot().await.state.as_wire_str() == "running";
    let mut journal = SwitchJournal {
        old_target_id: old_target_id.clone(),
        new_target_id: body.agent.clone(),
        target_agent_id: candidate_config.agent.id.clone(),
        candidate_fingerprint: candidate_fingerprint(&canonical),
        was_running,
        phase: SwitchJournalPhase::Planned,
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
        install: Some(install),
        restarted: was_running,
        restart_started,
        set_model: false,
        models: Vec::new(),
        follow_up: None,
        provisioned,
        skills_port,
        skills_link: link_outcome.report,
        skills_link_error: link_outcome.error,
        cleaned_configs: Vec::new(),
        cleanup_errors: Vec::new(),
    }))
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
) -> Result<SwitchJournalAction> {
    let same_target = journal.requested_target_matches(requested);
    // Post-commit the on-disk primary target id is rewritten to the target
    // agent id (config canonicalization invariant), so the committed marker
    // is the agent id, not the requested target id.
    let committed_on_disk = fresh_config.agent.id == journal.target_agent_id;
    if journal.phase == SwitchJournalPhase::Completed {
        if same_target && committed_on_disk {
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
        // The config write is the commit marker (the session rename strictly
        // precedes it), so disk showing the new primary means the switch
        // committed. Verify the bytes match the journaled candidate before
        // trusting them: an operator edit between attempts must not be
        // silently adopted as the in-flight switch's outcome.
        let on_disk = fresh_config.to_canonical_toml()?;
        if candidate_fingerprint(&on_disk) != journal.candidate_fingerprint {
            return Err(StackError::AgentSwitchConflict {
                reason: format!(
                    "the on-disk config for `{}` does not match the interrupted switch's candidate; repair the config or the switch journal before retrying",
                    journal.new_target_id
                ),
            });
        }
        return Ok(SwitchJournalAction::ResumeCommitted);
    }
    Ok(SwitchJournalAction::ResumeFromCommitBoundary)
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

/// Shared commit boundary for both switch paths: journal the plan, apply the
/// commit (session rename + canonical config write), re-apply the runtime,
/// and advance the journal to RuntimeApplied. The caller runs its own
/// post-commit cleanup and then drives the journal to Completed.
async fn commit_switch_and_apply_runtime(
    commit: SwitchCommit<'_>,
    journal: &mut SwitchJournal,
) -> Result<bool> {
    let state = commit.state;
    // A same-target resume must reproduce the journaled candidate byte for
    // byte; a divergence means the operator edited config between attempts
    // and this retry would converge on a different switch than the one that
    // was interrupted.
    if let Some(prior) = commit.resume_journal
        && prior.candidate_fingerprint != journal.candidate_fingerprint
    {
        return Err(StackError::AgentSwitchConflict {
            reason: format!(
                "the recomputed switch to `{}` no longer matches the interrupted attempt's candidate; restore the previous config or repair the switch journal before retrying",
                prior.new_target_id
            ),
        });
    }
    persist_switch_journal(&state.runtime_paths.config_path, journal)?;
    if commit.rename_sessions {
        // Rename sessions to the new primary target BEFORE writing the new
        // config. The rename can fail (e.g. a UNIQUE(target_id,
        // agent_session_id) collision is detected up front), and if it does
        // the on-disk config must stay untouched so config and DB never
        // diverge. Re-running after a crash between rename and write is a
        // no-op: zero rows still carry the old target id.
        let rename_result = {
            let store = state.state.lock().await;
            store.rename_session_target_id(
                commit.old_target_id,
                &commit.candidate_config.array.primary_target,
            )
        };
        if let Err(rename_error) = rename_result {
            // The collision check rejects before any row moves and the config
            // write below has not run, so nothing durable changed: drop the
            // Planned journal persisted above rather than strand an
            // in-progress record that would 409 every later switch while a
            // same-target retry just reproduces the collision.
            if matches!(rename_error, StackError::SessionTargetRenameConflict { .. }) {
                remove_switch_journal(&state.runtime_paths.config_path)?;
            }
            return Err(rename_error);
        }
    }
    crate::fs_util::atomic_write_owner_only(
        &state.runtime_paths.config_path,
        commit.canonical.as_bytes(),
    )?;
    state.refresh_array_runtime_from_disk().await?;
    journal.phase = SwitchJournalPhase::Committed;
    persist_switch_journal(&state.runtime_paths.config_path, journal)?;
    let restart_started = apply_switch_runtime(
        state,
        commit.old_target_id,
        &commit.candidate_config.array.primary_target,
        commit.candidate_config,
        commit.was_running,
    )
    .await?;
    journal.phase = SwitchJournalPhase::RuntimeApplied;
    persist_switch_journal(&state.runtime_paths.config_path, journal)?;
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

    // `--drop` cleanup cannot be reconstructed on a post-commit resume: the
    // source agent's identity was renamed away with its target, so there is
    // no trustworthy config left to clean against. Surface the skip rather
    // than silently dropping the flag.
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
        required_env_refs: candidate_config.agent.env.clone(),
        secret_migrations: Vec::new(),
        install: None,
        restarted: journal.was_running,
        restart_started,
        set_model: false,
        models: Vec::new(),
        follow_up: None,
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
/// response is a pure no-op — no rewrite, no stop/start, no install re-run.
/// `old_agent_id` reports the current agent (which is the target) because
/// nothing changed.
fn completed_switch_response(
    fresh_config: &Config,
    target_agent_id: &str,
) -> ApiSuccess<AgentSwitchResponse> {
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
        required_env_refs: fresh_config.agent.env.clone(),
        secret_migrations: Vec::new(),
        install: None,
        restarted: false,
        restart_started: false,
        set_model: false,
        models: Vec::new(),
        follow_up: None,
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
