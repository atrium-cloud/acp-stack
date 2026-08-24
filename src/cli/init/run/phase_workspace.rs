use super::*;

/// Step: workspace_materialize — clone repos and download/extract data sources into the workspace.
pub(super) fn run_workspace_materialize_step(flow: &mut InitFlow) -> Result<()> {
    let output_mode = flow.output_mode;
    let workspace_for_verify = flow.config.workspace.clone();
    if flow.args.skip_workspace_init()
        && !step_needs_resume(&flow.prior_init_steps, step_kind::WORKSPACE_MATERIALIZE)
    {
        init_println!(output_mode, "workspace: skipped (--skip-workspace-init)");
        prompt::emit_state_signal(|| {
            applicability(
                InitCategory::Workspace,
                false,
                ApplicabilitySource::Args,
                "--skip-workspace-init",
            )
        });
        return Ok(());
    }
    init_println!(output_mode, "progress: materializing workspace sources");
    let log_paths = crate::runtime::workspace_sources::workspace_init::WorkspaceLogPaths::for_run(
        &crate::runtime::workspace_sources::workspace_init::default_workspace_init_log_base(
            &flow.home,
        ),
        &flow.init_run.id,
    );
    create_dir_owner_only(&log_paths.run_dir)?;
    // Pre-computed so a mid-clone failure still records the log dir on the init_steps row.
    let log_dir_str = log_paths.run_dir.display().to_string();
    let config = &flow.config;
    let secret_store = &flow.secret_store;
    let materialize_report = &mut flow.materialize_report;
    let result = record_init_step_with_default_log_dir(
        &flow.store,
        &flow.init_run,
        3,
        step_kind::WORKSPACE_MATERIALIZE,
        Some(&log_dir_str),
        || Ok(workspace_postcondition_holds(&workspace_for_verify)),
        || {
            let report = crate::runtime::workspace_sources::workspace_init::materialize_workspace(
                &config.workspace,
                secret_store,
                Some(&log_paths),
            )?;
            let step_log_dir = report.log_dir.as_ref().map(|p| p.display().to_string());
            *materialize_report = Some(report);
            Ok(StepOutcome {
                log_dir: step_log_dir,
                payload_json: "{}".to_owned(),
                background: false,
            })
        },
    );
    if let Err(error) = result {
        return finalize_with_error(&flow.store, &flow.init_run, error);
    }
    Ok(())
}

/// Step: deps_apply — run declared dependency install actions before the agent is launched for provider/model discovery.
pub(super) fn run_deps_apply_step(flow: &mut InitFlow) -> Result<()> {
    let output_mode = flow.output_mode;
    let deps_candidates = pending_candidates(&flow.config, None);
    if deps_candidates.is_empty() {
        // Re-asserted rather than trusted from the earlier derivation: the install and workspace
        // steps in between can satisfy the last pending action.
        prompt::emit_state_signal(|| {
            applicability(
                InitCategory::Deps,
                false,
                ApplicabilitySource::Args,
                "no pending dependency install actions",
            )
        });
    }
    // Probed once and reused, so the prompt cannot promise a mode the apply won't use.
    let deps_escalation = if pending_system_candidates(&flow.config, None).is_empty() {
        PrivilegeEscalation::NotNeeded
    } else {
        probe_privilege_escalation()
    };
    // finalize_with_error so a confirmation error marks the run terminal instead of leaving it pending.
    let deps_apply_requested = match should_apply_deps_for_init(
        &flow.args,
        &deps_candidates,
        prompts_enabled(&flow.args),
        &deps_escalation,
        &flow.config.workspace.default_shell,
        &mut |line| init_println!(output_mode, "{line}"),
    ) {
        Ok(requested) => requested,
        Err(error) => return finalize_with_error(&flow.store, &flow.init_run, error),
    };
    if !(deps_apply_requested || step_needs_resume(&flow.prior_init_steps, step_kind::DEPS_APPLY)) {
        return Ok(());
    }
    init_println!(output_mode, "progress: applying dependencies");
    let store = &flow.store;
    let config = &flow.config;
    let init_run_id = flow.init_run.id.clone();
    let deps_apply_async = flow.args.deps_apply_async;
    let background_apply_run_id: std::cell::RefCell<Option<String>> = std::cell::RefCell::new(None);
    let result = record_init_step(
        store,
        &flow.init_run,
        10,
        step_kind::DEPS_APPLY,
        || Ok(pending_candidates(config, None).is_empty()),
        || {
            if deps_apply_async {
                let (outcome, apply_run_id) = launch_background_deps_apply(
                    store,
                    config,
                    &init_run_id,
                    &deps_escalation,
                    output_mode,
                )?;
                *background_apply_run_id.borrow_mut() = Some(apply_run_id);
                return Ok(outcome);
            }
            let report = apply_dependencies_tracked(
                config,
                store,
                TrackedApplyRun::Claim {
                    origin: DEPS_APPLY_ORIGIN_INIT,
                    init_run_id: Some(&init_run_id),
                },
                None,
                &config.workspace.default_shell,
                &deps_escalation,
                |current, total, name| {
                    init_println!(
                        output_mode,
                        "progress: applying dependency {current}/{total}: {name}"
                    );
                    Ok(())
                },
            )?;
            // Action failures fail init; privilege skips do not, because an un-escalatable host is a
            // host property. A later resume re-runs the skipped deps: the step verifier stays false for them.
            let mut failures = Vec::new();
            let mut skipped_privileged = Vec::new();
            let mut skipped_privilege_uid: Option<u32> = None;
            for entry in &report.results {
                match &entry.outcome {
                    DepApplyOutcome::Failed { exit_code, .. } => {
                        let code = exit_code
                            .map(|value| value.to_string())
                            .unwrap_or_else(|| "?".to_owned());
                        failures.push(format!("{} failed (exit={code})", entry.name));
                    }
                    DepApplyOutcome::PrivilegeRequired { uid } => {
                        skipped_privileged.push(entry.name.clone());
                        skipped_privilege_uid = Some(*uid);
                    }
                    DepApplyOutcome::Installed | DepApplyOutcome::AlreadyPresent => {}
                }
            }
            if !failures.is_empty() {
                if !skipped_privileged.is_empty() {
                    failures.push(format!(
                        "{} action(s) skipped on privilege",
                        skipped_privileged.len(),
                    ));
                }
                return Err(StackError::DepsApplyFailed {
                    summary: failures.join("; "),
                    apply_run_id: report.apply_run_id.clone(),
                    retry_command: "acps init --resume --deps-apply --deps-apply-yes",
                });
            }
            if !skipped_privileged.is_empty() {
                init_println!(
                    output_mode,
                    "warning: {count} dependency install action(s) need root and were skipped (uid={uid}, no passwordless sudo)",
                    count = skipped_privileged.len(),
                    // The outcome carries the real euid; `deps_escalation.uid()` reports 0 under `NotNeeded`.
                    uid = skipped_privilege_uid.unwrap_or_default(),
                );
                for candidate in pending_system_candidates(config, None) {
                    init_println!(
                        output_mode,
                        "  - {name}: {manual}",
                        name = candidate.name,
                        manual =
                            manual_privileged_command(&config.workspace.default_shell, &candidate,),
                    );
                }
                init_println!(
                    output_mode,
                    "recorded as privilege_required under `acps installer history --agent deps_apply` (apply_run_id={})",
                    report.apply_run_id,
                );
                init_println!(
                    output_mode,
                    "after installing them manually (or granting passwordless sudo), resume with: acps init --resume --deps-apply --deps-apply-yes"
                );
            }
            Ok(StepOutcome::with_payload(format!(
                r#"{{"apply_run_id":"{}","applied":{},"skipped_privileged":{}}}"#,
                report.apply_run_id,
                report.results.len(),
                skipped_privileged.len(),
            )))
        },
    );
    // The background worker outlives init, so its id must reach both handoff frames before any
    // early return, including when the worker spawned but this step's own record write then failed.
    let background_run_id = background_apply_run_id.into_inner();
    flow.handoff_context.deps_apply_run_id = background_run_id.clone();
    if let Some(context) = flow.key_handover.failure_context.as_mut() {
        context.deps_apply_run_id = background_run_id;
    }
    if let Err(error) = result {
        return finalize_with_error(&flow.store, &flow.init_run, error);
    }
    Ok(())
}

/// `--deps-apply-async` branch of the deps_apply step: claim the single-flight `deps_apply_runs` row and spawn the detached worker so init proceeds while the install runs.
pub(super) fn launch_background_deps_apply(
    store: &StateStore,
    config: &Config,
    init_run_id: &str,
    escalation: &PrivilegeEscalation,
    output_mode: InitOutputMode,
) -> Result<(StepOutcome, String)> {
    use crate::cli::deps_apply_worker::spawn_detached_worker;
    use crate::runtime::dependencies::deps_apply::deps_run_liveness;
    use crate::runtime::process_runner::current_boot_id;
    use crate::state::{DEPS_APPLY_RUN_FAILED, DepsApplyRunFinish};

    let is_live = deps_run_liveness();
    store.reconcile_stale_deps_apply_runs(&is_live)?;
    if let Some(running) = store.running_deps_apply_run()? {
        // Adopt only this init run's own background install: recording the step against a foreign
        // live apply would silently skip this run's own declared deps.
        if running.origin == DEPS_APPLY_ORIGIN_INIT_BACKGROUND
            && running.init_run_id.as_deref() == Some(init_run_id)
        {
            init_println!(
                output_mode,
                "progress: dependency install already running in background (apply_run_id={})",
                running.id,
            );
            let outcome = StepOutcome::background_with_payload(format!(
                r#"{{"apply_run_id":"{}","background":true,"adopted":true}}"#,
                running.id,
            ));
            return Ok((outcome, running.id));
        }
        return Err(StackError::DepsApplyInFlight {
            apply_run_id: running.id,
        });
    }

    let apply_run_id = crate::state::next_deps_apply_run_id();
    let pending = pending_candidates(config, None).len();
    store.claim_deps_apply_run(
        NewDepsApplyRun {
            id: &apply_run_id,
            origin: DEPS_APPLY_ORIGIN_INIT_BACKGROUND,
            init_run_id: Some(init_run_id),
            feature: None,
            pid: None,
            boot_id: current_boot_id().as_deref(),
            total: candidates_for(config, None).len() as i64,
        },
        &is_live,
    )?;
    // From here the row is `running` with a null pid, so every fallible step before the worker
    // exists must settle the row on failure or the single-flight slot wedges until the grace expires.
    let spawn_result = (|| -> Result<(u32, std::path::PathBuf)> {
        let log_dir = crate::state::default_installer_log_base(&home_dir()?)
            .join("deps_apply")
            .join(&apply_run_id);
        create_dir_owner_only(&log_dir)?;
        let config_path = crate::config::default_config_path()?;
        let pid = spawn_detached_worker(
            &config_path,
            store.path(),
            &apply_run_id,
            None,
            escalation,
            &log_dir,
        )?;
        Ok((pid, log_dir))
    })();
    let (pid, log_dir) = match spawn_result {
        Ok(value) => value,
        Err(error) => {
            // Nothing durable started: settle the claimed row so it cannot wedge the single-flight slot.
            let detail = error.to_string();
            if let Err(finish_error) = store.finish_deps_apply_run(
                &apply_run_id,
                DepsApplyRunFinish {
                    status: DEPS_APPLY_RUN_FAILED,
                    completed: 0,
                    installed: 0,
                    already_present: 0,
                    privilege_required: 0,
                    failed: 0,
                    error_code: Some("deps.apply_failed"),
                    error_detail: Some(&detail),
                    payload_json: "{}",
                },
            ) {
                tracing::warn!(error = %finish_error, apply_run_id, "deps apply: failed to settle run row after background start failure");
            }
            return Err(StackError::DepsApplyFailed {
                summary: format!("failed to start background dependency apply: {detail}"),
                apply_run_id,
                retry_command: "acps init --resume --deps-apply --deps-apply-yes --deps-apply-async",
            });
        }
    };
    // The worker self-stamps on startup too, so a failed stamp here only widens the null-pid grace window.
    if let Err(error) = store.stamp_deps_apply_child(
        &apply_run_id,
        i64::from(pid),
        current_boot_id().as_deref(),
        log_dir.to_str(),
    ) {
        tracing::warn!(%error, apply_run_id, "deps apply: failed to stamp background child pid");
    }
    init_println!(
        output_mode,
        "progress: dependency install started in background (apply_run_id={apply_run_id}, pid={pid})",
    );
    init_println!(
        output_mode,
        "poll it with: GET /v1/deps/apply/runs/{apply_run_id}",
    );
    let mut outcome = StepOutcome::background_with_payload(format!(
        r#"{{"apply_run_id":"{apply_run_id}","background":true,"pid":{pid},"pending":{pending}}}"#,
    ));
    outcome.log_dir = log_dir.to_str().map(str::to_owned);
    Ok((outcome, apply_run_id))
}

/// Step: capability_probe — handshake-only spawn to capture the agent's ACP `initialize` advertisement. A failed probe never fails init.
pub(super) fn run_capability_probe_step(flow: &mut InitFlow) -> Result<()> {
    let output_mode = flow.output_mode;
    init_println!(output_mode, "progress: probing agent capabilities");
    let store = &flow.store;
    let home = &flow.home;
    let config = &flow.config;
    let secret_store = &flow.secret_store;
    let probed_capabilities = &mut flow.probed_capabilities;
    let ignored_features = &mut flow.ignored_features;
    let result = record_init_step(
        store,
        &flow.init_run,
        12,
        step_kind::CAPABILITY_PROBE,
        // Always re-probe on resume: a reinstall between runs can change the advertisement.
        || Ok(false),
        || {
            let outcome = probe_agent_capabilities_for_init(home, config);
            // The handshake is the only authority on MCP; it overrides the provisional verdict.
            prompt::emit_state_signal(|| mcp_applicability_from_probe(&outcome));
            match outcome {
                CapabilityProbeOutcome::Probed(capabilities) => {
                    store.upsert_agent_capabilities(&config.agent.id, &capabilities.to_json()?)?;
                    // Best-effort: an unresolvable MCP declaration surfaces at session time, not here.
                    match crate::runtime::agent::mcp::resolve_mcp_servers(&config.mcp, secret_store)
                        .and_then(|declared| capabilities.ignored_mcp_features(declared))
                    {
                        Ok(ignored) => *ignored_features = ignored,
                        Err(error) => {
                            tracing::warn!(
                                %error,
                                "skipping MCP capability assessment: declared servers did not resolve"
                            );
                        }
                    }
                    if let Some(settlement) =
                        mcp_settlement_from_probe(&capabilities, config, ignored_features)
                    {
                        prompt::emit_state_signal(|| settlement);
                    }
                    let payload = serde_json::json!({
                        "probe_status": "ok",
                        "protocol_version": capabilities.protocol_version,
                        "agent_name": capabilities.agent_name,
                        "ignored": &*ignored_features,
                    });
                    *probed_capabilities = Some(capabilities);
                    Ok(StepOutcome::with_payload(payload.to_string()))
                }
                CapabilityProbeOutcome::Unavailable { reason } => {
                    let payload = serde_json::json!({
                        "probe_status": "unavailable",
                        "reason": reason,
                    });
                    Ok(StepOutcome::with_payload(payload.to_string()))
                }
            }
        },
    );
    if let Err(error) = result {
        return finalize_with_error(&flow.store, &flow.init_run, error);
    }
    flow.handoff_context.ignored_features = flow.ignored_features.clone();
    if let Some(context) = flow.key_handover.failure_context.as_mut() {
        context.ignored_features = flow.ignored_features.clone();
    }
    Ok(())
}

/// Step: mcp_configure — interactive MCP prompting, which must run after the probe because MCP support is only knowable from the installed agent's advertisement.
pub(super) fn run_mcp_configure_step(flow: &mut InitFlow) -> Result<()> {
    let output_mode = flow.output_mode;
    let mcp_prompting_active =
        mcp_prompting_enabled(&flow.args, flow.creating_config, &flow.config);
    // `step_needs_resume`: a resumed run must still settle a prior failed row, which the body
    // then does without prompts because `mcp_prompting_active` is false on every resume path.
    if !(mcp_prompting_active
        || step_needs_resume(&flow.prior_init_steps, step_kind::MCP_CONFIGURE))
    {
        return Ok(());
    }
    let config_path = &flow.config_path;
    let probed_capabilities = &flow.probed_capabilities;
    let args = &mut flow.args;
    let config = &mut flow.config;
    let secret_store = &mut flow.secret_store;
    let result = record_init_step(
        &flow.store,
        &flow.init_run,
        13,
        step_kind::MCP_CONFIGURE,
        // Interactively-collected answers cannot be replayed, so a prior succeeded row skips.
        || Ok(true),
        || {
            let Some(capabilities) = probed_capabilities.as_ref() else {
                init_println!(output_mode, "mcp: skipped (agent capabilities unavailable)");
                prompt::emit_state_signal(|| {
                    applicability(
                        InitCategory::Mcp,
                        false,
                        ApplicabilitySource::Probe,
                        "agent capabilities unavailable",
                    )
                });
                return Ok(StepOutcome::with_payload(
                    r#"{"prompted":false,"reason":"probe_unavailable"}"#,
                ));
            };
            if !capabilities.advertises_mcp_support() {
                init_println!(
                    output_mode,
                    "mcp: skipped (agent does not advertise MCP support)"
                );
                prompt::emit_state_signal(|| {
                    applicability(
                        InitCategory::Mcp,
                        false,
                        ApplicabilitySource::Probe,
                        "agent does not advertise MCP support",
                    )
                });
                return Ok(StepOutcome::with_payload(
                    r#"{"prompted":false,"reason":"no_mcp_transports"}"#,
                ));
            }
            let offer_http = capabilities.supports_mcp_capability("http");
            let mut transports_offered = vec!["stdio"];
            if offer_http {
                transports_offered.push("http");
            }
            // The gate must stay outside the call: `prompt::confirm` consults the hosted driver
            // before its `interactive` argument, so an unguarded call re-drives the wizard on resume.
            if mcp_prompting_active
                && prompt::confirm(
                    prompt::HostedPromptKind::McpAdd,
                    mcp_prompting_active,
                    "Add MCP servers?",
                    false,
                )?
            {
                prompt_mcp_servers(mcp_prompting_active, args, offer_http)?;
            }
            let new_servers =
                mcp_servers_from_prompted(&args.prompt_mcp_stdio, &args.prompt_mcp_http)?;
            let added = merge_prompted_mcp_servers(&mut config.mcp.servers, new_servers)?;
            if !added.is_empty() {
                let canonical = config.to_canonical_toml()?;
                // Later steps read the in-memory config, not the file, so the reassignment is what makes them see the servers.
                *config = config::load_config_from_str(&canonical)?;
                atomic_write_owner_only(config_path, canonical.as_bytes())?;
                let stored =
                    collect_mcp_secret_refs_for_init(mcp_prompting_active, config, secret_store)?;
                if !stored.is_empty() {
                    init_println!(output_mode, "declared secrets: set ({})", stored.join(", "));
                }
                prompt::emit_state_signal(|| InitStateSignal::CategorySettled {
                    category: InitCategory::Mcp,
                    value: Some(added.join(", ")),
                });
            }
            let payload = serde_json::json!({
                "prompted": mcp_prompting_active,
                "added": added,
                "transports_offered": transports_offered,
            });
            Ok(StepOutcome::with_payload(payload.to_string()))
        },
    );
    if let Err(error) = result {
        return finalize_with_error(&flow.store, &flow.init_run, error);
    }
    Ok(())
}
