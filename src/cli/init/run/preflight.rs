use super::*;

/// Whether init should drive interactive prompts: a real TTY and no
/// prompt-suppressing automation flags. The single source of truth for the
/// gate, so every prompt site honors the same contract.
pub(super) fn prompts_enabled_for(args: &InitArgs, stdin_is_terminal: bool) -> bool {
    (stdin_is_terminal || prompt::hosted_driver_active())
        && !args.non_interactive
        && !args.handoff_json
}

pub(in crate::cli::init) fn prompts_enabled(args: &InitArgs) -> bool {
    prompts_enabled_for(args, io::stdin().is_terminal())
}

/// Whether the post-probe `mcp_configure` step drives its own prompts. Hosted
/// runs stream them like any other init prompt, so declaring MCP servers in the
/// start request is what keeps a hosted session out of the wizard: those
/// servers are already in `config.mcp.servers` by the time this is evaluated.
pub(super) fn mcp_prompting_enabled(
    args: &InitArgs,
    creating_config: bool,
    config: &Config,
) -> bool {
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

pub(super) fn prompt_config_source_if_needed(
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

pub(super) fn import_config_for_init(
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

pub(super) fn agent_install_progress_message(attempt: u32) -> String {
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
pub(in crate::cli::init) fn mcp_applicability_from_probe(
    outcome: &CapabilityProbeOutcome,
) -> InitStateSignal {
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
pub(in crate::cli::init) fn mcp_settlement_from_probe(
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
                    && feature.value == *name
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
pub(super) fn installed_skill_names(reports: &[SkillInstallReport]) -> Option<String> {
    let names = reports
        .iter()
        .flat_map(|report| report.installed.iter().map(|entry| entry.name.as_str()))
        .collect::<Vec<_>>();
    (!names.is_empty()).then(|| names.join(", "))
}

/// A reason is carried only for an inapplicable verdict: "why is this lane
/// missing" is the question a client asks, and an applicable lane answers it
/// by simply appearing.
pub(super) fn applicability(
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
pub(in crate::cli::init) fn agent_settlement_signals(
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
    // A custom agent has no registry entry, so init drives none of the
    // harness-configuration lanes for it: provider, model, mode, and effort go
    // through the agent's own environment, and skills have no known install dir.
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
    signals.push(registry_applicability(
        InitCategory::Effort,
        entry.is_some_and(|entry| entry.set_effort),
        entry.map_or(custom_reason, |_| "agent does not take a reasoning effort"),
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
    // These four rest on the disk rather than on anything this run did, so they
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
    if let Some(effort) = config.agent.effort.clone() {
        signals.push(InitStateSignal::CategoryProvisionallySettled {
            category: InitCategory::Effort,
            value: effort,
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
pub(super) fn restore_recorded_skill_plan(args: &mut InitArgs, recorded: &RecordedInitArgs) {
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
