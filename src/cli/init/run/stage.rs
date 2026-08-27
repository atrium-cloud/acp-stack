use super::*;

/// Everything between "the run row exists" and "the first tracked step runs".
/// Every failure here MUST still finalize the run, or the pending row is adopted by a later `--resume`.
pub(super) fn stage_init_config(
    mut args: InitArgs,
    base: InitBase,
    output_mode: InitOutputMode,
    shared_secret_store: Option<SharedSecretStore>,
) -> Result<InitSetup> {
    let InitBase {
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
    } = base;

    let mut config = Config::load_from_path(&config_path)
        .or_else(|error| finalize_failure(&store, &init_run, error))?;
    // Skip the registry re-apply for a custom agent (a `lookup_required` on its id would fail) or
    // an imported config with no explicit `--agent`; explicit flags override the skip.
    let custom_agent_flags_present = resolve_custom_agent_spec(&args)
        .or_else(|error| finalize_failure(&store, &init_run, error))?
        .is_some();
    let selected_agent = if !custom_agent_flags_present
        && args.agent.is_none()
        && (is_custom_agent(&config, &registry) || imported_config)
    {
        None
    } else {
        select_agent_for_init(&args, &registry)
            .or_else(|error| finalize_failure(&store, &init_run, error))?
    };
    let adapter_override_action = match resolve_adapter_override_action(&args) {
        Ok(action) => action,
        Err(error) => return finalize_failure(&store, &init_run, error),
    };
    let agent_applied = match &selected_agent {
        Some(AgentSelection::Registry(entry)) => {
            // Fail fast on agents the runtime cannot drive headlessly, rather than at first session spawn.
            entry
                .ensure_supported()
                .or_else(|error| finalize_failure(&store, &init_run, error))?;
            // Re-confirming the same agent keeps its configured provider; an agent change clears it.
            let kept_provider_id = if entry.id == config.agent.id {
                config
                    .agent
                    .provider
                    .as_ref()
                    .map(|provider| provider.id.clone())
            } else {
                None
            };
            crate::runtime::agent::switch::ensure_endpoint_override_survives_target(
                &entry.id,
                entry.set_provider_base_url,
                kept_provider_id.as_deref(),
            )
            .or_else(|error| finalize_failure(&store, &init_run, error))?;
            apply_registry_entry_to_config(&mut config, entry);
            // Must follow the registry re-apply, which clears a stored override on an agent change.
            apply_adapter_override_action(&mut config, &adapter_override_action);
            apply_agent_launch_command(&mut config, entry);
            true
        }
        Some(AgentSelection::Custom(spec)) => {
            // A custom agent has no registry-managed native config surface for an endpoint override.
            crate::runtime::agent::switch::ensure_endpoint_override_survives_target(
                &spec.id, false, None,
            )
            .or_else(|error| finalize_failure(&store, &init_run, error))?;
            apply_custom_agent_to_config(&mut config, spec);
            true
        }
        None => {
            match &adapter_override_action {
                Some(AdapterOverrideAction::Set(_)) => {
                    let entry = registry
                        .lookup(&config.agent.id)
                        .ok_or_else(|| StackError::InvalidParam {
                            field: "--adapter-override-command",
                            reason: format!(
                                "`{}` is not a registry agent; designated adapters apply to registry agents only (use the --custom-agent-* flags for escape-hatch agents)",
                                config.agent.id
                            ),
                        })
                        .or_else(|error| finalize_failure(&store, &init_run, error))?;
                    let changed =
                        apply_adapter_override_action(&mut config, &adapter_override_action);
                    apply_agent_launch_command(&mut config, entry);
                    changed
                }
                Some(AdapterOverrideAction::Clear) => {
                    let changed =
                        apply_adapter_override_action(&mut config, &adapter_override_action);
                    // Clearing must stay available after the agent leaves the registry, which is
                    // precisely when a stale designation needs removing; the curated launch command
                    // can only be restored while the entry still exists.
                    if let Some(entry) = registry.lookup(&config.agent.id) {
                        apply_agent_launch_command(&mut config, entry);
                    }
                    changed
                }
                None => false,
            }
        }
    };
    // A stored designation on a non-registry agent drives no install or launch lane; reachable via
    // imported configs, which skip the registry re-apply.
    if config.agent.adapter_override.is_some() && registry.lookup(&config.agent.id).is_none() {
        return finalize_failure(
            &store,
            &init_run,
            StackError::InvalidParam {
                field: "agent.adapter_override",
                reason: format!(
                    "`{}` is not a registry agent; designated adapters apply to registry agents only",
                    config.agent.id
                ),
            },
        );
    }
    let agent_selected = selected_agent.is_some();
    if agent_applied {
        let rewrite = (|| -> Result<()> {
            let canonical = config.to_canonical_toml()?;
            config = config::load_config_from_str(&canonical)?;
            atomic_write_owner_only(&config_path, canonical.as_bytes())
        })();
        if let Err(error) = rewrite {
            return finalize_failure(&store, &init_run, error);
        }
    }
    // The agent is final on every path into this point, which is what makes the registry-derived
    // verdicts below trustworthy.
    let native_config_pending =
        pending_init_native_config.is_some() || args.native_config_revision.is_some();
    prompt::emit_state_signals(|| {
        agent_settlement_signals(&config, &registry, &args, native_config_pending)
    });

    let recorded_native_config_operation: Option<
        crate::runtime::agent::native_config_import::NativeConfigOperation,
    > = match prior_init_steps
        .iter()
        .find(|step| {
            step.kind == step_kind::NATIVE_CONFIG_IMPORT
                && matches!(
                    step.status.as_str(),
                    INIT_STEP_SUCCEEDED | INIT_STEP_SKIPPED
                )
        })
        .map(|step| {
            let payload: serde_json::Value =
                serde_json::from_str(&step.payload_json).map_err(|_| {
                    StackError::InitRunCorrupted {
                        reason: "native config import step payload is invalid".to_owned(),
                    }
                })?;
            serde_json::from_value(payload.get("operation").cloned().ok_or_else(|| {
                StackError::InitRunCorrupted {
                    reason: "native config import step omitted its operation".to_owned(),
                }
            })?)
            .map_err(|_| StackError::InitRunCorrupted {
                reason: "native config import step operation is invalid".to_owned(),
            })
        })
        .transpose()
    {
        Ok(operation) => operation,
        Err(error) => return finalize_failure(&store, &init_run, error),
    };
    if pending_init_native_config.is_some() || args.native_config_revision.is_some() {
        if let Some(operation) = recorded_native_config_operation.as_ref() {
            args.provider = None;
            if operation.agent_config.model.is_some()
                && args.model.as_deref() != operation.agent_config.model.as_deref()
            {
                args.model = None;
            }
        } else if let Some(provider_id) = args.provider.clone() {
            if !native_config_provider_preapplied {
                let preapply = (|| -> Result<()> {
                    apply_provider_to_config(
                        &args,
                        &registry,
                        &mut config,
                        &config_path,
                        provider_id,
                    )?;
                    let canonical = config.to_canonical_toml()?;
                    config = config::load_config_from_str(&canonical)?;
                    atomic_write_owner_only(&config_path, canonical.as_bytes())
                })();
                if let Err(error) = preapply {
                    return finalize_failure(&store, &init_run, error);
                }
            }
            args.provider = None;
        }
    }
    let init_native_config_record = match native_config::stage_for_init(
        pending_init_native_config.as_ref(),
        args.native_config_revision.as_deref(),
        recorded_native_config_operation.as_ref(),
        &init_run.id,
        &config,
        &config_path,
        &state_path,
        &home,
    ) {
        Ok(record) => record,
        Err(error) => return finalize_failure(&store, &init_run, error),
    };
    if init_native_config_record.as_ref().is_some_and(|record| {
        record.prepared.as_ref().is_some_and(|prepared| {
            prepared
                .selected_managed_field_ids
                .iter()
                .any(|id| id == "model")
        })
    }) {
        args.model = None;
    }

    let edge_requested = apply_edge_profile_to_config(&args, &mut config)
        .or_else(|error| finalize_failure(&store, &init_run, error))?;
    let supabase_configured = apply_supabase_to_config_for_init(&args, &mut config)
        .or_else(|error| finalize_failure(&store, &init_run, error))?;
    prompt_init_skills_if_needed(&mut args, &config, &registry, &skill_catalog)
        .or_else(|error| finalize_failure(&store, &init_run, error))?;
    if edge_requested || supabase_configured {
        let rewrite = (|| -> Result<()> {
            let canonical = config.to_canonical_toml()?;
            config = config::load_config_from_str(&canonical)?;
            atomic_write_owner_only(&config_path, canonical.as_bytes())
        })();
        if let Err(error) = rewrite {
            return finalize_failure(&store, &init_run, error);
        }
    }

    if resumed
        && !args.no_skills
        && !args.essential_skills
        && args.skills_source.is_none()
        && args.skills.is_empty()
        && let Some(recorded) = recorded_args.as_ref()
    {
        restore_recorded_skill_plan(&mut args, recorded);
    }
    if step_needs_resume(&prior_init_steps, step_kind::PROVIDER_CONFIGURE)
        && args.provider.is_none()
    {
        args.provider = config
            .agent
            .provider
            .as_ref()
            .map(|provider| provider.id.clone());
        // A failed provider_configure step that owned only model can legitimately resume without
        // `--provider`, so error only when the provider is known to be required and absent.
        let resume_recorded_provider = recorded_args.as_ref().and_then(|r| r.provider.clone());
        if args.provider.is_none() && resume_recorded_provider.is_some() {
            return finalize_failure(
                &store,
                &init_run,
                StackError::InitRunCorrupted {
                    reason: format!(
                        "init run {} has a failed provider_configure step recorded with a provider but no provider id is available now; pass --provider on resume",
                        init_run.id
                    ),
                },
            );
        }
    }
    if step_needs_resume(&prior_init_steps, step_kind::TESTFLIGHT) {
        args.testflight = true;
        args.skip_testflight = false;
    } else if resumed
        && !args.testflight
        && !args.skip_testflight
        && let Some(recorded) = recorded_args.as_ref()
    {
        args.testflight = recorded.testflight;
        args.skip_testflight = recorded.skip_testflight;
    }
    if let Err(error) = preflight_provider_for_init(&args, &registry, &config, &config_path)
        .and_then(|_| preflight_model_and_mode_for_init(&args, &registry, &config, &config_path))
    {
        return finalize_failure(&store, &init_run, error);
    }

    // An unsatisfiable skills declaration is a hard error rather than a silent skip.
    let skill_install_plan =
        match resolve_skill_install_plan(&args, &home, &config, &registry, &skill_catalog) {
            Ok(plan) => plan,
            Err(error) => return finalize_failure(&store, &init_run, error),
        };

    Ok(InitSetup {
        args,
        output_mode,
        home,
        config_path,
        state_path,
        registry,
        config,
        config_status,
        creating_config,
        legacy_auth,
        agent_env_collection,
        store,
        init_run,
        prior_init_steps,
        init_native_config_record,
        edge_requested,
        agent_selected,
        skill_install_plan,
        mutation,
        shared_secret_store,
    })
}
