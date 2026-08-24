use super::*;

/// Step: provider_configure — write provider/model into the config and persist
/// canonical TOML if anything changed.
pub(super) fn run_provider_configure_step(flow: &mut InitFlow) -> Result<()> {
    let output_mode = flow.output_mode;
    init_println!(output_mode, "progress: configuring provider and model");
    let provider_verify_config = flow.config.clone();
    let provider_verify_home = flow.home.clone();
    let agent_selected = flow.agent_selected;
    let edge_requested = flow.edge_requested;
    let home = &flow.home;
    let config_path = &flow.config_path;
    let registry = &flow.registry;
    let args = &flow.args;
    let config = &mut flow.config;
    let secret_store = &mut flow.secret_store;
    let result = record_init_step(
        &flow.store,
        &flow.init_run,
        4,
        step_kind::PROVIDER_CONFIGURE,
        || {
            // Idempotent only when no lane this step owns has an explicit change
            // requested; otherwise a resumed `--model`/`--mode`/`--effort` would be
            // skipped because the prior succeeded row passes the verifier.
            let secret_store = SecretStore::open(&provider_verify_home)?;
            Ok(args.provider.is_none()
                && args.model.is_none()
                && args.mode.is_none()
                && args.effort.is_none()
                && configured_provider_refs_satisfied(
                    registry,
                    &provider_verify_config,
                    &secret_store,
                ))
        },
        || {
            // All three lanes share one step, so each badges itself before the error
            // propagates; a step-level failure alone could not say which one broke.
            let provider_configured =
                configure_provider_for_init(args, registry, config, config_path, secret_store)
                    .inspect_err(|error| signal_category_failed(InitCategory::Provider, error))?;
            prompt::emit_state_signal(|| InitStateSignal::CategorySettled {
                category: InitCategory::Provider,
                value: config
                    .agent
                    .provider
                    .as_ref()
                    .map(|provider| provider.id.clone()),
            });
            let model_mode_outcome = configure_model_and_mode_for_init(
                args,
                home,
                registry,
                config,
                config_path,
                secret_store,
            )?;
            // Custom agents skip provider/model discovery and would otherwise never
            // spawn during init, so a broken binary must be caught here.
            if is_custom_agent(config, registry) {
                verify_agent_acp_connection(home, config, output_mode.is_text())?;
            }
            let model_mode_changed =
                matches!(model_mode_outcome.model_action, ModelModeAction::Set)
                    || matches!(model_mode_outcome.mode_action, ModelModeAction::Set)
                    || matches!(model_mode_outcome.effort_action, ModelModeAction::Set);
            let subagent_configured =
                configure_subagent_inherit_for_init(prompts_enabled(args), registry, config)?;
            if agent_selected
                || provider_configured
                || edge_requested
                || model_mode_changed
                || subagent_configured
            {
                let canonical = config.to_canonical_toml()?;
                *config = config::load_config_from_str(&canonical)?;
                atomic_write_owner_only(config_path, canonical.as_bytes())?;
            }
            Ok(StepOutcome::with_payload(format!(
                r#"{{"provider_configured":{provider_configured},"model_action":"{:?}","mode_action":"{:?}","effort_action":"{:?}","subagent_configured":{subagent_configured}}}"#,
                model_mode_outcome.model_action,
                model_mode_outcome.mode_action,
                model_mode_outcome.effort_action,
            )))
        },
    );
    if let Err(error) = result {
        return finalize_with_error(&flow.store, &flow.init_run, error);
    }
    Ok(())
}

/// acp-stack auto-update: configure `[updates.acp_stack]` before the summary.
/// Flags apply on any run; the interactive prompt is suppressed on resume.
pub(super) fn configure_stack_update(flow: &mut InitFlow) -> Result<()> {
    let stack_update_outcome = (|| -> Result<()> {
        let interactive = prompts_enabled(&flow.args) && !flow.args.resume && flow.creating_config;
        let changed = configure_stack_update_for_init(&flow.args, &mut flow.config, interactive)?;
        if changed {
            let canonical = flow.config.to_canonical_toml()?;
            flow.config = config::load_config_from_str(&canonical)?;
            atomic_write_owner_only(&flow.config_path, canonical.as_bytes())?;
        }
        Ok(())
    })();
    if let Err(error) = stack_update_outcome {
        return finalize_with_error(&flow.store, &flow.init_run, error);
    }
    Ok(())
}

/// Managed agent auto-update: override the seeded `[agent.auto_update]` default.
/// Managed-ness comes from the registry, not block presence, so an imported config
/// missing the block is still treated as managed.
pub(super) fn configure_agent_update(flow: &mut InitFlow) -> Result<()> {
    let agent_update_outcome = (|| -> Result<()> {
        let managed = !is_custom_agent(&flow.config, &flow.registry);
        let interactive = prompts_enabled(&flow.args) && !flow.args.resume && flow.creating_config;
        let changed =
            configure_agent_update_for_init(&flow.args, &mut flow.config, managed, interactive)?;
        if changed {
            let canonical = flow.config.to_canonical_toml()?;
            flow.config = config::load_config_from_str(&canonical)?;
            atomic_write_owner_only(&flow.config_path, canonical.as_bytes())?;
        }
        Ok(())
    })();
    if let Err(error) = agent_update_outcome {
        return finalize_with_error(&flow.store, &flow.init_run, error);
    }
    Ok(())
}
