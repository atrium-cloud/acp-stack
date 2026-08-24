use super::*;

/// Step: agent_install — install the configured agent if requested.
pub(super) fn run_agent_install_step(flow: &mut InitFlow) -> Result<()> {
    let output_mode = flow.output_mode;
    let install_requested = should_install_agent(&flow.config, &flow.registry)?;
    let install_step_needs_resume =
        step_needs_resume(&flow.prior_init_steps, step_kind::AGENT_INSTALL);
    if !(install_requested || install_step_needs_resume) {
        return Ok(());
    }
    let install_interactive = prompts_enabled(&flow.args);
    let verify_config = flow.config.clone();
    let verify_workspace_root = PathBuf::from(flow.config.workspace.root.clone());
    let verify_local_bin_dir = local_bin_dir(&flow.home);
    let store = &flow.store;
    let home = &flow.home;
    let config = &flow.config;
    let registry = &flow.registry;
    let args = &flow.args;
    let install_outcome = &mut flow.install_outcome;
    let result = record_init_step(
        store,
        &flow.init_run,
        2,
        step_kind::AGENT_INSTALL,
        || {
            Ok(installer_postcondition_holds(
                &verify_config,
                &verify_workspace_root,
                &verify_local_bin_dir,
            ))
        },
        || {
            if !args.skip_workspace_init() {
                crate::runtime::workspace_sources::workspace_init::prepare_workspace_base_dirs(
                    &config.workspace,
                )?;
            }
            // Snapshot before and after so the payload lists exactly the installer rows this
            // attempt produced.
            let prior_ids: std::collections::HashSet<String> = store
                .query_installer_runs_filtered(Some(&config.agent.id), 1024)
                .map(|rows| rows.into_iter().map(|r| r.id).collect())
                .unwrap_or_default();
            let install_started = std::time::Instant::now();
            let outcome = run_install_with_retry(
                |attempt| {
                    let message = agent_install_progress_message(attempt);
                    if install_interactive {
                        prompt::with_spinner(&message, || {
                            install_configured_agent(home, config, registry, store)
                        })
                    } else {
                        init_println!(output_mode, "progress: {message}");
                        install_configured_agent(home, config, registry, store)
                    }
                },
                |attempt, error, delay| {
                    init_println!(
                        output_mode,
                        "agent install attempt {attempt} failed: {error}"
                    );
                    init_println!(output_mode, "retrying in {}s", delay.as_secs());
                    std::thread::sleep(delay);
                },
                || install_started.elapsed(),
            )?;
            let label = outcome.label();
            let path = outcome.path().display().to_string();
            let new_installer_run_ids: Vec<String> = store
                .query_installer_runs_filtered(Some(&config.agent.id), 1024)
                .map(|rows| {
                    rows.into_iter()
                        .map(|r| r.id)
                        .filter(|id| !prior_ids.contains(id))
                        .collect()
                })
                .unwrap_or_default();
            *install_outcome = Some(outcome.clone());
            let payload = serde_json::json!({
                "label": label,
                "path": path,
                "installer_run_ids": new_installer_run_ids,
            });
            Ok(StepOutcome::with_payload(payload.to_string()))
        },
    );
    if let Err(error) = result {
        return finalize_with_error(&flow.store, &flow.init_run, error);
    }
    Ok(())
}

/// Step: native_config_import — apply the reviewed native global config after installation.
pub(super) fn run_native_config_import_step(flow: &mut InitFlow) -> Result<()> {
    let output_mode = flow.output_mode;
    let Some(record) = flow.init_native_config_record.as_mut() else {
        return Ok(());
    };
    init_println!(output_mode, "progress: importing native Agent config");
    let already_applied = record.phase
        == crate::runtime::agent::native_config_import::NativeConfigOperationPhase::Applied;
    let config_path = &flow.config_path;
    let state_path = &flow.state_path;
    let home = &flow.home;
    let config = &mut flow.config;
    let handoff_context = &mut flow.handoff_context;
    let key_handover = &mut flow.key_handover;
    let result = record_init_step(
        &flow.store,
        &flow.init_run,
        11,
        step_kind::NATIVE_CONFIG_IMPORT,
        || Ok(already_applied),
        || {
            let (updated, operation) =
                native_config::apply_for_init(record, config_path, state_path, home)?;
            *config = updated;
            prompt::emit_state_signal(|| InitStateSignal::CategorySettled {
                category: InitCategory::NativeConfig,
                value: Some(operation.revision.clone()),
            });
            handoff_context.native_config_import = Some(operation.clone());
            if let Some(context) = key_handover.failure_context.as_mut() {
                context.native_config_import = Some(operation.clone());
            }
            Ok(StepOutcome::with_payload(
                serde_json::json!({ "operation": operation }).to_string(),
            ))
        },
    );
    if let Err(error) = result {
        return finalize_with_error(&flow.store, &flow.init_run, error);
    }
    let applied = flow
        .init_native_config_record
        .as_ref()
        .is_some_and(|record| {
            record.phase
                == crate::runtime::agent::native_config_import::NativeConfigOperationPhase::Applied
        });
    if applied {
        let operation = flow
            .init_native_config_record
            .as_ref()
            .map(|record| record.operation.clone());
        flow.handoff_context.native_config_import = operation.clone();
        if let Some(context) = flow.key_handover.failure_context.as_mut() {
            context.native_config_import = operation;
        }
        flow.config = Config::load_from_path(&flow.config_path)?;
    }
    Ok(())
}

/// Step: agent_skills_install — install selected Agent Skills before the first launch.
pub(super) fn run_agent_skills_install_step(flow: &mut InitFlow) -> Result<()> {
    let output_mode = flow.output_mode;
    let skill_step_needs_resume =
        step_needs_resume(&flow.prior_init_steps, step_kind::AGENT_SKILLS_INSTALL);
    if !(flow.skill_install_plan.is_some() || skill_step_needs_resume) {
        return Ok(());
    }
    init_println!(output_mode, "progress: installing agent skills");
    let Some(plan) = flow.skill_install_plan.clone() else {
        return finalize_with_error(
            &flow.store,
            &flow.init_run,
            StackError::InitRunCorrupted {
                reason: format!(
                    "init run {} has a failed agent_skills_install step but no recorded skill install request",
                    flow.init_run.id
                ),
            },
        );
    };
    let verify_plan = plan.clone();
    let prior_init_steps = &flow.prior_init_steps;
    let home = &flow.home;
    let config = &flow.config;
    let registry = &flow.registry;
    let skill_install_reports = &mut flow.skill_install_reports;
    let result = record_init_step(
        &flow.store,
        &flow.init_run,
        9,
        step_kind::AGENT_SKILLS_INSTALL,
        || {
            Ok(skill_install_postcondition_holds(
                &verify_plan,
                prior_init_steps,
            ))
        },
        || {
            let (reports, link_outcome) = install_init_skills(&plan, home, config, registry)?;
            if let Some(link_error) = &link_outcome.error {
                init_println!(
                    output_mode,
                    "warning: skill link refresh failed: {link_error}"
                );
            }
            let requested_skills = plan
                .selections
                .iter()
                .map(|selection| {
                    serde_json::json!({
                        "source_id": selection.source.id,
                        "selectors": selection.skills,
                    })
                })
                .collect::<Vec<_>>();
            let payload = serde_json::to_string(&serde_json::json!({
                "request": { "skills": requested_skills },
                "reports": &reports,
                "link": &link_outcome.report,
                "link_error": &link_outcome.error,
            }))
            .map_err(|source| StackError::SkillInstallFailed {
                reason: format!("serialize skill install report: {source}"),
            })?;
            prompt::emit_state_signal(|| InitStateSignal::CategorySettled {
                category: InitCategory::Skills,
                value: installed_skill_names(&reports),
            });
            *skill_install_reports = reports;
            Ok(StepOutcome::with_payload(payload))
        },
    );
    if let Err(error) = result {
        return finalize_with_error(&flow.store, &flow.init_run, error);
    }
    Ok(())
}
