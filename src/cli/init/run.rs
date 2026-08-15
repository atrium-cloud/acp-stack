use super::*;

macro_rules! init_println {
    ($output:expr, $($arg:tt)*) => {
        if $output.is_text() {
            println!($($arg)*);
        } else if $output.is_hosted() {
            prompt::emit_progress(format!($($arg)*));
        }
    };
}

/// Whether init should drive interactive prompts: a real TTY and no
/// prompt-suppressing automation flags. The single source of truth for the
/// gate, so every prompt site honors the same contract.
fn prompts_enabled_for(args: &InitArgs, stdin_is_terminal: bool) -> bool {
    (stdin_is_terminal || prompt::hosted_driver_active())
        && !args.non_interactive
        && !args.handoff_json
}

pub(super) fn prompts_enabled(args: &InitArgs) -> bool {
    prompts_enabled_for(args, io::stdin().is_terminal())
}

/// Whether the post-probe `mcp_configure` step drives its own prompts. Hosted
/// runs stream them like any other init prompt, so declaring MCP servers in the
/// start request is what keeps a hosted session out of the wizard: those
/// servers are already in `config.mcp.servers` by the time this is evaluated.
fn mcp_prompting_enabled(args: &InitArgs, creating_config: bool, config: &Config) -> bool {
    creating_config && !args.resume && prompts_enabled(args) && config.mcp.servers.is_empty()
}

fn config_import_source_for_init(
    args: &InitArgs,
) -> Result<Option<cli_config::ConfigImportSource<'_>>> {
    match (
        args.from_file.as_deref(),
        args.from_toml.as_deref(),
        args.from_base64.as_deref(),
    ) {
        (None, None, None) => Ok(None),
        (Some(path), None, None) => Ok(Some(cli_config::ConfigImportSource::Path(path))),
        (None, Some(raw_toml), None) => Ok(Some(cli_config::ConfigImportSource::Toml(raw_toml))),
        (None, None, Some(encoded)) => Ok(Some(cli_config::ConfigImportSource::Base64(encoded))),
        _ => Err(StackError::InvalidParam {
            field: "--from-file",
            reason: "choose only one of --from-file, --from-toml, or --from-base64".to_owned(),
        }),
    }
}

fn prompt_config_source_if_needed(
    args: &mut InitArgs,
    config_path: &Path,
    state_path: &Path,
) -> Result<()> {
    if args.config_import_source_label().is_some() || args.resume || args.fresh {
        return Ok(());
    }
    let interactive = prompts_enabled(args);
    if !interactive {
        return Ok(());
    }
    let resumable = config_path.exists() && resumable_init_exists(state_path)?;
    if config_path.exists() && !resumable {
        return Ok(());
    }

    #[derive(Clone, PartialEq, Eq)]
    enum ConfigSourceChoice {
        Resume,
        ContinueExisting,
        ImportFile,
        PasteBase64,
        StartFresh,
    }

    let mut items = Vec::new();
    if resumable {
        items.push(prompt::item(
            ConfigSourceChoice::Resume,
            "resume",
            "Resume interrupted init",
            "",
        ));
        items.push(prompt::item(
            ConfigSourceChoice::ContinueExisting,
            "continue_existing",
            "Continue with existing config",
            "",
        ));
    } else {
        items.push(prompt::item(
            ConfigSourceChoice::ImportFile,
            "import_file",
            "Import acps-config.toml path",
            "",
        ));
        items.push(prompt::item(
            ConfigSourceChoice::PasteBase64,
            "paste_base64",
            "Paste base64 acps-config.toml",
            "",
        ));
        items.push(prompt::item(
            ConfigSourceChoice::StartFresh,
            "start_fresh",
            "Start fresh",
            "",
        ));
    }

    match prompt::select(
        prompt::HostedPromptKind::ConfigSource,
        interactive,
        "Config source",
        &items,
    )? {
        Some(ConfigSourceChoice::Resume) => {
            args.resume = true;
        }
        Some(ConfigSourceChoice::ContinueExisting | ConfigSourceChoice::StartFresh) | None => {}
        Some(ConfigSourceChoice::ImportFile) => {
            let Some(path) = prompt::text(
                prompt::HostedPromptKind::ConfigSourcePath,
                interactive,
                "acps-config.toml path",
                true,
            )?
            else {
                return Ok(());
            };
            args.from_file = Some(PathBuf::from(path.trim()));
        }
        Some(ConfigSourceChoice::PasteBase64) => {
            let Some(encoded) = prompt::text(
                prompt::HostedPromptKind::ConfigSourceBase64,
                interactive,
                "base64 acps-config.toml",
                true,
            )?
            else {
                return Ok(());
            };
            args.from_base64 = Some(encoded.trim().to_owned());
        }
    }
    Ok(())
}

fn resumable_init_exists(state_path: &Path) -> Result<bool> {
    if !state_path.exists() {
        return Ok(false);
    }
    let store = StateStore::open(state_path)?;
    store.migrate()?;
    Ok(crate::runtime::init_runner::find_resumable_run(&store)?.is_some())
}

fn import_config_for_init(
    args: &InitArgs,
    config_path: &Path,
    output_mode: InitOutputMode,
) -> Result<bool> {
    let Some(source) = config_import_source_for_init(args)? else {
        return Ok(false);
    };
    if config_path.exists() {
        return Err(StackError::ConfigExists {
            path: config_path.to_path_buf(),
        });
    }
    let payload = cli_config::load_config_import_payload(source)?;
    if output_mode.is_text() {
        cli_config::print_config_import_progress(true);
    }
    write_new_file_owner_only(config_path, payload.canonical.as_bytes())?;
    init_println!(output_mode, "imported config: {}", config_path.display());
    Ok(true)
}

fn agent_install_progress_message(attempt: u32) -> String {
    if attempt == 1 {
        "installing agent".to_owned()
    } else {
        format!("installing agent (attempt {attempt}/{MAX_INSTALL_ATTEMPTS})")
    }
}

/// MCP applicability as the live handshake reports it. An agent that
/// advertises no MCP transport cannot be given servers, and a probe that could
/// not run leaves no evidence MCP works — both are inapplicable, with the
/// probe's own wording as the reason.
pub(super) fn mcp_applicability_from_probe(outcome: &CapabilityProbeOutcome) -> InitStateSignal {
    match outcome {
        CapabilityProbeOutcome::Probed(capabilities) => applicability(
            InitCategory::Mcp,
            capabilities.advertises_mcp_support(),
            ApplicabilitySource::Probe,
            "agent does not advertise MCP support",
        ),
        CapabilityProbeOutcome::Unavailable { reason } => {
            applicability(InitCategory::Mcp, false, ApplicabilitySource::Probe, reason)
        }
    }
}

/// The MCP lane's outcome as the probe leaves it. A run that declared its
/// servers up front never reaches the prompt that would otherwise settle this
/// lane, and a resumed run re-probes every time, so this is where the servers
/// the agent will actually be handed become known. `ignored` is the partition
/// the probe already computed, which is what keeps this report and the
/// runtime's own transport filtering from drifting apart; a partition that
/// could not be computed arrives empty, leaving the list untrimmed rather than
/// unreported. `None` when nothing will be delivered — an agent that advertises
/// no MCP support has already been ruled inapplicable, and a run with no
/// declared servers leaves the lane for the prompt to settle.
pub(super) fn mcp_settlement_from_probe(
    capabilities: &crate::runtime::agent::acp_bridge::AgentCapabilitiesDto,
    config: &Config,
    ignored: &[crate::runtime::agent::acp_bridge::IgnoredFeature],
) -> Option<InitStateSignal> {
    if !capabilities.advertises_mcp_support() {
        return None;
    }
    let delivered = config
        .mcp
        .servers
        .iter()
        .map(|server| server.name())
        .filter(|name| {
            !ignored.iter().any(|feature| {
                feature.feature == crate::runtime::agent::acp_bridge::IGNORED_FEATURE_MCP_SERVER
                    && feature.target == *name
            })
        })
        .collect::<Vec<_>>();
    (!delivered.is_empty()).then(|| InitStateSignal::CategorySettled {
        category: InitCategory::Mcp,
        value: Some(delivered.join(", ")),
    })
}

/// Skills freshly written this run. A resumed run whose skills were all
/// already present installs nothing, which settles the category with no value
/// rather than an empty list.
fn installed_skill_names(reports: &[SkillInstallReport]) -> Option<String> {
    let names = reports
        .iter()
        .flat_map(|report| report.installed.iter().map(|entry| entry.name.as_str()))
        .collect::<Vec<_>>();
    (!names.is_empty()).then(|| names.join(", "))
}

/// A reason is carried only for an inapplicable verdict: "why is this lane
/// missing" is the question a client asks, and an applicable lane answers it
/// by simply appearing.
fn applicability(
    category: InitCategory,
    applicable: bool,
    source: ApplicabilitySource,
    reason: &str,
) -> InitStateSignal {
    InitStateSignal::CategoryApplicability {
        category,
        applicable,
        source,
        reason: (!applicable).then(|| reason.to_owned()),
    }
}

/// Everything knowable about the categories the instant the agent is settled:
/// the registry says which lanes this agent even has, the flags say which of
/// the remaining lanes this run will drive, and the config on disk says what
/// the harness lanes already hold. Returned rather than emitted so the
/// derivation is exercisable without a hosted driver.
///
/// MCP is deliberately absent from the applicability verdicts — the registry
/// has no MCP column and only the live capability probe can answer it, so MCP
/// stays provisionally applicable until the probe corrects it.
pub(super) fn agent_settlement_signals(
    config: &Config,
    registry: &RegistryCatalog,
    args: &InitArgs,
    native_config_pending: bool,
) -> Vec<InitStateSignal> {
    let mut signals = vec![InitStateSignal::CategorySettled {
        category: InitCategory::Agent,
        value: Some(config.agent.id.clone()),
    }];
    let registry_applicability = |category, applicable, reason: &str| {
        applicability(category, applicable, ApplicabilitySource::Registry, reason)
    };
    // A custom agent has no registry entry, so init drives none of the four
    // harness-configuration lanes for it: provider, model, and mode go through
    // the agent's own environment, and skills have no known install dir.
    let entry = registry.lookup(&config.agent.id);
    let custom_reason = "custom agents configure this outside acp-stack";
    signals.push(registry_applicability(
        InitCategory::Provider,
        entry.is_some_and(|entry| entry.set_provider),
        entry.map_or(custom_reason, |_| "agent does not take a provider"),
    ));
    signals.push(registry_applicability(
        InitCategory::Model,
        entry.is_some_and(|entry| entry.set_model),
        entry.map_or(custom_reason, |_| "agent does not take a model"),
    ));
    signals.push(registry_applicability(
        InitCategory::Mode,
        entry.is_some_and(|entry| entry.set_mode),
        entry.map_or(custom_reason, |_| "agent does not take a mode"),
    ));
    let skills_applicable = entry.is_some_and(|entry| {
        entry.supports_agent_skills && entry.agent_skills_install_dir.is_some()
    }) && !args.no_skills;
    signals.push(registry_applicability(
        InitCategory::Skills,
        skills_applicable,
        // The reason is wire surface for hosted clients, which have no flags:
        // a hosted request turns skills off by declaring none of them.
        if args.no_skills {
            "no skills were declared"
        } else {
            entry.map_or(custom_reason, |_| "agent does not support Agent Skills")
        },
    ));
    let args_applicability = |category, applicable, reason: &str| {
        applicability(category, applicable, ApplicabilitySource::Args, reason)
    };
    signals.push(args_applicability(
        InitCategory::Workspace,
        !args.skip_workspace_init(),
        "--skip-workspace-init",
    ));
    signals.push(args_applicability(
        InitCategory::NativeConfig,
        native_config_pending,
        "no native Agent config was uploaded",
    ));
    signals.push(args_applicability(
        InitCategory::Deps,
        !pending_candidates(config, None).is_empty(),
        "no pending dependency install actions",
    ));
    // A resumed run replays its configuration steps as skipped and a fully
    // declared run never prompts, so on those paths no write site ever fires
    // and the lanes would report `settled` with a null value. The config in
    // hand is the outcome, so it is what gets reported; when a lane is really
    // driven later, its write site settles it again with the same value, and
    // an `awaiting_input` prompt still outranks a settlement while it is live.
    // MCP is deliberately not settled here: only the probe knows whether the
    // installed agent can be given servers at all, so its lane settles there.
    // These three rest on the disk rather than on anything this run did, so they
    // settle provisionally: an agent that dropped a lane since the config was
    // written must still be able to retract it from the live discovery pass.
    if let Some(provider) = config.agent.provider.as_ref() {
        signals.push(InitStateSignal::CategoryProvisionallySettled {
            category: InitCategory::Provider,
            value: provider.id.clone(),
        });
    }
    // `write_model_into_config` puts the model in the provider slot for
    // provider-backed agents and at the agent root otherwise, clearing the slot
    // it did not use; reading them in that order recovers the written value.
    if let Some(model) = config
        .agent
        .provider
        .as_ref()
        .and_then(|provider| provider.model.clone())
        .or_else(|| config.agent.model.clone())
    {
        signals.push(InitStateSignal::CategoryProvisionallySettled {
            category: InitCategory::Model,
            value: model,
        });
    }
    if let Some(mode) = config.agent.mode.clone() {
        signals.push(InitStateSignal::CategoryProvisionallySettled {
            category: InitCategory::Mode,
            value: mode,
        });
    }
    signals
}

/// Re-adopt the skill plan a resumed run recorded, for a resume that redeclared
/// nothing about skills.
///
/// `agent_settlement_signals` runs well before this point and read the request's
/// own `no_skills`, so a resume inheriting a skills-off verdict from the original
/// run has already been reported to hosted clients as having a Skills lane. The
/// skills step will not run and the terminal sweep would settle the lane with no
/// value, so the verdict is corrected here — legally, since nothing has settled
/// Skills yet.
fn restore_recorded_skill_plan(args: &mut InitArgs, recorded: &RecordedInitArgs) {
    args.skills_source = recorded.skills_source.clone();
    args.skills = recorded.skills.clone();
    args.essential_skills = recorded.essential_skills;
    args.no_skills = recorded.no_skills;
    if args.no_skills {
        prompt::emit_state_signal(|| InitStateSignal::CategoryApplicability {
            category: InitCategory::Skills,
            applicable: false,
            source: ApplicabilitySource::Args,
            reason: Some("no skills were declared".to_owned()),
        });
    }
}

fn signal_category_failed(category: InitCategory, error: &StackError) {
    prompt::emit_state_signal(|| InitStateSignal::CategoryFailed {
        category,
        code: error.error_code().to_owned(),
    });
}

fn signal_step_started(kind: &'static str) {
    prompt::emit_state_signal(|| InitStateSignal::StepStarted { kind });
}

fn signal_step_finished(kind: &'static str, result: &Result<StepDisposition>) {
    prompt::emit_state_signal(|| InitStateSignal::StepFinished {
        kind,
        // A failed step has no disposition of its own; the error_code is what
        // distinguishes it, so the executed/skipped axis reports the body ran.
        disposition: result
            .as_ref()
            .copied()
            .unwrap_or(StepDisposition::Executed),
        error_code: result
            .as_ref()
            .err()
            .map(|error| error.error_code().to_owned()),
    });
}

/// `init_runner::record_step` bracketed with state signals. The runtime
/// recorder stays ignorant of hosted concepts, so the bracketing lives here,
/// on the driver side; the call order below is the authority on step sequence,
/// never the ordinals.
fn record_init_step(
    store: &StateStore,
    run: &crate::state::InitRunRecord,
    ordinal: i64,
    kind: &'static str,
    verify: impl FnOnce() -> Result<bool>,
    body: impl FnOnce() -> Result<StepOutcome>,
) -> Result<StepDisposition> {
    signal_step_started(kind);
    let result = record_step(store, run, ordinal, kind, verify, body);
    signal_step_finished(kind, &result);
    result
}

/// Signal-bracketed [`crate::runtime::init_runner::record_step_with_default_log_dir`].
#[allow(clippy::too_many_arguments)]
fn record_init_step_with_default_log_dir(
    store: &StateStore,
    run: &crate::state::InitRunRecord,
    ordinal: i64,
    kind: &'static str,
    default_log_dir: Option<&str>,
    verify: impl FnOnce() -> Result<bool>,
    body: impl FnOnce() -> Result<StepOutcome>,
) -> Result<StepDisposition> {
    signal_step_started(kind);
    let result = crate::runtime::init_runner::record_step_with_default_log_dir(
        store,
        run,
        ordinal,
        kind,
        default_log_dir,
        verify,
        body,
    );
    signal_step_finished(kind, &result);
    result
}

pub(in crate::cli) fn run_init(args: InitArgs, mode: InitMode) -> Result<()> {
    let output_mode = if args.handoff_json {
        InitOutputMode::HandoffJson
    } else {
        InitOutputMode::Text
    };
    run_init_with_output(args, mode, output_mode)
}

pub(super) fn run_hosted_init(args: InitArgs, mode: InitMode) -> Result<()> {
    run_init_with_output(args, mode, InitOutputMode::Hosted)
}

pub(in crate::cli) fn run_init_command(command: InitCommand, mode: InitMode) -> Result<()> {
    match command.command {
        Some(InitSubcommand::Serve(args)) => serve::run_init_serve(args),
        None => run_init(command.args, mode),
    }
}

fn run_init_with_output(
    mut args: InitArgs,
    mode: InitMode,
    output_mode: InitOutputMode,
) -> Result<()> {
    // Hosted init always rotates: the plaintext keys only ever travel in the
    // result frame, so a preserved run would leave the backend permanently
    // unable to obtain credentials for an instance with pre-existing state.
    // Folded into the flag BEFORE the run record is written so a later CLI
    // `acps init --resume` of a crashed hosted run replays the rotation and
    // reprints fresh keys instead of "preserving" already-invalidated ones.
    args.rotate_keys = args.rotate_keys || matches!(output_mode, InitOutputMode::Hosted);
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
    validate_stack_update_args(&args)?;
    validate_agent_update_args(&args)?;

    let home = home_dir()?;
    let config_path = config::default_config_path()?;
    let _mutation = acquire_agent_config_mutation_file_lock(&config_path)?;
    let state_path = default_state_path(&home);
    let config_dir = parent_dir(&config_path)?;
    let state_dir = parent_dir(&state_path)?;

    let mut pending_init_native_config =
        review_native_config_upload_for_init(&mut args, &config_path)?;
    create_dir_owner_only(config_dir)?;
    create_dir_owner_only(state_dir)?;
    prompt_config_source_if_needed(&mut args, &config_path, &state_path)?;
    let imported_config = import_config_for_init(&args, &config_path, output_mode)?;
    let registry = RegistryCatalog::load_with_override(&operator_registry_override(&home))?;

    // Preflight (untracked): new configs must start with a real registry
    // agent. This runs before writing the starter config so a declined or
    // missing first-run selection never leaves `agent.id = "placeholder"` on
    // disk.
    let creating_config = !config_path.exists();
    if creating_config && !args.resume {
        apply_supabase_env_defaults(&mut args)?;
    } else if !creating_config && !args.resume {
        reject_supabase_init_args_for_existing_config(&args)?;
        reject_agent_env_refs_for_existing_config(&args)?;
        reject_deps_args_for_existing_config(&args)?;
        reject_data_source_args_for_existing_config(&args)?;
    }
    // A custom agent declared via `--custom-agent-*` is resolved up front; it
    // satisfies the "real agent" requirement without an `--agent` registry id
    // and threads through both config apply sites below.
    let mut custom_agent_spec: Option<CustomAgentSpec> = resolve_custom_agent_spec(&args)?;
    if let Some(spec) = &custom_agent_spec {
        reject_registry_id_for_custom_agent(&spec.id, &registry)?;
    }
    if creating_config && !args.resume && args.agent.is_none() && custom_agent_spec.is_none() {
        if !prompts_enabled(&args) {
            return Err(StackError::InvalidParam {
                field: "--agent",
                reason: "non-interactive init requires selecting a real agent; run `acps init` in a TTY or pass `--non-interactive --agent <id>` or the `--custom-agent-*` flags".to_owned(),
            });
        }
        match select_agent_for_init(&args, &registry)?.ok_or_else(|| StackError::InvalidParam {
            field: "--agent",
            reason: "initializing a new config requires selecting a real agent".to_owned(),
        })? {
            AgentSelection::Registry(entry) => args.agent = Some(entry.id.clone()),
            AgentSelection::Custom(spec) => custom_agent_spec = Some(spec),
        }
    }
    let skill_catalog = SkillCatalog::load_embedded()?;
    if creating_config && !args.resume {
        prompt_environment_configuration_if_needed(&mut args, &registry, &skill_catalog)?;
    }
    // Operator agent env refs (flags + interactive add-loop). On a fresh run the
    // interactive loop also collects masked values; on resume only the replayed
    // `--agent-env-ref` names are re-collected below (interactive values cannot
    // be replayed). Names are appended to `config.agent.env` only after the store
    // verifies them (below), so a failed run never persists an unresolved ref.
    let mut agent_env_collection = if creating_config && !args.resume {
        collect_agent_env_refs_for_init(&args, prompts_enabled(&args))?
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
        resolve_init_run(&args, &store)?;
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
        validate_deployment_overrides_match_existing(&args, &existing_config)?;
        reject_starter_only_mcp_args_for_existing_config(&args)?;
        if imported_config {
            "imported config"
        } else {
            "validated existing config"
        }
    } else {
        let starter_config = starter_config(&args)?;
        let mut new_config = config::load_config_from_str(&starter_config)?;
        if let Some(spec) = &custom_agent_spec {
            apply_custom_agent_to_config(&mut new_config, spec);
        } else if let Some(agent_id) = args.agent.as_deref() {
            let entry = registry.lookup_required(agent_id)?;
            entry.ensure_supported()?;
            apply_registry_entry_to_config(&mut new_config, entry);
        }
        push_args_deps_to_config(&mut new_config, &args)?;
        if let Some(pending) = pending_init_native_config.as_mut() {
            native_config_provider_preapplied = prepare_native_config_for_new_init(
                &args,
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
    let init_run = resolve_init_run(&args, &store)?;
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
    if resumed && args.agent.is_none() {
        args.agent = recorded_args
            .as_ref()
            .and_then(|recorded| recorded.agent.clone())
            .or_else(|| {
                init_run
                    .agent_id
                    .clone()
                    .filter(|agent| agent != STARTER_AGENT_ID)
            });
    }
    #[cfg(feature = "dev-tools")]
    if resumed && let Some(recorded) = recorded_args.as_ref() {
        args.skip_workspace_init = args.skip_workspace_init || recorded.skip_workspace_init;
    }
    // Replay a recorded rotation request so a bare `--resume` cannot silently
    // downgrade a rotating run into a preserving one.
    if resumed && let Some(recorded) = recorded_args.as_ref() {
        args.rotate_keys = args.rotate_keys || recorded.rotate_keys;
    }
    if resumed
        && args.edge.is_none()
        && let Some(recorded) = recorded_args.as_ref()
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
    if resumed && let Some(recorded) = recorded_args.as_ref() {
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
    if resumed && let Some(recorded) = recorded_args.as_ref() {
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
    // On resume, re-collect the replayed `--agent-env-ref` names (flags only) so
    // they are re-verified against the now-open store rather than silently
    // dropped. Interactive values from the original run cannot be replayed.
    if resumed {
        agent_env_collection = collect_agent_env_refs_for_init(&args, false)?;
    }

    if resumed && let Some(recorded) = recorded_args.as_ref() {
        if args.model.is_none() {
            args.model = recorded.model.clone();
        }
        if args.mode.is_none() {
            args.mode = recorded.mode.clone();
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

    let mut config = Config::load_from_path(&config_path)?;
    // Skip the registry re-apply when it cannot or should not run: a custom
    // (non-registry) agent is already fully applied at creation time (and a
    // `lookup_required` on its id would fail), and an imported config without an
    // explicit `--agent` keeps the agent it was imported with.
    // Explicit `--custom-agent-*` flags override the skip so an operator can
    // re-point an existing custom agent. Explicit `--agent` also overrides an
    // existing custom config and switches back to the supported registry flow.
    let custom_agent_flags_present = resolve_custom_agent_spec(&args)?.is_some();
    let selected_agent = if !custom_agent_flags_present
        && args.agent.is_none()
        && (is_custom_agent(&config, &registry) || imported_config)
    {
        None
    } else {
        select_agent_for_init(&args, &registry)?
    };
    let agent_applied = match &selected_agent {
        Some(AgentSelection::Registry(entry)) => {
            // Fail fast on agents the runtime cannot drive headlessly (browser
            // OAuth, terminal-only adapters, etc.). Without this check init would
            // happily install the binary and only fail at first session spawn,
            // wasting bandwidth and operator time.
            entry.ensure_supported()?;
            apply_registry_entry_to_config(&mut config, entry);
            true
        }
        Some(AgentSelection::Custom(spec)) => {
            apply_custom_agent_to_config(&mut config, spec);
            true
        }
        None => false,
    };
    if agent_applied {
        let canonical = config.to_canonical_toml()?;
        config = config::load_config_from_str(&canonical)?;
        atomic_write_owner_only(&config_path, canonical.as_bytes())?;
    }
    // The agent is now final on every path into this point — fresh, existing,
    // imported, resumed, or custom — which is what makes the registry-derived
    // verdicts below trustworthy. `pending_init_native_config` still holds the
    // uploaded config; `args.native_config_revision` covers the resumed form.
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
        Err(error) => return finalize_with_error(&store, &init_run, error),
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
                    return finalize_with_error(&store, &init_run, error);
                }
            }
            args.provider = None;
        }
    }
    let mut init_native_config_record = match native_config::stage_for_init(
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
        Err(error) => return finalize_with_error(&store, &init_run, error),
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

    let edge_requested = apply_edge_profile_to_config(&args, &mut config)?;
    let supabase_configured = apply_supabase_to_config_for_init(&args, &mut config)?;
    prompt_init_skills_if_needed(&mut args, &config, &registry, &skill_catalog)?;
    if edge_requested || supabase_configured {
        let canonical = config.to_canonical_toml()?;
        config = config::load_config_from_str(&canonical)?;
        atomic_write_owner_only(&config_path, canonical.as_bytes())?;
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
        // A failed provider_configure step that owned only model (no
        // provider was ever set) can legitimately resume without `--provider`.
        // Only error when we know provider is required AND absent.
        let resume_recorded_provider = recorded_args.as_ref().and_then(|r| r.provider.clone());
        if args.provider.is_none() && resume_recorded_provider.is_some() {
            return finalize_with_error(
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
        return finalize_with_error(&store, &init_run, error);
    }

    // An unsatisfiable skills declaration (e.g. essential skills for an agent
    // without an install dir) is a hard error — a declaration silently
    // skipped would be worse — but it must finalize the run like every other
    // failure here, or the pending row would be adopted by a later --resume.
    let skill_install_plan =
        match resolve_skill_install_plan(&args, &home, &config, &registry, &skill_catalog) {
            Ok(plan) => plan,
            Err(error) => return finalize_with_error(&store, &init_run, error),
        };

    let mut auth_status: &'static str = "preserved existing API keys";
    let mut key_handover = KeyHandover {
        keys: None,
        output_mode,
        failure_context: None,
        auth_ready: false,
        emitted: false,
    };

    // -----------------------------------------------------------------
    // Step 1: secrets_init — generate or preserve session + admin verifiers.
    // Verifier: both verifier rows present in state.
    // -----------------------------------------------------------------
    let mut secret_store = SecretStore::open_or_create(&home)?;
    let mut handoff_context = InitHandoffContext {
        config_path: config_path.clone(),
        state_path: state_path.clone(),
        secret_store_path: secret_store.store_path().to_path_buf(),
        age_key_path: age_key_path(&home),
        agent_id: config.agent.id.clone(),
        agent_name: config.agent.name.clone(),
        native_config_import: None,
        ignored_features: Vec::new(),
    };
    key_handover.failure_context = Some(handoff_context.clone());
    init_println!(output_mode, "progress: initializing auth");
    // Hosted rotation was already folded into the flag at entry, so this
    // reads the single source of truth (and any replayed recorded value).
    let rotate_keys = args.rotate_keys;
    let key_policy = if rotate_keys {
        KeyPolicy::RotateExisting
    } else {
        KeyPolicy::PreserveExisting
    };
    let step_result = record_init_step(
        &store,
        &init_run,
        1,
        step_kind::SECRETS_INIT,
        // A rotating run must never replay as Skipped: a skipped step emits
        // no plaintext, which is exactly the wedge rotation exists to fix.
        || {
            if rotate_keys {
                Ok(false)
            } else {
                store.auth_key_pair_present()
            }
        },
        || {
            let outcome = perform_auth_init(
                &store,
                legacy_auth.as_ref(),
                &home,
                &mut secret_store,
                key_policy,
            )?;
            auth_status = outcome.status;
            let generated_keys = outcome.generated_keys;
            let rotated = outcome.rotated_keys;
            key_handover.keys = outcome.fresh_keys;
            key_handover.auth_ready = true;
            if generated_keys {
                let (kind, message) = if rotated {
                    ("auth.keys_rotated", "rotated session and admin API keys")
                } else {
                    (
                        "auth.keys_generated",
                        "generated session and admin API keys",
                    )
                };
                store.append_event_with_source(
                    "info",
                    kind,
                    crate::state::EVENT_SOURCE_CLI,
                    message,
                    &serde_json::json!({
                        "key_kinds": ["session", "admin"],
                    })
                    .to_string(),
                )?;
            }
            Ok(StepOutcome::with_payload(
                serde_json::json!({
                    "key_kinds": ["session", "admin"],
                    "status": auth_status,
                })
                .to_string(),
            ))
        },
    );
    let disposition = match step_result {
        Ok(d) => d,
        Err(error) => return finalize_with_error(&store, &init_run, error),
    };
    // Honest "auth:" line for the skipped path — we did not generate keys
    // this run, we trusted the verifier instead.
    let auth_status = if matches!(disposition, StepDisposition::Skipped) {
        key_handover.auth_ready = true;
        "preserved existing API keys"
    } else {
        auth_status
    };
    // Write interactively-collected agent env values and verify flag-provided
    // refs now that the store is open, before the agent is installed/launched so
    // `resolve_agent_env` resolves them. The ref names are appended to
    // `agent.env` only AFTER verification succeeds, so a run that fails here never
    // persists an unresolved ref (which a later `--resume` would otherwise
    // complete around). No-op when nothing was collected (a resume or an existing
    // config).
    let env_apply = (|| -> Result<()> {
        apply_agent_env_collection(&mut secret_store, &agent_env_collection)?;
        if append_agent_env_refs(&mut config, &agent_env_collection) {
            let canonical = config.to_canonical_toml()?;
            config = config::load_config_from_str(&canonical)?;
            atomic_write_owner_only(&config_path, canonical.as_bytes())?;
        }
        Ok(())
    })();
    if let Err(error) = env_apply {
        return finalize_with_error(&store, &init_run, error);
    }
    // Offer masked entry for secret refs declared by MCP servers and S3 data
    // sources (flags, wizard, or hosted request). Skipped refs are not an
    // error: they surface later in MCP health or workspace materialization,
    // and a hosted backend may push them through the secrets API post-init.
    // Resume runs re-offer refs that were skipped on the failed attempt.
    if creating_config || args.resume {
        match collect_declared_secret_refs_for_init(
            prompts_enabled(&args),
            &config,
            &mut secret_store,
        ) {
            Ok(stored) if !stored.is_empty() => {
                init_println!(output_mode, "declared secrets: set ({})", stored.join(", "));
            }
            Ok(_) => {}
            Err(error) => return finalize_with_error(&store, &init_run, error),
        }
    }
    // Hold the freshly-generated keys until init exits. Drop renders the
    // handover last (after the summary and testflight), and still surfaces them
    // if a later step fails and returns early.
    let mut key_handover = key_handover;
    if let Some(supabase) = config.logging.supabase.as_ref()
        && supabase.enabled
    {
        let stored = match ensure_supabase_secret(
            &mut secret_store,
            &supabase.api_key_ref,
            prompts_enabled(&args),
        ) {
            Ok(stored) => stored,
            Err(error) => return finalize_with_error(&store, &init_run, error),
        };
        if stored {
            init_println!(
                output_mode,
                "supabase secret: set ({})",
                supabase.api_key_ref
            );
        } else {
            init_println!(
                output_mode,
                "supabase secret: preserved ({})",
                supabase.api_key_ref
            );
        }
    }

    if let Some(record) = init_native_config_record.as_mut()
        && let Err(error) =
            native_config::rebase_for_init(record, &config, &config_path, &state_path, &home)
    {
        return finalize_with_error(&store, &init_run, error);
    }
    if let Some(prepared) = init_native_config_record
        .as_ref()
        .and_then(|record| record.prepared.as_ref())
        && let Err(error) = collect_prepared_secret_refs_for_init(
            &args,
            &registry,
            &prepared.canonical_config,
            &config_path,
            &mut secret_store,
        )
        .and_then(|()| {
            crate::runtime::agent::native_config_import::validate_native_config_secret_refs(
                prepared, &home,
            )
        })
    {
        return finalize_with_error(&store, &init_run, error);
    }

    // -----------------------------------------------------------------
    // Step 2: agent_install — install the configured agent if requested.
    // -----------------------------------------------------------------
    let install_requested = should_install_agent(&config, &registry)?;
    let mut install_outcome: Option<InstallerOutcome> = None;
    let install_step_needs_resume = step_needs_resume(&prior_init_steps, step_kind::AGENT_INSTALL);
    if install_requested || install_step_needs_resume {
        let install_interactive = prompts_enabled(&args);
        let verify_config = config.clone();
        let verify_workspace_root = PathBuf::from(config.workspace.root.clone());
        let verify_local_bin_dir = local_bin_dir(&home);
        let result = record_init_step(
            &store,
            &init_run,
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
                // Snapshot the latest installer_runs row ids for this
                // agent so the install closure can correlate the init
                // step row to whichever installer attempts the install
                // produced. Doing the lookup before AND after the
                // install lets the payload list precisely the rows that
                // belong to this attempt.
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
                                install_configured_agent(&home, &config, &registry, &store)
                            })
                        } else {
                            init_println!(output_mode, "progress: {message}");
                            install_configured_agent(&home, &config, &registry, &store)
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
                install_outcome = Some(outcome.clone());
                let payload = serde_json::json!({
                    "label": label,
                    "path": path,
                    "installer_run_ids": new_installer_run_ids,
                });
                Ok(StepOutcome::with_payload(payload.to_string()))
            },
        );
        if let Err(error) = result {
            return finalize_with_error(&store, &init_run, error);
        }
    }

    // -----------------------------------------------------------------
    // Step 11: native_config_import — apply the reviewed native global
    // config after installation and before the first discovery launch.
    // -----------------------------------------------------------------
    if let Some(record) = init_native_config_record.as_mut() {
        init_println!(output_mode, "progress: importing native Agent config");
        let already_applied = record.phase
            == crate::runtime::agent::native_config_import::NativeConfigOperationPhase::Applied;
        let result = record_init_step(
            &store,
            &init_run,
            11,
            step_kind::NATIVE_CONFIG_IMPORT,
            || Ok(already_applied),
            || {
                let (updated, operation) =
                    native_config::apply_for_init(record, &config_path, &state_path, &home)?;
                config = updated;
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
            return finalize_with_error(&store, &init_run, error);
        }
        if record.phase
            == crate::runtime::agent::native_config_import::NativeConfigOperationPhase::Applied
        {
            handoff_context.native_config_import = Some(record.operation.clone());
            if let Some(context) = key_handover.failure_context.as_mut() {
                context.native_config_import = Some(record.operation.clone());
            }
            config = Config::load_from_path(&config_path)?;
        }
    }

    // -----------------------------------------------------------------
    // Step 3: agent_skills_install — install selected Agent Skills before
    // first launch/testflight. Agent harnesses auto-detect the files.
    // -----------------------------------------------------------------
    let mut skill_install_reports: Vec<SkillInstallReport> = Vec::new();
    let skill_step_needs_resume =
        step_needs_resume(&prior_init_steps, step_kind::AGENT_SKILLS_INSTALL);
    if skill_install_plan.is_some() || skill_step_needs_resume {
        init_println!(output_mode, "progress: installing agent skills");
        let Some(plan) = skill_install_plan.clone() else {
            return finalize_with_error(
                &store,
                &init_run,
                StackError::InitRunCorrupted {
                    reason: format!(
                        "init run {} has a failed agent_skills_install step but no recorded skill install request",
                        init_run.id
                    ),
                },
            );
        };
        let verify_plan = plan.clone();
        let result = record_init_step(
            &store,
            &init_run,
            9,
            step_kind::AGENT_SKILLS_INSTALL,
            || {
                Ok(skill_install_postcondition_holds(
                    &verify_plan,
                    &prior_init_steps,
                ))
            },
            || {
                let (reports, link_outcome) =
                    install_init_skills(&plan, &home, &config, &registry)?;
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
                skill_install_reports = reports;
                Ok(StepOutcome::with_payload(payload))
            },
        );
        if let Err(error) = result {
            return finalize_with_error(&store, &init_run, error);
        }
    }

    // -----------------------------------------------------------------
    // Step 3: workspace_materialize — clone repos + download/extract
    // data sources into /workspace/usr/. Skipped if --skip-workspace-init.
    // Verifier: every source destination has its sentinel file.
    // -----------------------------------------------------------------
    let workspace_for_verify = config.workspace.clone();
    let mut materialize_report = None;
    if !args.skip_workspace_init()
        || step_needs_resume(&prior_init_steps, step_kind::WORKSPACE_MATERIALIZE)
    {
        init_println!(output_mode, "progress: materializing workspace sources");
        let log_paths =
            crate::runtime::workspace_sources::workspace_init::WorkspaceLogPaths::for_run(
                &crate::runtime::workspace_sources::workspace_init::default_workspace_init_log_base(
                    &home,
                ),
                &init_run.id,
            );
        create_dir_owner_only(&log_paths.run_dir)?;
        // Pre-compute the log_dir path so a mid-clone failure still
        // records it on the init_steps row — otherwise the operator
        // would see `log_dir = NULL` exactly when they need the
        // captured stderr most.
        let log_dir_str = log_paths.run_dir.display().to_string();
        let result = record_init_step_with_default_log_dir(
            &store,
            &init_run,
            3,
            step_kind::WORKSPACE_MATERIALIZE,
            Some(&log_dir_str),
            || Ok(workspace_postcondition_holds(&workspace_for_verify)),
            || {
                let report =
                    crate::runtime::workspace_sources::workspace_init::materialize_workspace(
                        &config.workspace,
                        &secret_store,
                        Some(&log_paths),
                    )?;
                let step_log_dir = report.log_dir.as_ref().map(|p| p.display().to_string());
                materialize_report = Some(report);
                Ok(StepOutcome {
                    log_dir: step_log_dir,
                    payload_json: "{}".to_owned(),
                })
            },
        );
        if let Err(error) = result {
            return finalize_with_error(&store, &init_run, error);
        }
    } else {
        init_println!(output_mode, "workspace: skipped (--skip-workspace-init)");
        prompt::emit_state_signal(|| {
            applicability(
                InitCategory::Workspace,
                false,
                ApplicabilitySource::Args,
                "--skip-workspace-init",
            )
        });
    }

    // -----------------------------------------------------------------
    // Step 10 (ordinal): deps_apply — run declared dependency install
    // actions before the agent is launched for provider/model discovery, so
    // deps the agent needs to run already exist. Opt-in: a TTY confirm, or
    // `--deps-apply --deps-apply-yes` non-interactively.
    // -----------------------------------------------------------------
    let deps_candidates = pending_candidates(&config, None);
    if deps_candidates.is_empty() {
        // Re-asserted here rather than trusted from the agent-settlement
        // derivation: the install and workspace steps in between can satisfy
        // the last pending action.
        prompt::emit_state_signal(|| {
            applicability(
                InitCategory::Deps,
                false,
                ApplicabilitySource::Args,
                "no pending dependency install actions",
            )
        });
    }
    // Probe escalation once and reuse it for the preflight notice and the
    // apply itself, so the prompt cannot promise a mode the apply won't use.
    let deps_escalation = if pending_system_candidates(&config, None).is_empty() {
        PrivilegeEscalation::NotNeeded
    } else {
        probe_privilege_escalation()
    };
    // Wrap in finalize_with_error so a confirmation error (e.g. `--deps-apply`
    // without `--deps-apply-yes`) marks the run terminal instead of leaving it
    // pending after the earlier steps already succeeded.
    let deps_apply_requested = match should_apply_deps_for_init(
        &args,
        &deps_candidates,
        prompts_enabled(&args),
        &deps_escalation,
        &config.workspace.default_shell,
        &mut |line| init_println!(output_mode, "{line}"),
    ) {
        Ok(requested) => requested,
        Err(error) => return finalize_with_error(&store, &init_run, error),
    };
    if deps_apply_requested || step_needs_resume(&prior_init_steps, step_kind::DEPS_APPLY) {
        init_println!(output_mode, "progress: applying dependencies");
        let result = record_init_step(
            &store,
            &init_run,
            10,
            step_kind::DEPS_APPLY,
            || Ok(pending_candidates(&config, None).is_empty()),
            || {
                let report = apply_dependencies_with_escalation(
                    &config,
                    None,
                    Some(&store),
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
                // Genuine action failures fail init: the operator confirmed
                // an apply that then broke. Privilege skips do not — an
                // un-escalatable host is a host property, so init degrades
                // and continues to provider/auth collection; the skipped
                // deps stay visible via `privilege_required` audit rows and
                // health, and a later resume re-runs them because the step
                // verifier (`pending_candidates(...).is_empty()`) is still
                // false for them.
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
                        // The outcome carries the real euid;
                        // `deps_escalation.uid()` reports 0 under
                        // `NotNeeded`, which can still reach
                        // PrivilegeRequired when a system dep turned
                        // pending between probe and apply.
                        uid = skipped_privilege_uid.unwrap_or_default(),
                    );
                    for candidate in pending_system_candidates(&config, None) {
                        init_println!(
                            output_mode,
                            "  - {name}: {manual}",
                            name = candidate.name,
                            manual = manual_privileged_command(
                                &config.workspace.default_shell,
                                &candidate,
                            ),
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
        if let Err(error) = result {
            return finalize_with_error(&store, &init_run, error);
        }
    }

    // -----------------------------------------------------------------
    // Step 12: capability_probe — handshake-only spawn of the installed
    // agent to capture its ACP `initialize` advertisement, which feeds the
    // MCP prompt gate below, the ignored-features report, and (persisted)
    // `GET /v1/agent/capabilities`. A failed probe never fails init.
    // -----------------------------------------------------------------
    init_println!(output_mode, "progress: probing agent capabilities");
    let mut probed_capabilities: Option<crate::runtime::agent::acp_bridge::AgentCapabilitiesDto> =
        None;
    let mut ignored_features: Vec<crate::runtime::agent::acp_bridge::IgnoredFeature> = Vec::new();
    let result = record_init_step(
        &store,
        &init_run,
        12,
        step_kind::CAPABILITY_PROBE,
        // Always re-probe on resume: a reinstall or update between runs can
        // change the advertisement, and a stale "supported" is worse than one
        // redundant short-lived spawn.
        || Ok(false),
        || {
            let outcome = probe_agent_capabilities_for_init(&home, &config);
            // The handshake is the only authority on MCP: the registry has no
            // MCP column, so whatever the agent just advertised (or failed to)
            // overrides the provisional verdict.
            prompt::emit_state_signal(|| mcp_applicability_from_probe(&outcome));
            match outcome {
                CapabilityProbeOutcome::Probed(capabilities) => {
                    store.upsert_agent_capabilities(&config.agent.id, &capabilities.to_json()?)?;
                    // The ignore assessment is best-effort: an unresolvable MCP
                    // declaration (missing secret, absent stdio binary) is a
                    // pre-existing config condition surfaced at session time, not
                    // a reason to fail the probe step.
                    match crate::runtime::agent::mcp::resolve_mcp_servers(
                        &config.mcp,
                        &secret_store,
                    )
                    .and_then(|declared| capabilities.ignored_mcp_features(declared))
                    {
                        Ok(ignored) => ignored_features = ignored,
                        Err(error) => {
                            tracing::warn!(
                                %error,
                                "skipping MCP capability assessment: declared servers did not resolve"
                            );
                        }
                    }
                    if let Some(settlement) =
                        mcp_settlement_from_probe(&capabilities, &config, &ignored_features)
                    {
                        prompt::emit_state_signal(|| settlement);
                    }
                    let payload = serde_json::json!({
                        "probe_status": "ok",
                        "protocol_version": capabilities.protocol_version,
                        "agent_name": capabilities.agent_name,
                        "ignored": ignored_features,
                    });
                    probed_capabilities = Some(capabilities);
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
        return finalize_with_error(&store, &init_run, error);
    }
    handoff_context.ignored_features = ignored_features.clone();
    if let Some(context) = key_handover.failure_context.as_mut() {
        context.ignored_features = ignored_features.clone();
    }

    // -----------------------------------------------------------------
    // Step 13: mcp_configure — interactive MCP prompting. Lives after the
    // probe (not in the pre-install wizard) because MCP support is only
    // knowable from the installed agent's advertisement, which also bounds
    // the transport picker below (`advertises_mcp_support`, `offer_http`).
    // Flag-driven runs declare MCP in the starter config and are covered by
    // the ignored-features report. Hosted runs get the same prompts on the
    // stream, each carrying its machine-readable kind; a session that
    // declared MCP servers in its start request arrives here with a
    // non-empty `config.mcp.servers` and skips prompting outright, so
    // declaring up front still wins.
    // -----------------------------------------------------------------
    let mcp_prompting_active = mcp_prompting_enabled(&args, creating_config, &config);
    // `step_needs_resume`: a resumed run must still settle a prior failed
    // `mcp_configure` row even though prompting is gated off on resume — the
    // body then settles it without prompts (the confirm gate below is
    // `mcp_prompting_active`, false on every resume path).
    if mcp_prompting_active || step_needs_resume(&prior_init_steps, step_kind::MCP_CONFIGURE) {
        let result = record_init_step(
            &store,
            &init_run,
            13,
            step_kind::MCP_CONFIGURE,
            // Interactively-collected answers cannot be replayed; a prior
            // succeeded row skips instead of re-driving prompts on resume.
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
                // The gate stays outside the call rather than riding only the
                // `interactive` argument: `prompt::confirm` consults the
                // hosted driver before that flag, so an unguarded call would
                // re-drive the wizard on a resumed hosted run, whose answers
                // this step cannot replay.
                if mcp_prompting_active
                    && prompt::confirm(
                        prompt::HostedPromptKind::McpAdd,
                        mcp_prompting_active,
                        "Add MCP servers?",
                        false,
                    )?
                {
                    prompt_mcp_servers(mcp_prompting_active, &mut args, offer_http)?;
                }
                let new_servers =
                    mcp_servers_from_prompted(&args.prompt_mcp_stdio, &args.prompt_mcp_http)?;
                let added = merge_prompted_mcp_servers(&mut config.mcp.servers, new_servers)?;
                if !added.is_empty() {
                    let canonical = config.to_canonical_toml()?;
                    // The reassignment is what makes provider_configure and
                    // agent_headless_config see the servers: later steps read
                    // the in-memory config, not the file.
                    config = config::load_config_from_str(&canonical)?;
                    atomic_write_owner_only(&config_path, canonical.as_bytes())?;
                    let stored = collect_mcp_secret_refs_for_init(
                        mcp_prompting_active,
                        &config,
                        &mut secret_store,
                    )?;
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
            return finalize_with_error(&store, &init_run, error);
        }
    }

    // -----------------------------------------------------------------
    // Step 4: provider_configure — write provider/model into the config
    // and persist canonical TOML if anything changed.
    // -----------------------------------------------------------------
    init_println!(output_mode, "progress: configuring provider and model");
    let provider_verify_config = config.clone();
    let provider_verify_home = home.clone();
    let result = record_init_step(
        &store,
        &init_run,
        4,
        step_kind::PROVIDER_CONFIGURE,
        || {
            // Provider config is idempotent only when there's no explicit
            // change requested for any lane this step owns (provider, model,
            // mode). We always re-run on resume so partial writes (e.g. missing
            // secret refs) get re-collected, and so a resumed `--model`/`--mode`
            // still gets validated and persisted rather than silently skipped
            // because the prior succeeded row passes the verifier.
            let secret_store = SecretStore::open(&provider_verify_home)?;
            Ok(args.provider.is_none()
                && args.model.is_none()
                && args.mode.is_none()
                && configured_provider_refs_satisfied(
                    &registry,
                    &provider_verify_config,
                    &secret_store,
                ))
        },
        || {
            // All three lanes live inside one step, so a step-level failure
            // alone could not say which of them broke; the lane badges itself
            // before the error propagates. The model/mode lanes badge
            // themselves from inside `configure_model_and_mode_for_init`, which
            // is the only place that knows which of the two was live.
            // Settlement rides the config writes, the one place each value is
            // written.
            let provider_configured = configure_provider_for_init(
                &args,
                &registry,
                &mut config,
                &config_path,
                &mut secret_store,
            )
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
                &args,
                &home,
                &registry,
                &mut config,
                &config_path,
            )?;
            // Custom agents skip provider/model discovery, so they would
            // otherwise never spawn during init. Gate on an ACP session here so a
            // non-ACP or broken custom binary is caught now, not at first session.
            if is_custom_agent(&config, &registry) {
                verify_agent_acp_connection(&home, &config, output_mode.is_text())?;
            }
            let model_mode_changed =
                matches!(model_mode_outcome.model_action, ModelModeAction::Set)
                    || matches!(model_mode_outcome.mode_action, ModelModeAction::Set);
            let subagent_configured = configure_subagent_inherit_for_init(
                prompts_enabled(&args),
                &registry,
                &mut config,
            )?;
            if selected_agent.is_some()
                || provider_configured
                || edge_requested
                || model_mode_changed
                || subagent_configured
            {
                let canonical = config.to_canonical_toml()?;
                config = config::load_config_from_str(&canonical)?;
                atomic_write_owner_only(&config_path, canonical.as_bytes())?;
            }
            Ok(StepOutcome::with_payload(format!(
                r#"{{"provider_configured":{provider_configured},"model_action":"{:?}","mode_action":"{:?}","subagent_configured":{subagent_configured}}}"#,
                model_mode_outcome.model_action, model_mode_outcome.mode_action,
            )))
        },
    );
    if let Err(error) = result {
        return finalize_with_error(&store, &init_run, error);
    }

    // acp-stack auto-update: configure `[updates.acp_stack]` before the summary.
    // Flags apply on any run; the interactive prompt is suppressed on resume.
    let stack_update_outcome = (|| -> Result<()> {
        let changed = configure_stack_update_for_init(
            &args,
            &mut config,
            prompts_enabled(&args) && !args.resume && creating_config,
        )?;
        if changed {
            let canonical = config.to_canonical_toml()?;
            config = config::load_config_from_str(&canonical)?;
            atomic_write_owner_only(&config_path, canonical.as_bytes())?;
        }
        Ok(())
    })();
    if let Err(error) = stack_update_outcome {
        return finalize_with_error(&store, &init_run, error);
    }

    // Managed agent auto-update: override the `[agent.auto_update]` default that
    // `apply_registry_entry_to_config` seeded. Whether the agent is managed comes
    // from the registry, not block presence, so an imported/re-init config that
    // lacks the block is still treated as managed. Same interactivity gate as the
    // stack-update step; the prompt only appears for managed registry agents.
    let agent_update_outcome = (|| -> Result<()> {
        let managed = !is_custom_agent(&config, &registry);
        let changed = configure_agent_update_for_init(
            &args,
            &mut config,
            managed,
            prompts_enabled(&args) && !args.resume && creating_config,
        )?;
        if changed {
            let canonical = config.to_canonical_toml()?;
            config = config::load_config_from_str(&canonical)?;
            atomic_write_owner_only(&config_path, canonical.as_bytes())?;
        }
        Ok(())
    })();
    if let Err(error) = agent_update_outcome {
        return finalize_with_error(&store, &init_run, error);
    }

    // -----------------------------------------------------------------
    // Step 5: agent_headless_config — write the agent's local config
    // files so the harness can start without first-run prompts.
    // -----------------------------------------------------------------
    let mut provisioned_agent_configs = Vec::new();
    init_println!(output_mode, "progress: writing agent headless config");
    let result = record_init_step(
        &store,
        &init_run,
        5,
        step_kind::AGENT_HEADLESS_CONFIG,
        || {
            // provision is idempotent (atomic_write_owner_only); cheap to
            // re-run, so the verifier just always says no — every run we
            // re-derive the canonical output. This is correct for resume
            // because the operator's config may have changed since last run.
            Ok(false)
        },
        || {
            let candidate_paths = headless_config_candidate_paths(&config.agent.id, &home);
            let snapshots = capture_path_snapshots(&candidate_paths)?;
            let mut dir_scan = candidate_paths
                .iter()
                .filter_map(|path| path.parent().map(Path::to_path_buf))
                .collect::<Vec<_>>();
            dir_scan.extend(headless_config_side_dirs(&config.agent.id, &home));
            let dir_listings = capture_dir_listings_for(&dir_scan)?;

            crate::runtime::agent::provider_model_catalog::refresh_provider_models_best_effort_blocking(
                &home, &config,
            );
            match crate::runtime::agent::agent_headless_config::provision_agent_headless_config(
                &config, &home,
            ) {
                Ok(paths) => {
                    provisioned_agent_configs = paths;
                    Ok(StepOutcome::empty())
                }
                Err(error) => {
                    restore_headless_snapshots(snapshots);
                    remove_new_files_in_dirs(dir_listings);
                    Err(error)
                }
            }
        },
    );
    if let Err(error) = result {
        return finalize_with_error(&store, &init_run, error);
    }

    // -----------------------------------------------------------------
    // Step 6: edge_artifacts — render Cloudflare config files when an
    // edge profile was requested.
    // -----------------------------------------------------------------
    let mut provisioned_edge_artifacts = Vec::new();
    if edge_requested || step_needs_resume(&prior_init_steps, step_kind::EDGE_ARTIFACTS) {
        init_println!(output_mode, "progress: preparing Cloudflare edge artifacts");
        let result = record_init_step(
            &store,
            &init_run,
            6,
            step_kind::EDGE_ARTIFACTS,
            || Ok(false),
            || {
                let config_dir = parent_dir(&config_path)?;
                provisioned_edge_artifacts =
                    match config.edge.cloudflare.as_ref() {
                        Some(cloudflare) if cloudflare.enabled && cloudflare.mode == "managed" => {
                            let service_url = crate::edge::service_url_from_bind(&config.api.bind)?;
                            let api_token_ref = cloudflare.api_token_ref.clone().ok_or(
                                StackError::MissingField {
                                    field: "edge.cloudflare.api_token_ref",
                                },
                            )?;
                            let account_id_ref = cloudflare.account_id_ref.clone().ok_or(
                                StackError::MissingField {
                                    field: "edge.cloudflare.account_id_ref",
                                },
                            )?;
                            let api_token = secret_store.get(&api_token_ref)?.to_owned();
                            let account_id = secret_store.get(&account_id_ref)?.to_owned();
                            let created_tunnel = {
                                let cloudflare = config.edge.cloudflare.as_mut().ok_or(
                                    StackError::MissingField {
                                        field: "edge.cloudflare",
                                    },
                                )?;
                                crate::edge::ensure_managed_cloudflare_tunnel(
                                    cloudflare,
                                    &api_token,
                                    &account_id,
                                )?
                            };
                            if created_tunnel {
                                let canonical = config.to_canonical_toml()?;
                                config = config::load_config_from_str(&canonical)?;
                                atomic_write_owner_only(&config_path, canonical.as_bytes())?;
                            }
                            let cloudflare = config.edge.cloudflare.as_ref().ok_or(
                                StackError::MissingField {
                                    field: "edge.cloudflare",
                                },
                            )?;
                            crate::edge::finish_managed_cloudflare_provisioning(
                                config_dir,
                                cloudflare,
                                &service_url,
                                &api_token,
                                &account_id,
                            )?
                        }
                        Some(cloudflare) if cloudflare.enabled => {
                            let service_url = crate::edge::service_url_from_bind(&config.api.bind)?;
                            crate::edge::write_cloudflare_artifacts(
                                config_dir,
                                cloudflare,
                                &service_url,
                            )?
                        }
                        _ => Vec::new(),
                    };
                Ok(StepOutcome::empty())
            },
        );
        if let Err(error) = result {
            return finalize_with_error(&store, &init_run, error);
        }
    }

    // -----------------------------------------------------------------
    // Step 7: init_complete — record the durable "initialized" event.
    // Resume verifier: the event is already present in the unified log.
    // -----------------------------------------------------------------
    let verify_run_id = init_run.id.clone();
    let result = record_init_step(
        &store,
        &init_run,
        7,
        step_kind::INIT_COMPLETE,
        || Ok(init_complete_event_already_recorded(&store, &verify_run_id)),
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
        return finalize_with_error(&store, &init_run, error);
    }

    init_println!(output_mode, "initialized acp-stack");
    init_println!(output_mode, "{config_status}: {}", config_path.display());
    init_println!(output_mode, "state: {}", state_path.display());
    init_println!(
        output_mode,
        "secrets: {}",
        secret_store.store_path().display()
    );
    init_println!(output_mode, "age key: {}", age_key_path(&home).display());
    init_println!(output_mode, "auth: {auth_status}");
    init_println!(
        output_mode,
        "agent: {} ({})",
        config.agent.name,
        config.agent.id
    );
    if let Some(outcome) = install_outcome {
        init_println!(output_mode, "agent install: {}", outcome.label());
        init_println!(output_mode, "agent path: {}", outcome.path().display());
        init_println!(output_mode, "agent sha256: {}", outcome.sha256());
    }
    for report in skill_install_reports {
        for entry in report.installed {
            init_println!(
                output_mode,
                "skill installed: {} -> {}",
                entry.name,
                entry.path.display()
            );
        }
        for entry in report.skipped {
            init_println!(output_mode, "skill already installed: {}", entry.name);
        }
    }
    for provisioned in provisioned_agent_configs {
        init_println!(
            output_mode,
            "{}: {}",
            provisioned.label,
            provisioned.path.display()
        );
    }
    for artifact in provisioned_edge_artifacts {
        init_println!(
            output_mode,
            "{}: {}",
            artifact.label,
            artifact.path.display()
        );
    }
    if let Some(materialize) = &materialize_report {
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
        for ignored in &ignored_features {
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

    // -----------------------------------------------------------------
    // Step 8: testflight — optional real-prompt test. Decision uses
    // the resolver above; only `Run` actually executes the agent.
    // -----------------------------------------------------------------
    if let Some(decision) = resolve_testflight_decision(&args, &config, &registry)? {
        let result = record_init_step(
            &store,
            &init_run,
            8,
            step_kind::TESTFLIGHT,
            || Ok(!matches!(decision, TestflightDecision::Run)),
            || {
                match decision {
                    TestflightDecision::Run => {
                        init_println!(output_mode, "---");
                        init_println!(output_mode, "running real-prompt agent testflight");
                        crate::cli::agent::run_init_testflight(
                            &home,
                            &config,
                            &registry,
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
                }
                Ok(StepOutcome::with_payload(format!(
                    r#"{{"decision":"{decision:?}"}}"#
                )))
            },
        );
        if let Err(error) = result {
            return finalize_with_error(&store, &init_run, error);
        }
    }

    // Resume-aware finalization. If a prior step in this run is still
    // `pending`, `running`, or `failed` (because the current invocation's
    // flags skipped over it),
    // the aggregate run status must NOT settle to `succeeded`. We mark
    // it `failed` instead and surface a clear error so the operator
    // knows to re-run with the original flags.
    let prior_steps = store.query_init_steps(&init_run.id)?;
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
        crate::runtime::init_runner::finalize_run(&store, &init_run.id, INIT_RUN_FAILED)?;
        return Err(StackError::InitRunCorrupted {
            reason: format!(
                "init run {} has unsettled steps {unsettled:?}; re-run with the original flags to drive them to completion",
                init_run.id,
            ),
        });
    }
    if output_mode.is_machine_handoff() {
        key_handover.record(&store, &init_run.id)?;
        crate::runtime::init_runner::finalize_run(&store, &init_run.id, INIT_RUN_SUCCEEDED)?;
        if output_mode.is_handoff_json() {
            key_handover.print_handoff_json("initialized", &handoff_context)?;
        } else {
            key_handover.emit_handoff_payload("initialized", &handoff_context);
        }
    } else {
        // Finalize before printing so a state-store failure here surfaces as a
        // failed run (keys still reach the operator via the Drop guard) instead
        // of a success handover followed by a nonzero exit.
        crate::runtime::init_runner::finalize_run(&store, &init_run.id, INIT_RUN_SUCCEEDED)?;
        key_handover.print_and_record(&store, &init_run.id)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::sync::Arc;

    #[derive(Debug, Parser)]
    struct TestInitArgs {
        #[command(flatten)]
        args: InitArgs,
    }

    fn parse_init_args(args: &[&str]) -> InitArgs {
        let mut argv = vec!["init-test"];
        argv.extend_from_slice(args);
        TestInitArgs::parse_from(argv).args
    }

    #[test]
    fn handoff_json_disables_shared_prompt_gate_with_terminal_stdin() {
        let interactive = parse_init_args(&[]);
        assert!(prompts_enabled_for(&interactive, true));

        let handoff = parse_init_args(&["--handoff-json"]);
        assert!(!prompts_enabled_for(&handoff, true));
    }

    #[test]
    fn agent_install_progress_message_hides_attempt_on_first_try() {
        assert_eq!(agent_install_progress_message(1), "installing agent");
        assert_eq!(
            agent_install_progress_message(2),
            format!("installing agent (attempt 2/{MAX_INSTALL_ATTEMPTS})")
        );
    }

    use prompt::RecordingPromptDriver as RecordingDriver;

    fn config_for_agent(agent_id: &str) -> Config {
        let mut config = config::load_config_from_str(include_str!(
            "../../../tests/fixtures/valid-opencode-stack.toml"
        ))
        .expect("fixture config");
        config.agent.id = agent_id.to_owned();
        config
    }

    fn applicability_of(signals: &[InitStateSignal], wanted: InitCategory) -> Option<bool> {
        signals.iter().find_map(|signal| match signal {
            InitStateSignal::CategoryApplicability {
                category,
                applicable,
                ..
            } if *category == wanted => Some(*applicable),
            _ => None,
        })
    }

    fn settlement_signals_for(agent_id: &str) -> Vec<InitStateSignal> {
        let registry = RegistryCatalog::load_embedded().expect("registry");
        let args = parse_init_args(&[]);
        agent_settlement_signals(&config_for_agent(agent_id), &registry, &args, false)
    }

    // amp declares set_provider=false/set_model=false in the registry, so a
    // client must not show either lane as pending input that will never come.
    #[test]
    fn registry_derivation_marks_amp_provider_and_model_inapplicable() {
        let signals = settlement_signals_for("amp");
        assert_eq!(
            signals.first(),
            Some(&InitStateSignal::CategorySettled {
                category: InitCategory::Agent,
                value: Some("amp".to_owned()),
            }),
            "agent settles before anything is derived from it"
        );
        assert_eq!(
            applicability_of(&signals, InitCategory::Provider),
            Some(false)
        );
        assert_eq!(applicability_of(&signals, InitCategory::Model), Some(false));
        assert_eq!(applicability_of(&signals, InitCategory::Mode), Some(true));
        assert_eq!(applicability_of(&signals, InitCategory::Skills), Some(true));
    }

    #[test]
    fn registry_derivation_marks_cursor_provider_inapplicable_but_keeps_model() {
        let signals = settlement_signals_for("cursor");
        assert_eq!(
            applicability_of(&signals, InitCategory::Provider),
            Some(false)
        );
        assert_eq!(applicability_of(&signals, InitCategory::Model), Some(true));
        assert_eq!(applicability_of(&signals, InitCategory::Mode), Some(true));
    }

    #[test]
    fn registry_derivation_marks_every_harness_lane_inapplicable_for_a_custom_agent() {
        let signals = settlement_signals_for("not-in-the-registry");
        for category in [
            InitCategory::Provider,
            InitCategory::Model,
            InitCategory::Mode,
            InitCategory::Skills,
        ] {
            assert_eq!(
                applicability_of(&signals, category),
                Some(false),
                "custom agents configure {} outside acp-stack",
                category.id()
            );
        }
    }

    #[test]
    fn registry_derivation_reports_mcp_nowhere_and_reads_flags_for_the_rest() {
        let signals = settlement_signals_for("opencode");
        assert_eq!(
            applicability_of(&signals, InitCategory::Mcp),
            None,
            "only the capability probe may rule on MCP"
        );
        assert_eq!(
            applicability_of(&signals, InitCategory::Workspace),
            Some(true)
        );
        assert_eq!(
            applicability_of(&signals, InitCategory::NativeConfig),
            Some(false),
            "no native config was uploaded"
        );
        assert_eq!(
            applicability_of(&signals, InitCategory::Deps),
            Some(false),
            "the fixture declares no dependency install actions"
        );
    }

    #[test]
    fn registry_derivation_honors_no_skills() {
        let registry = RegistryCatalog::load_embedded().expect("registry");
        let args = parse_init_args(&["--no-skills"]);
        let signals = agent_settlement_signals(&config_for_agent("amp"), &registry, &args, false);
        assert_eq!(
            applicability_of(&signals, InitCategory::Skills),
            Some(false)
        );
    }

    fn skills_applicability_under_restore(recorded: &RecordedInitArgs) -> (InitArgs, Option<bool>) {
        // The hosted resume shape: the request redeclares nothing about skills,
        // so `no_skills` is false when the settlement signals go out and only
        // the recorded run can put it back.
        let mut args = parse_init_args(&["--resume"]);
        let driver = Arc::new(RecordingDriver::default());
        prompt::with_hosted_driver(driver.clone(), || {
            restore_recorded_skill_plan(&mut args, recorded);
        });
        let applicability = applicability_of(&driver.recorded(), InitCategory::Skills);
        (args, applicability)
    }

    // The settlement signals fire before the recorded args are restored, so a
    // resume of a `--no-skills` run has already reported the lane as applicable
    // by the time the restore turns the skills step off. Without the correction
    // the terminal sweep would settle Skills with no value, telling the client
    // the lane ran.
    #[test]
    fn a_resume_that_inherits_no_skills_withdraws_the_skills_lane() {
        let (args, applicability) = skills_applicability_under_restore(&RecordedInitArgs {
            no_skills: true,
            ..Default::default()
        });
        assert!(args.no_skills);
        assert_eq!(applicability, Some(false));
    }

    #[test]
    fn a_resume_that_inherits_a_skill_plan_leaves_the_skills_lane_alone() {
        let (args, applicability) = skills_applicability_under_restore(&RecordedInitArgs {
            skills_source: Some("github:example".to_owned()),
            skills: vec!["writing-plans".to_owned()],
            ..Default::default()
        });
        assert_eq!(args.skills_source.as_deref(), Some("github:example"));
        assert_eq!(args.skills, vec!["writing-plans".to_owned()]);
        assert_eq!(
            applicability, None,
            "a resume that still has skills to install must not retract the lane"
        );
    }

    #[test]
    fn registry_derivation_reports_a_pending_native_config_as_applicable() {
        let registry = RegistryCatalog::load_embedded().expect("registry");
        let args = parse_init_args(&[]);
        let signals = agent_settlement_signals(&config_for_agent("amp"), &registry, &args, true);
        assert_eq!(
            applicability_of(&signals, InitCategory::NativeConfig),
            Some(true)
        );
    }

    #[test]
    fn registry_derivation_reports_pending_dependencies_as_applicable() {
        let registry = RegistryCatalog::load_embedded().expect("registry");
        let args = parse_init_args(&[]);
        let mut config = config_for_agent("opencode");
        config.dependencies = crate::config::DependenciesConfig {
            commands: vec![crate::config::DependencyEntry {
                name: "acps-state-signal-absent-tool".to_owned(),
                required: true,
                feature: None,
                install: Some(crate::config::DependencyInstallAction {
                    shell: "true".to_owned(),
                    creates: Some("acps-state-signal-absent-tool".to_owned()),
                    scope: crate::config::DependencyInstallScope::User,
                    timeout_secs: None,
                }),
            }],
            ..Default::default()
        };
        let signals = agent_settlement_signals(&config, &registry, &args, false);
        assert_eq!(applicability_of(&signals, InitCategory::Deps), Some(true));
    }

    #[test]
    fn probe_rules_on_mcp_applicability() {
        // Spelled out rather than routed back through `applicability`: the rule
        // under test is that an applicable verdict carries no reason, and
        // reusing the helper the implementation uses would assert nothing.
        let advertised = capabilities_fixture(serde_json::json!({ "http": true }));
        assert_eq!(
            mcp_applicability_from_probe(&CapabilityProbeOutcome::Probed(advertised)),
            InitStateSignal::CategoryApplicability {
                category: InitCategory::Mcp,
                applicable: true,
                source: ApplicabilitySource::Probe,
                reason: None,
            },
        );

        let silent = capabilities_fixture(serde_json::json!({}));
        assert_eq!(
            mcp_applicability_from_probe(&CapabilityProbeOutcome::Probed(silent)),
            InitStateSignal::CategoryApplicability {
                category: InitCategory::Mcp,
                applicable: false,
                source: ApplicabilitySource::Probe,
                reason: Some("agent does not advertise MCP support".to_owned()),
            },
        );

        assert_eq!(
            mcp_applicability_from_probe(&CapabilityProbeOutcome::Unavailable {
                reason: "agent command `placebo` not found on PATH".to_owned(),
            }),
            InitStateSignal::CategoryApplicability {
                category: InitCategory::Mcp,
                applicable: false,
                source: ApplicabilitySource::Probe,
                reason: Some("agent command `placebo` not found on PATH".to_owned()),
            },
        );
    }

    fn config_without_mcp_servers() -> Config {
        let mut config = config_for_agent("opencode");
        config.mcp.servers.clear();
        config
    }

    fn config_with_mcp_servers(names: &[&str]) -> Config {
        let mut config = config_without_mcp_servers();
        config.mcp.servers = names
            .iter()
            .map(|name| {
                crate::config::McpServerConfig::Stdio(crate::config::McpStdioServer {
                    name: (*name).to_owned(),
                    command: format!("mcp-{name}"),
                    args: Vec::new(),
                    env: Vec::new(),
                })
            })
            .collect();
        config
    }

    fn ignored_mcp_server(name: &str) -> crate::runtime::agent::acp_bridge::IgnoredFeature {
        crate::runtime::agent::acp_bridge::IgnoredFeature {
            feature: crate::runtime::agent::acp_bridge::IGNORED_FEATURE_MCP_SERVER,
            target: name.to_owned(),
            capability: "mcpCapabilities.stdio",
            reason: "agent does not advertise this MCP transport".to_owned(),
        }
    }

    fn settled_mcp(value: &str) -> Option<InitStateSignal> {
        Some(InitStateSignal::CategorySettled {
            category: InitCategory::Mcp,
            value: Some(value.to_owned()),
        })
    }

    // The lane reports what the agent will actually be handed, which is why the
    // settlement reads the probe's own partition rather than the config list.
    #[test]
    fn probe_settles_mcp_with_the_servers_the_agent_will_be_given() {
        let config = config_with_mcp_servers(&["linear", "files"]);
        let advertised = capabilities_fixture(serde_json::json!({ "stdio": true }));
        assert_eq!(
            mcp_settlement_from_probe(&advertised, &config, &[]),
            settled_mcp("linear, files")
        );
        assert_eq!(
            mcp_settlement_from_probe(&advertised, &config, &[ignored_mcp_server("files")]),
            settled_mcp("linear"),
            "a server the agent cannot take must not be reported as configured"
        );
        assert_eq!(
            mcp_settlement_from_probe(
                &advertised,
                &config,
                &[ignored_mcp_server("linear"), ignored_mcp_server("files")]
            ),
            None,
            "nothing delivered leaves the lane for the applicability verdict"
        );
    }

    // Both ways the settlement declines: the agent takes no servers at all, and
    // the run declared none. The first is the case the probe-first ordering
    // exists for — the applicability verdict has to be the last word.
    #[test]
    fn probe_settles_no_mcp_without_support_or_declarations() {
        assert_eq!(
            mcp_settlement_from_probe(
                &capabilities_fixture(serde_json::json!({})),
                &config_with_mcp_servers(&["linear"]),
                &[],
            ),
            None
        );
        assert_eq!(
            mcp_settlement_from_probe(
                &capabilities_fixture(serde_json::json!({ "stdio": true })),
                &config_without_mcp_servers(),
                &[],
            ),
            None
        );
    }

    fn declared_stdio_servers() -> Vec<crate::config::McpServerConfig> {
        mcp_servers_from_prompted(
            &[InitMcpStdioServer {
                name: "files".to_owned(),
                command: "mcp-files".to_owned(),
                args: Vec::new(),
                env: Vec::new(),
            }],
            &[],
        )
        .expect("declared servers")
    }

    // The hosted lift's boundary: a session that declared nothing gets the
    // streamed picker, and one that declared its servers up front is left
    // alone — the wizard is skipped, not answered on the client's behalf.
    #[test]
    fn declared_mcp_servers_keep_a_hosted_run_out_of_the_wizard() {
        let args = parse_init_args(&[]);
        let mut config = config_without_mcp_servers();
        prompt::with_hosted_driver(Arc::new(RecordingDriver::default()), || {
            assert!(
                mcp_prompting_enabled(&args, true, &config),
                "a hosted session that declared no MCP servers gets the picker"
            );
            config.mcp.servers = declared_stdio_servers();
            assert!(
                !mcp_prompting_enabled(&args, true, &config),
                "declared servers skip prompting entirely"
            );
        });
    }

    // The lift only widened the hosted path: every other run reaches the
    // wizard exactly as before.
    #[test]
    fn mcp_prompting_stays_off_for_non_hosted_and_flag_driven_runs() {
        let config = config_without_mcp_servers();
        assert!(
            !mcp_prompting_enabled(&parse_init_args(&[]), true, &config),
            "no hosted driver and no terminal stdin under `cargo test`"
        );
        prompt::with_hosted_driver(Arc::new(RecordingDriver::default()), || {
            for flag in ["--non-interactive", "--handoff-json", "--resume"] {
                let args = parse_init_args(&[flag]);
                assert!(
                    !mcp_prompting_enabled(&args, true, &config),
                    "`{flag}` must keep the MCP wizard off"
                );
            }
            assert!(
                !mcp_prompting_enabled(&parse_init_args(&[]), false, &config),
                "an existing config is never re-prompted for MCP"
            );
        });
    }

    fn capabilities_fixture(
        mcp_capabilities: serde_json::Value,
    ) -> crate::runtime::agent::acp_bridge::AgentCapabilitiesDto {
        serde_json::from_value(serde_json::json!({
            "protocol_version": 1,
            "capabilities": { "mcpCapabilities": mcp_capabilities },
            "agent_name": "placebo",
            "agent_title": null,
            "agent_version": null,
        }))
        .expect("capabilities fixture")
    }

    /// The step kinds in the order `run_init_with_output` drives them. Call
    /// order is the authority on sequence, so this list is maintained against
    /// the call sites, never against the ordinals.
    const STEP_CALL_ORDER: [&str; 13] = [
        step_kind::SECRETS_INIT,
        step_kind::AGENT_INSTALL,
        step_kind::NATIVE_CONFIG_IMPORT,
        step_kind::AGENT_SKILLS_INSTALL,
        step_kind::WORKSPACE_MATERIALIZE,
        step_kind::DEPS_APPLY,
        step_kind::CAPABILITY_PROBE,
        step_kind::MCP_CONFIGURE,
        step_kind::PROVIDER_CONFIGURE,
        step_kind::AGENT_HEADLESS_CONFIG,
        step_kind::EDGE_ARTIFACTS,
        step_kind::INIT_COMPLETE,
        step_kind::TESTFLIGHT,
    ];

    fn test_store() -> (tempfile::TempDir, StateStore, crate::state::InitRunRecord) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = StateStore::open(dir.path().join("state.sqlite")).expect("store");
        store.migrate().expect("migrate");
        let run = crate::runtime::init_runner::begin_run(&store, None, None, "{}").expect("run");
        (dir, store, run)
    }

    /// Drives every step through its wrapper, including the pre-created
    /// log-dir variant workspace materialization uses.
    fn drive_all_steps(store: &StateStore, run: &crate::state::InitRunRecord) {
        for (index, kind) in STEP_CALL_ORDER.iter().enumerate() {
            let ordinal = index as i64 + 1;
            if *kind == step_kind::WORKSPACE_MATERIALIZE {
                record_init_step_with_default_log_dir(
                    store,
                    run,
                    ordinal,
                    kind,
                    Some("/tmp/acps-test-logs"),
                    || Ok(false),
                    || Ok(StepOutcome::empty()),
                )
                .expect("workspace step");
            } else {
                record_init_step(
                    store,
                    run,
                    ordinal,
                    kind,
                    || Ok(false),
                    || Ok(StepOutcome::empty()),
                )
                .expect("step");
            }
        }
    }

    #[test]
    fn step_wrappers_signal_started_and_finished_in_call_order() {
        let (_dir, store, run) = test_store();
        let driver = Arc::new(RecordingDriver::default());
        prompt::with_hosted_driver(driver.clone(), || drive_all_steps(&store, &run));

        let expected: Vec<InitStateSignal> = STEP_CALL_ORDER
            .iter()
            .flat_map(|kind| {
                [
                    InitStateSignal::StepStarted { kind },
                    InitStateSignal::StepFinished {
                        kind,
                        disposition: StepDisposition::Executed,
                        error_code: None,
                    },
                ]
            })
            .collect();
        assert_eq!(driver.recorded(), expected);
    }

    // A terminal run must not merely discard signals, it must never build
    // them: the derivations behind them walk the registry and the filesystem.
    #[test]
    fn signals_are_never_built_without_a_hosted_driver() {
        let built = std::cell::Cell::new(false);
        prompt::emit_state_signal(|| {
            built.set(true);
            InitStateSignal::StepStarted {
                kind: step_kind::AGENT_INSTALL,
            }
        });
        prompt::emit_state_signals(|| {
            built.set(true);
            Vec::new()
        });
        assert!(!built.get());
    }

    #[test]
    fn step_wrapper_carries_the_error_code_of_a_failed_step() {
        let (_dir, store, run) = test_store();
        let driver = Arc::new(RecordingDriver::default());
        let error = prompt::with_hosted_driver(driver.clone(), || {
            record_init_step(
                &store,
                &run,
                1,
                step_kind::AGENT_INSTALL,
                || Ok(false),
                || {
                    Err(StackError::AgentInitializeFailed {
                        reason: "synthetic".to_owned(),
                    })
                },
            )
            .expect_err("body error propagates")
        });
        assert_eq!(
            driver.recorded(),
            vec![
                InitStateSignal::StepStarted {
                    kind: step_kind::AGENT_INSTALL,
                },
                InitStateSignal::StepFinished {
                    kind: step_kind::AGENT_INSTALL,
                    disposition: StepDisposition::Executed,
                    error_code: Some(error.error_code().to_owned()),
                },
            ]
        );
    }

    #[test]
    fn step_wrapper_reports_a_verifier_skip_as_skipped() {
        let (_dir, store, run) = test_store();
        record_init_step(
            &store,
            &run,
            1,
            step_kind::AGENT_INSTALL,
            || Ok(false),
            || Ok(StepOutcome::empty()),
        )
        .expect("first pass");

        let driver = Arc::new(RecordingDriver::default());
        prompt::with_hosted_driver(driver.clone(), || {
            record_init_step(
                &store,
                &run,
                1,
                step_kind::AGENT_INSTALL,
                || Ok(true),
                || panic!("verified step must not re-run its body"),
            )
            .expect("resume")
        });
        assert_eq!(
            driver.recorded(),
            vec![
                InitStateSignal::StepStarted {
                    kind: step_kind::AGENT_INSTALL,
                },
                InitStateSignal::StepFinished {
                    kind: step_kind::AGENT_INSTALL,
                    disposition: StepDisposition::Skipped,
                    error_code: None,
                },
            ]
        );
    }

    #[test]
    fn installed_skill_names_omits_an_empty_list() {
        assert_eq!(installed_skill_names(&[]), None);
    }

    // The highest-blast-radius part of the mode lane: amp-class agents
    // (set_model=false) never spawned during init before it existed, so a
    // harness that cannot complete a provisional session must degrade to a
    // skipped lane rather than fail the run. The stub resolves and spawns, then
    // exits without speaking ACP — exactly the shape of that failure.
    #[cfg(unix)]
    #[test]
    fn a_mode_only_discovery_failure_skips_the_lane_instead_of_failing_init() {
        use std::os::unix::fs::PermissionsExt;

        let tempdir = tempfile::tempdir().expect("tempdir");
        let stub = tempdir.path().join("silent-agent");
        std::fs::write(&stub, "#!/bin/sh\nexit 0\n").expect("stub written");
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755))
            .expect("stub is executable");
        let registry = RegistryCatalog::load_embedded().expect("registry");
        let mut config = config_for_agent("amp");
        config.agent.command = stub.display().to_string();
        config.agent.args = Vec::new();
        config.agent.env = Vec::new();
        config.agent.install = None;
        config.agent.cwd = Some(tempdir.path().display().to_string());
        config.workspace.root = tempdir.path().display().to_string();
        let args = parse_init_args(&[]);
        let driver = Arc::new(RecordingDriver::default());

        let outcome = prompt::with_hosted_driver(driver.clone(), || {
            configure_model_and_mode_for_init(
                &args,
                tempdir.path(),
                &registry,
                &mut config,
                &tempdir.path().join("acps-config.toml"),
            )
        })
        .expect("a mode-only discovery failure must not fail init");

        assert_eq!(outcome.mode_action, ModelModeAction::Skipped);
        assert!(config.agent.mode.is_none());
        assert!(
            driver.recorded().iter().any(|signal| matches!(
                signal,
                InitStateSignal::CategoryApplicability {
                    category: InitCategory::Mode,
                    applicable: false,
                    // Not `Discovery`: the session never opened, so this reports
                    // that the check could not be made, which is not grounds for
                    // withdrawing a mode the config already holds.
                    source: ApplicabilitySource::DiscoveryUnavailable,
                    ..
                }
            )),
            "the skipped lane is reported, not silent: {:?}",
            driver.recorded()
        );
    }
}
