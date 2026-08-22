use super::*;

/// The untracked preflight's result: everything resolved before the config is
/// loaded for the tracked steps. Consumed by [`stage::stage_init_config`],
/// which turns it into the [`InitSetup`] the recorded steps run against.
pub(super) struct InitBase {
    pub(super) home: PathBuf,
    pub(super) config_path: PathBuf,
    pub(super) state_path: PathBuf,
    pub(super) registry: RegistryCatalog,
    pub(super) skill_catalog: SkillCatalog,
    pub(super) creating_config: bool,
    pub(super) imported_config: bool,
    pub(super) native_config_provider_preapplied: bool,
    pub(super) legacy_auth: Option<crate::config::LegacyAuthConfig>,
    pub(super) config_status: &'static str,
    pub(super) pending_init_native_config: Option<PendingInitNativeConfig>,
    pub(super) agent_env_collection: AgentEnvCollection,
    pub(super) store: StateStore,
    pub(super) init_run: crate::state::InitRunRecord,
    pub(super) prior_init_steps: Vec<crate::state::InitStepRecord>,
    pub(super) resumed: bool,
    pub(super) recorded_args: Option<RecordedInitArgs>,
    pub(super) mutation: crate::fs_util::AgentConfigMutationFileLock,
}

/// Preflight through run selection: validate the flags, settle the agent, write
/// or validate the config on disk, open the state store, and pick the run row
/// every tracked step below records into.
pub(super) fn prepare_init_base(
    args: &mut InitArgs,
    mode: InitMode,
    output_mode: InitOutputMode,
) -> Result<InitBase> {
    if args.skip_workspace_init() && mode != InitMode::Dev {
        return Err(StackError::InvalidParam {
            field: "--skip-workspace-init",
            reason: "development-only flag; use `acps dev init --skip-workspace-init`".to_owned(),
        });
    }
    if args.resume && args.config_import_source_label().is_some() {
        return Err(StackError::InvalidParam {
            field: "--resume",
            reason: "config import sources cannot be combined with init resume".to_owned(),
        });
    }
    validate_stack_update_args(args)?;
    validate_agent_update_args(args)?;

    let home = home_dir()?;
    let config_path = config::default_config_path()?;
    let mutation = acquire_agent_config_mutation_file_lock(&config_path)?;
    let state_path = default_state_path(&home);
    let config_dir = parent_dir(&config_path)?;
    let state_dir = parent_dir(&state_path)?;

    let mut pending_init_native_config = review_native_config_upload_for_init(args, &config_path)?;
    create_dir_owner_only(config_dir)?;
    create_dir_owner_only(state_dir)?;
    prompt_config_source_if_needed(args, &config_path, &state_path)?;
    let imported_config = import_config_for_init(args, &config_path, output_mode)?;
    let registry = RegistryCatalog::load_with_override(&operator_registry_override(&home))?;

    // Preflight (untracked): new configs must start with a real registry
    // agent. This runs before writing the starter config so a declined or
    // missing first-run selection never leaves `agent.id = "placeholder"` on
    // disk.
    let creating_config = !config_path.exists();
    if creating_config && !args.resume {
        apply_supabase_env_defaults(args)?;
    } else if !creating_config && !args.resume {
        reject_supabase_init_args_for_existing_config(args)?;
        reject_agent_env_refs_for_existing_config(args)?;
        reject_deps_args_for_existing_config(args)?;
        reject_data_source_args_for_existing_config(args)?;
    }
    // A custom agent declared via `--custom-agent-*` is resolved up front; it
    // satisfies the "real agent" requirement without an `--agent` registry id
    // and threads through both config apply sites below.
    let mut custom_agent_spec: Option<CustomAgentSpec> = resolve_custom_agent_spec(args)?;
    if let Some(spec) = &custom_agent_spec {
        reject_registry_id_for_custom_agent(&spec.id, &registry)?;
    }
    if creating_config && !args.resume && args.agent.is_none() && custom_agent_spec.is_none() {
        if !prompts_enabled(args) {
            return Err(StackError::InvalidParam {
                field: "--agent",
                reason: "non-interactive init requires selecting a real agent; run `acps init` in a TTY or pass `--non-interactive --agent <id>` or the `--custom-agent-*` flags".to_owned(),
            });
        }
        match select_agent_for_init(args, &registry)?.ok_or_else(|| StackError::InvalidParam {
            field: "--agent",
            reason: "initializing a new config requires selecting a real agent".to_owned(),
        })? {
            AgentSelection::Registry(entry) => args.agent = Some(entry.id.clone()),
            AgentSelection::Custom(spec) => custom_agent_spec = Some(spec),
        }
    }
    let skill_catalog = SkillCatalog::load_embedded()?;
    if creating_config && !args.resume {
        prompt_environment_configuration_if_needed(args, &registry, &skill_catalog)?;
    }
    // Operator agent env refs (flags + interactive add-loop). On a fresh run the
    // interactive loop also collects masked values; on resume only the replayed
    // `--agent-env-ref` names are re-collected below (interactive values cannot
    // be replayed). Names are appended to `config.agent.env` only after the store
    // verifies them (below), so a failed run never persists an unresolved ref.
    let mut agent_env_collection = if creating_config && !args.resume {
        collect_agent_env_refs_for_init(args, prompts_enabled(args))?
    } else {
        AgentEnvCollection::default()
    };

    // `--resume` skips the real-agent preflight above, so with no config on
    // disk the starter-config branch below would persist `agent.id =
    // "placeholder"` before `resolve_init_run` gets a chance to reject a
    // resume with nothing to resume. Resolving the run first keeps the
    // preflight invariant; a legitimate resume after a manually deleted
    // config still proceeds and repairs the config from the recorded run.
    if args.resume && !config_path.exists() {
        pre_create_owner_only(&state_path)?;
        let store = StateStore::open(&state_path)?;
        store.migrate()?;
        set_owner_only_file(&state_path)?;
        resolve_init_run(args, &store)?;
    }

    let mut legacy_auth = None;
    let mut native_config_provider_preapplied = false;
    let config_status = if config_path.exists() {
        // Repair perms before validation so a failure to parse the file does not
        // leave a permissive config on disk; matches the behavior of `acps status`.
        set_owner_only_file(&config_path)?;
        let loaded_config = Config::load_from_path_with_legacy(&config_path)?;
        legacy_auth = loaded_config.legacy_auth;
        let existing_config = loaded_config.config;
        validate_deployment_overrides_match_existing(args, &existing_config)?;
        reject_starter_only_mcp_args_for_existing_config(args)?;
        if imported_config {
            "imported config"
        } else {
            "validated existing config"
        }
    } else {
        let starter_config = starter_config(args)?;
        let mut new_config = config::load_config_from_str(&starter_config)?;
        // The secret store can predate this init (an orchestrator applied an
        // override before the first config existed), so even a fresh config
        // must prove the override survives the agent being written.
        if let Some(spec) = &custom_agent_spec {
            crate::runtime::agent::switch::ensure_endpoint_override_survives_target(
                &spec.id, false, None,
            )?;
            apply_custom_agent_to_config(&mut new_config, spec);
        } else if let Some(agent_id) = args.agent.as_deref() {
            let entry = registry.lookup_required(agent_id)?;
            entry.ensure_supported()?;
            crate::runtime::agent::switch::ensure_endpoint_override_survives_target(
                &entry.id,
                entry.set_provider_base_url,
                None,
            )?;
            apply_registry_entry_to_config(&mut new_config, entry);
        }
        push_args_deps_to_config(&mut new_config, args)?;
        if let Some(pending) = pending_init_native_config.as_mut() {
            native_config_provider_preapplied = prepare_native_config_for_new_init(
                args,
                &registry,
                pending,
                &mut new_config,
                &config_path,
                &home,
            )?;
        }
        let canonical = new_config.to_canonical_toml()?;
        config::load_config_from_str(&canonical)?;
        write_new_file_owner_only(&config_path, canonical.as_bytes())?;
        Config::load_from_path(&config_path)?;
        "created starter config"
    };

    pre_create_owner_only(&state_path)?;
    let store = StateStore::open(&state_path)?;
    store.migrate()?;
    set_owner_only_file(&state_path)?;
    // Pick the run row: either resume an existing one (explicit `--resume` or
    // auto-detected non-terminal latest) or start fresh. Recording every
    // tracked phase as a step lets `acps init resume` continue from the first
    // unsettled step on the next invocation.
    let init_run = resolve_init_run(args, &store)?;
    let prior_init_steps = store.query_init_steps(&init_run.id)?;
    let resumed = args.resume;
    if resumed {
        init_println!(output_mode, "resuming init run {}", init_run.id);
    } else {
        init_println!(output_mode, "init run {}", init_run.id);
    }

    let recorded_args = if resumed {
        Some(recorded_init_args(&init_run)?)
    } else {
        None
    };
    replay_recorded_args(args, &init_run, resumed, recorded_args.as_ref())?;
    // On resume, re-collect the replayed `--agent-env-ref` names (flags only) so
    // they are re-verified against the now-open store rather than silently
    // dropped. Interactive values from the original run cannot be replayed.
    if resumed {
        agent_env_collection = collect_agent_env_refs_for_init(args, false)?;
    }
    replay_recorded_provider_args(args, resumed, recorded_args.as_ref());

    Ok(InitBase {
        home,
        config_path,
        state_path,
        registry,
        skill_catalog,
        creating_config,
        imported_config,
        native_config_provider_preapplied,
        legacy_auth,
        config_status,
        pending_init_native_config,
        agent_env_collection,
        store,
        init_run,
        prior_init_steps,
        resumed,
        recorded_args,
        mutation,
    })
}

/// Fold a resumed run's recorded declarations back into `args` so a bare
/// `--resume` still drives the run the original invocation asked for.
fn replay_recorded_args(
    args: &mut InitArgs,
    init_run: &crate::state::InitRunRecord,
    resumed: bool,
    recorded_args: Option<&RecordedInitArgs>,
) -> Result<()> {
    if resumed && args.agent.is_none() {
        args.agent = recorded_args
            .and_then(|recorded| recorded.agent.clone())
            .or_else(|| {
                init_run
                    .agent_id
                    .clone()
                    .filter(|agent| agent != STARTER_AGENT_ID)
            });
    }
    #[cfg(feature = "dev-tools")]
    if resumed && let Some(recorded) = recorded_args {
        args.skip_workspace_init = args.skip_workspace_init || recorded.skip_workspace_init;
    }
    // Replay a recorded rotation request so a bare `--resume` cannot silently
    // downgrade a rotating run into a preserving one.
    if resumed && let Some(recorded) = recorded_args {
        args.rotate_keys = args.rotate_keys || recorded.rotate_keys;
    }
    if resumed
        && args.edge.is_none()
        && let Some(recorded) = recorded_args
        && let Some(edge) = recorded.edge.as_deref()
    {
        args.edge = Some(EdgeProviderArg::from_config_value(edge).ok_or_else(|| {
            StackError::InitRunCorrupted {
                reason: format!("init run {} has invalid edge `{edge}`", init_run.id),
            }
        })?);
        args.exposure = recorded
            .exposure
            .as_deref()
            .map(|exposure| {
                EdgeExposureArg::from_config_value(exposure).ok_or_else(|| {
                    StackError::InitRunCorrupted {
                        reason: format!(
                            "init run {} has invalid exposure `{exposure}`",
                            init_run.id
                        ),
                    }
                })
            })
            .transpose()?;
        args.hostname = recorded.hostname.clone();
        if let Some(mode) = recorded.cloudflare_mode.as_deref() {
            args.cloudflare_mode = CloudflareModeArg::from_config_value(mode).ok_or_else(|| {
                StackError::InitRunCorrupted {
                    reason: format!(
                        "init run {} has invalid cloudflare_mode `{mode}`",
                        init_run.id
                    ),
                }
            })?;
        }
        args.cloudflare_api_token_ref = recorded.cloudflare_api_token_ref.clone();
        args.cloudflare_account_id_ref = recorded.cloudflare_account_id_ref.clone();
        if let Some(deployment) = recorded.cloudflared_deployment.as_deref() {
            args.cloudflared_deployment = CloudflaredDeploymentArg::from_config_value(deployment)
                .ok_or_else(|| StackError::InitRunCorrupted {
                reason: format!(
                    "init run {} has invalid cloudflared_deployment `{deployment}`",
                    init_run.id
                ),
            })?;
        }
    }
    if resumed && let Some(recorded) = recorded_args {
        if !args.no_supabase {
            args.no_supabase = recorded.no_supabase;
        }
        if args.supabase_url.is_none() {
            args.supabase_url = recorded.supabase_url.clone();
        }
        if args.supabase_schema.is_none() {
            args.supabase_schema = recorded.supabase_schema.clone();
        }
        if args.supabase_api_key_ref.is_none() {
            args.supabase_api_key_ref = recorded.supabase_api_key_ref.clone();
        }
    }
    // Replay deps-apply, stack-update, and agent-env-ref intents so a bare
    // `--resume` still honors them (their effects run in late steps / are
    // verified after a failure point).
    if resumed && let Some(recorded) = recorded_args {
        if args.agent_env_ref.is_empty() {
            args.agent_env_ref = recorded.agent_env_ref.clone();
        }
        if !args.deps_apply {
            args.deps_apply = recorded.deps_apply;
        }
        if !args.deps_apply_yes {
            args.deps_apply_yes = recorded.deps_apply_yes;
        }
        if args.stack_update.is_none() {
            args.stack_update = recorded.stack_update.clone();
        }
        if args.stack_update_frequency.is_none() {
            args.stack_update_frequency = recorded.stack_update_frequency.clone();
        }
        if args.agent_update.is_none() {
            args.agent_update = recorded.agent_update.clone();
        }
        if args.agent_update_frequency.is_none() {
            args.agent_update_frequency = recorded.agent_update_frequency.clone();
        }
        if args.native_config_revision.is_none() {
            args.native_config_revision = recorded.native_config_revision.clone();
        }
    }
    Ok(())
}

/// The provider/model/mode half of the replay. The credential-shaped flags ride
/// on the provider matching, so a resume that changed provider keeps none of
/// the recorded provider detail.
fn replay_recorded_provider_args(
    args: &mut InitArgs,
    resumed: bool,
    recorded_args: Option<&RecordedInitArgs>,
) {
    if resumed && let Some(recorded) = recorded_args {
        if args.model.is_none() {
            args.model = recorded.model.clone();
        }
        if args.mode.is_none() {
            args.mode = recorded.mode.clone();
        }
        if args.effort.is_none() {
            args.effort = recorded.effort.clone();
        }
        if args.provider.is_none() {
            args.provider = recorded.provider.clone();
        }
        if args.provider.as_deref() == recorded.provider.as_deref() {
            if args.api_key_ref.is_none() {
                args.api_key_ref = recorded.api_key_ref.clone();
            }
            args.custom_provider = args.custom_provider || recorded.custom_provider;
            if args.provider_name.is_none() {
                args.provider_name = recorded.provider_name.clone();
            }
            if args.base_url.is_none() {
                args.base_url = recorded.base_url.clone();
            }
            if args.provider_api.is_none() {
                args.provider_api = recorded.provider_api.clone();
            }
            if args.model_name.is_none() {
                args.model_name = recorded.model_name.clone();
            }
            if args.context.is_none() {
                args.context = recorded.context.clone();
            }
            if args.output_max_tokens.is_none() {
                args.output_max_tokens = recorded.output_max_tokens.clone();
            }
        }
    }
}
