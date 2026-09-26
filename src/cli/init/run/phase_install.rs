use super::*;
use crate::runtime::install::agent_installer::{
    HarnessInstall, InstalledArtifact, STEP_INSTALL, probe_binary_version,
};
use crate::runtime::install::install_ownership::{
    BinaryOwnership, ComponentRole, InstallComponent,
};

/// Step: agent_install. Installs the configured agent if requested.
pub(super) fn run_agent_install_step(flow: &mut InitFlow) -> Result<()> {
    let output_mode = flow.output_mode;
    let install_requested = should_install_agent(&flow.config, &flow.registry)?;
    let install_step_needs_resume =
        step_needs_resume(&flow.prior_init_steps, step_kind::AGENT_INSTALL);
    if !(install_requested || install_step_needs_resume) {
        return Ok(());
    }
    let install_interactive = prompts_enabled(&flow.args);
    // A bare resume replays no interactive answer, so the choice the prior attempt recorded is
    // what its binaries are held to.
    let recorded = match flow.prior_init_steps.iter().find(|step| {
        step.kind == step_kind::AGENT_INSTALL
            && matches!(
                step.status.as_str(),
                INIT_STEP_SUCCEEDED | INIT_STEP_SKIPPED
            )
    }) {
        Some(step) => match recorded_agent_install(&step.payload_json) {
            Ok(recorded) => recorded,
            Err(error) => return finalize_with_error(&flow.store, &flow.init_run, error),
        },
        None => RecordedAgentInstall::default(),
    };
    let verify_choice = flow.args.existing_agent.or(recorded.existing_agent);
    let verify_config = flow.config.clone();
    let verify_workspace_root = PathBuf::from(flow.config.workspace.root.clone());
    let verify_local_bin_dir = local_bin_dir(&flow.home);
    let verify_home = flow.home.clone();
    let store = &flow.store;
    let init_run = &flow.init_run;
    let home = &flow.home;
    let config_path = &flow.config_path;
    let config = &mut flow.config;
    let registry = &flow.registry;
    let args = &flow.args;
    let install_outcome = &mut flow.install_outcome;
    let result = record_init_step(
        store,
        init_run,
        2,
        step_kind::AGENT_INSTALL,
        || {
            // The prior install still runs, but a pin changed since then names another CLI.
            if recorded.harness_version != verify_config.agent.harness_version {
                return Ok(false);
            }
            installer_postcondition_holds(
                &verify_config,
                registry,
                store,
                verify_choice,
                &verify_workspace_root,
                &verify_local_bin_dir,
                &verify_home,
            )
        },
        || {
            if !args.skip_workspace_init() {
                crate::runtime::workspace_sources::workspace_init::prepare_workspace_base_dirs(
                    &config.workspace,
                )?;
            }
            // Run ahead of the installer's own call so the operator sees the Node outcome;
            // the installer then takes the offline fast path.
            prepare_node_runtime(home, output_mode);
            let plan = plan_existing_agent(
                args,
                install_interactive,
                output_mode,
                config,
                config_path,
                registry,
                store,
                &init_run.id,
                home,
            )?;
            let config = &*config;
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
                            install_configured_agent(home, config, registry, store, &plan.harness)
                        })
                    } else {
                        init_println!(output_mode, "progress: {message}");
                        install_configured_agent(home, config, registry, store, &plan.harness)
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
                "harness_version": config.agent.harness_version,
                "existing_agent": plan.existing_harness.as_ref().map(|existing| serde_json::json!({
                    "path": existing.path.display().to_string(),
                    "choice": existing.choice.as_config_value(),
                })),
                "replaced_adapter": plan.replaced_adapter.as_ref().map(|path| path.display().to_string()),
            });
            Ok(StepOutcome::with_payload(payload.to_string()))
        },
    );
    if let Err(error) = result {
        return finalize_with_error(&flow.store, &flow.init_run, error);
    }
    Ok(())
}

/// What the install step does with agent binaries acp-stack did not install.
struct ExistingAgentPlan {
    harness: HarnessInstall,
    existing_harness: Option<ExistingHarness>,
    /// A foreign ACP adapter, which is always replaced with its latest release.
    replaced_adapter: Option<PathBuf>,
}

/// An agent CLI acp-stack did not install, and the choice applied to it.
struct ExistingHarness {
    path: PathBuf,
    choice: ExistingAgentArg,
}

/// Detect agent binaries acp-stack did not install and settle what the install does with them.
/// A foreign agent CLI takes the operator's choice: `--existing-agent`, else the replacement
/// `--agent-version` or a configured pin implies, else the terminal prompt, else replacement with
/// the latest release.
#[allow(clippy::too_many_arguments)]
fn plan_existing_agent(
    args: &InitArgs,
    interactive: bool,
    output_mode: InitOutputMode,
    config: &mut Config,
    config_path: &Path,
    registry: &RegistryCatalog,
    store: &StateStore,
    run_id: &str,
    home: &Path,
) -> Result<ExistingAgentPlan> {
    let mut plan = ExistingAgentPlan {
        harness: HarnessInstall::Install,
        existing_harness: None,
        replaced_adapter: None,
    };
    let version_pinnable = agent_version_pin_blocker(config, registry)?.is_none();
    for (component, ownership) in detect_existing_agent_binaries(config, registry, store, home)? {
        // A binary kept on an earlier run still counts as not acp-stack's own.
        let (BinaryOwnership::Foreign(artifact) | BinaryOwnership::Kept(artifact)) = ownership
        else {
            continue;
        };
        if component.role == ComponentRole::Adapter {
            init_println!(
                output_mode,
                "progress: replacing the ACP adapter at {}, which acp-stack did not install, with its latest release",
                artifact.path.display()
            );
            plan.replaced_adapter = Some(artifact.path);
            continue;
        }
        let choice = match args.existing_agent {
            Some(choice) => choice,
            // A configured pin implies what `--agent-version` does, so the default never drops it.
            None if args.agent_version.is_some() || config.agent.harness_version.is_some() => {
                ExistingAgentArg::ReplaceVersion
            }
            None => match prompt_existing_agent_choice(
                interactive,
                version_pinnable,
                config,
                &component,
                &artifact,
                home,
            )? {
                Some(choice) => {
                    record_prompted_arg(store, run_id, "existing_agent", choice.as_config_value())?;
                    choice
                }
                None => ExistingAgentArg::ReplaceLatest,
            },
        };
        match choice {
            ExistingAgentArg::UseExisting => {
                init_println!(
                    output_mode,
                    "progress: keeping the agent CLI at {}, which acp-stack did not install",
                    artifact.path.display()
                );
                plan.harness = HarnessInstall::Keep(artifact.path.clone());
            }
            ExistingAgentArg::ReplaceLatest => {
                // Latest means latest: a pin a kept config still carries would install
                // something else.
                if config.agent.harness_version.take().is_some() {
                    persist_config(config, config_path)?;
                }
                init_println!(
                    output_mode,
                    "progress: replacing the agent CLI at {}, which acp-stack did not install, with its latest release",
                    artifact.path.display()
                );
            }
            ExistingAgentArg::ReplaceVersion => {
                let configured = args
                    .agent_version
                    .clone()
                    .or_else(|| config.agent.harness_version.clone());
                let version = match configured {
                    Some(version) => version,
                    None => {
                        let version = prompt_agent_version(interactive)?;
                        ensure_agent_version_installable(config, registry, &version)?;
                        record_prompted_arg(store, run_id, "agent_version", &version)?;
                        config.agent.harness_version = Some(version.clone());
                        persist_config(config, config_path)?;
                        version
                    }
                };
                init_println!(
                    output_mode,
                    "progress: replacing the agent CLI at {}, which acp-stack did not install, with version {version}",
                    artifact.path.display()
                );
            }
        }
        plan.existing_harness = Some(ExistingHarness {
            path: artifact.path,
            choice,
        });
    }
    Ok(plan)
}

/// Record a prompt's answer on the run before anything installs: a resume replays no prompt, only
/// the recorded arguments.
fn record_prompted_arg(store: &StateStore, run_id: &str, key: &str, value: &str) -> Result<()> {
    let patch = serde_json::Map::from_iter([(key.to_owned(), serde_json::Value::from(value))]);
    store.merge_init_run_args(run_id, &patch)
}

/// Ask what to do with a foreign agent CLI; `None` when nothing was asked. Only a terminal asks: a
/// hosted client declares the choice in its start request, and the prompt kind is outside the
/// streamed set.
fn prompt_existing_agent_choice(
    interactive: bool,
    version_pinnable: bool,
    config: &Config,
    component: &InstallComponent,
    artifact: &InstalledArtifact,
    home: &Path,
) -> Result<Option<ExistingAgentArg>> {
    if !interactive || prompt::hosted_driver_active() {
        return Ok(None);
    }
    // The probe executes the binary, so a native agent's CLI, which is also its ACP entry point,
    // must first pass the entry's integrity pin.
    let pin_holds = component.step != STEP_INSTALL
        || config
            .agent
            .expected_sha256
            .as_deref()
            .is_none_or(|expected| expected == artifact.sha256);
    let workspace_root = Path::new(&config.workspace.root);
    let version = pin_holds
        .then(|| {
            probe_binary_version(
                &artifact.path,
                workspace_root,
                &[&local_bin_dir(home)],
                home,
            )
        })
        .flatten()
        .unwrap_or_else(|| "version unknown".to_owned());
    let prompt_text = format!(
        "`{}` at {} ({version}) was not installed by acp-stack",
        component.command,
        artifact.path.display()
    );
    prompt::select(
        prompt::HostedPromptKind::ExistingAgent,
        interactive,
        &prompt_text,
        &existing_agent_choice_items(version_pinnable),
    )
}

/// The existing-install menu. A CLI no install lane can pin is never offered a specific version.
pub(super) fn existing_agent_choice_items(
    version_pinnable: bool,
) -> Vec<prompt::PromptItem<ExistingAgentArg>> {
    let item = |choice: ExistingAgentArg, label: &str| {
        prompt::item(choice, choice.as_config_value(), label, "")
    };
    let mut items = vec![item(
        ExistingAgentArg::ReplaceLatest,
        "Replace it with the latest release",
    )];
    if version_pinnable {
        items.push(item(
            ExistingAgentArg::ReplaceVersion,
            "Replace it with a specific version",
        ));
    }
    items.push(item(
        ExistingAgentArg::UseExisting,
        "Use the existing install",
    ));
    items
}

/// Ask for the version `replace-version` installs when neither the flag nor the start request
/// gave one; a run that cannot ask fails instead of guessing.
fn prompt_agent_version(interactive: bool) -> Result<String> {
    let version = if interactive && !prompt::hosted_driver_active() {
        prompt::text(
            prompt::HostedPromptKind::AgentVersion,
            interactive,
            "Agent CLI version to install",
            true,
        )?
    } else {
        None
    };
    let version = version.ok_or_else(|| StackError::InvalidParam {
        field: "--existing-agent",
        reason: "`replace-version` requires --agent-version".to_owned(),
    })?;
    let version = version.trim().to_owned();
    validate_agent_version_value("--agent-version", &version)?;
    Ok(version)
}

fn persist_config(config: &mut Config, config_path: &Path) -> Result<()> {
    let canonical = config.to_canonical_toml()?;
    *config = config::load_config_from_str(&canonical)?;
    atomic_write_owner_only(config_path, canonical.as_bytes())
}

/// Install or confirm the managed Node.js runtime before any agent installer runs. A failure is
/// reported and init continues: a recipe that needs Node fails its own prerequisite check.
fn prepare_node_runtime(home: &Path, output_mode: InitOutputMode) {
    use crate::runtime::node_runtime::{NODE_MAJOR, NodeRuntimeStatus, ensure, is_managed_host};
    // Unmanaged hosts are left to the installer's own call, which logs the fallback.
    if !is_managed_host() {
        return;
    }
    init_println!(
        output_mode,
        "progress: preparing Node.js {NODE_MAJOR} runtime"
    );
    match ensure(home) {
        Ok(NodeRuntimeStatus::Ready { version }) => {
            init_println!(output_mode, "progress: Node.js {version} ready");
        }
        Ok(NodeRuntimeStatus::Unsupported { .. } | NodeRuntimeStatus::NotReady) => {}
        Err(error) => {
            init_println!(
                output_mode,
                "warning: managed Node.js is unavailable: {error}"
            );
        }
    }
}

/// Step: native_config_import. Applies the reviewed native global config after installation.
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

/// Step: agent_skills_install. Installs selected Agent Skills before the first launch.
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
