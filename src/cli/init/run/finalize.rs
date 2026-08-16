use super::*;

/// Step: init_complete — record the durable "initialized" event.
/// Resume verifier: the event is already present in the unified log.
pub(super) fn run_init_complete_step(flow: &mut InitFlow) -> Result<()> {
    let verify_run_id = flow.init_run.id.clone();
    let store = &flow.store;
    let init_run = &flow.init_run;
    let result = record_init_step(
        store,
        init_run,
        7,
        step_kind::INIT_COMPLETE,
        || Ok(init_complete_event_already_recorded(store, &verify_run_id)),
        || {
            store.append_event_with_source(
                "info",
                "init.completed",
                crate::state::EVENT_SOURCE_CLI,
                "initialized",
                &serde_json::json!({ "init_run_id": init_run.id }).to_string(),
            )?;
            Ok(StepOutcome::empty())
        },
    );
    if let Err(error) = result {
        return finalize_with_error(&flow.store, &flow.init_run, error);
    }
    Ok(())
}

/// The operator-facing summary of everything this run settled.
pub(super) fn print_init_summary(flow: &InitFlow) {
    let output_mode = flow.output_mode;
    init_println!(output_mode, "initialized acp-stack");
    init_println!(
        output_mode,
        "{}: {}",
        flow.config_status,
        flow.config_path.display()
    );
    init_println!(output_mode, "state: {}", flow.state_path.display());
    init_println!(
        output_mode,
        "secrets: {}",
        flow.secret_store.store_path().display()
    );
    init_println!(
        output_mode,
        "age key: {}",
        age_key_path(&flow.home).display()
    );
    init_println!(output_mode, "auth: {}", flow.auth_status);
    init_println!(
        output_mode,
        "agent: {} ({})",
        flow.config.agent.name,
        flow.config.agent.id
    );
    if let Some(outcome) = flow.install_outcome.as_ref() {
        init_println!(output_mode, "agent install: {}", outcome.label());
        init_println!(output_mode, "agent path: {}", outcome.path().display());
        init_println!(output_mode, "agent sha256: {}", outcome.sha256());
    }
    for report in &flow.skill_install_reports {
        for entry in &report.installed {
            init_println!(
                output_mode,
                "skill installed: {} -> {}",
                entry.name,
                entry.path.display()
            );
        }
        for entry in &report.skipped {
            init_println!(output_mode, "skill already installed: {}", entry.name);
        }
    }
    for provisioned in &flow.provisioned_agent_configs {
        init_println!(
            output_mode,
            "{}: {}",
            provisioned.label,
            provisioned.path.display()
        );
    }
    for artifact in &flow.provisioned_edge_artifacts {
        init_println!(
            output_mode,
            "{}: {}",
            artifact.label,
            artifact.path.display()
        );
    }
    if let Some(materialize) = &flow.materialize_report {
        init_println!(
            output_mode,
            "workspace root: {}",
            materialize.root.display()
        );
        init_println!(
            output_mode,
            "workspace uploads: {}",
            materialize.uploads.display()
        );
        for entry in &materialize.code {
            init_println!(
                output_mode,
                "code source ({:?}): {}",
                entry.outcome,
                entry.destination.display()
            );
        }
        for entry in &materialize.data {
            init_println!(
                output_mode,
                "data source ({:?}): {}",
                entry.outcome,
                entry.destination.display()
            );
        }
    }

    // Ignored-feature notices are text-lane only, deliberately bypassing
    // `init_println!`: hosted progress frames reach end users, who must not
    // see them — the platform reads `ignored_features` from the handoff
    // payload instead.
    if output_mode.is_text() {
        for ignored in &flow.ignored_features {
            let label = match ignored.feature {
                crate::runtime::agent::acp_bridge::IGNORED_FEATURE_MCP_SERVER => "mcp server",
                other => other,
            };
            println!(
                "ignored: {label} \"{}\" ({}) — not supported by this agent's adapter/harness; left in acps-config.toml and skipped at runtime",
                ignored.target, ignored.capability
            );
        }
    }
}

/// Step: testflight — optional real-prompt test. Decision uses the resolver
/// above; only `Run` actually executes the agent.
pub(super) fn run_testflight_step(flow: &mut InitFlow) -> Result<()> {
    let output_mode = flow.output_mode;
    let Some(decision) =
        resolve_testflight_decision(&flow.args, &flow.config, &flow.registry, &flow.secret_store)?
    else {
        return Ok(());
    };
    let home = &flow.home;
    let config = &flow.config;
    let registry = &flow.registry;
    let result = record_init_step(
        &flow.store,
        &flow.init_run,
        8,
        step_kind::TESTFLIGHT,
        || Ok(!matches!(decision, TestflightDecision::Run)),
        || {
            match &decision {
                TestflightDecision::Run => {
                    init_println!(output_mode, "---");
                    init_println!(output_mode, "running real-prompt agent testflight");
                    crate::cli::agent::run_init_testflight(
                        home,
                        config,
                        registry,
                        output_mode.is_text(),
                    )?;
                }
                TestflightDecision::SkipExplicit => {
                    init_println!(output_mode, "testflight: skipped (--skip-testflight)");
                }
                TestflightDecision::SkipNonInteractive => {
                    init_println!(
                        output_mode,
                        "testflight: skipped (non-interactive run; pass --testflight to opt in)"
                    );
                }
                TestflightDecision::SkipDeclined => {
                    init_println!(output_mode, "testflight: skipped (declined at prompt)");
                }
                TestflightDecision::SkipUnsupported => {
                    init_println!(
                        output_mode,
                        "testflight: skipped (agent does not support headless testflight)"
                    );
                }
                TestflightDecision::SkipCredentialPending {
                    provider_id,
                    api_key_ref,
                } => {
                    init_println!(
                        output_mode,
                        "testflight: skipped (provider `{provider_id}` credential `{api_key_ref}` is pending a managed push)"
                    );
                }
            }
            Ok(StepOutcome::with_payload(format!(
                r#"{{"decision":"{}"}}"#,
                decision.label()
            )))
        },
    );
    if let Err(error) = result {
        return finalize_with_error(&flow.store, &flow.init_run, error);
    }
    Ok(())
}

/// Resume-aware finalization. If a prior step in this run is still `pending`,
/// `running`, or `failed` (because the current invocation's flags skipped over
/// it), the aggregate run status must NOT settle to `succeeded`. We mark it
/// `failed` instead and surface a clear error so the operator knows to re-run
/// with the original flags.
pub(super) fn finalize_init_run(flow: &mut InitFlow) -> Result<()> {
    let prior_steps = flow.store.query_init_steps(&flow.init_run.id)?;
    let unsettled: Vec<&str> = prior_steps
        .iter()
        .filter(|s| {
            matches!(
                s.status.as_str(),
                INIT_STEP_PENDING | INIT_STEP_RUNNING | INIT_STEP_FAILED
            )
        })
        .map(|s| s.kind.as_str())
        .collect();
    if !unsettled.is_empty() {
        crate::runtime::init_runner::finalize_run(&flow.store, &flow.init_run.id, INIT_RUN_FAILED)?;
        return Err(StackError::InitRunCorrupted {
            reason: format!(
                "init run {} has unsettled steps {unsettled:?}; re-run with the original flags to drive them to completion",
                flow.init_run.id,
            ),
        });
    }
    if flow.output_mode.is_machine_handoff() {
        flow.key_handover.record(&flow.store, &flow.init_run.id)?;
        crate::runtime::init_runner::finalize_run(
            &flow.store,
            &flow.init_run.id,
            INIT_RUN_SUCCEEDED,
        )?;
        if flow.output_mode.is_handoff_json() {
            flow.key_handover
                .print_handoff_json("initialized", &flow.handoff_context)?;
        } else {
            flow.key_handover
                .emit_handoff_payload("initialized", &flow.handoff_context);
        }
    } else {
        // Finalize before printing so a state-store failure here surfaces as a
        // failed run (keys still reach the operator via the Drop guard) instead
        // of a success handover followed by a nonzero exit.
        crate::runtime::init_runner::finalize_run(
            &flow.store,
            &flow.init_run.id,
            INIT_RUN_SUCCEEDED,
        )?;
        flow.key_handover
            .print_and_record(&flow.store, &flow.init_run.id)?;
    }
    Ok(())
}
