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
        "../../../../tests/fixtures/valid-opencode-stack.toml"
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
fn registry_derivation_marks_kimi_provider_applicable_with_model() {
    let signals = settlement_signals_for("kimi");
    assert_eq!(
        applicability_of(&signals, InitCategory::Provider),
        Some(true),
        "kimi selects between the subscription and Moonshot platform lanes"
    );
    assert_eq!(applicability_of(&signals, InitCategory::Model), Some(true));
    assert_eq!(applicability_of(&signals, InitCategory::Mode), Some(true));
}

#[test]
fn registry_derivation_marks_kilo_provider_inapplicable_but_keeps_model() {
    let signals = settlement_signals_for("kilo");
    assert_eq!(
        applicability_of(&signals, InitCategory::Provider),
        Some(false),
        "kilo leaves provider selection to the harness env"
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
        value: name.to_owned(),
        capability: "mcpCapabilities.stdio",
        reason: "agent does not advertise this MCP transport".to_owned(),
        option_id: None,
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
            &SecretStore::open_or_create(tempdir.path()).expect("secret store"),
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

/// A hosted init writes a custom provider whose api-key ref arrives later
/// through a managed credential push. Until it lands the agent cannot
/// resolve its environment, so the lanes that would spawn it must gate on
/// the credential rather than on the spawn failing.
#[cfg(unix)]
fn pending_credential_fixture() -> (tempfile::TempDir, Config, RegistryCatalog, SecretStore) {
    use std::os::unix::fs::PermissionsExt;

    let tempdir = tempfile::tempdir().expect("tempdir");
    let stub = tempdir.path().join("stub-agent");
    std::fs::write(&stub, "#!/bin/sh\nexit 0\n").expect("stub written");
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755))
        .expect("stub is executable");
    let registry = RegistryCatalog::load_embedded().expect("registry");
    let mut config = config_for_agent("opencode");
    // Binary and cwd both resolve, so nothing but the pending credential
    // can be what stops the lane.
    config.agent.command = stub.display().to_string();
    config.agent.args = Vec::new();
    config.agent.env = vec!["PENDING_CUSTOM_KEY".to_owned()];
    config.agent.install = None;
    config.agent.cwd = Some(tempdir.path().display().to_string());
    config.workspace.root = tempdir.path().display().to_string();
    config.agent.provider = Some(config::AgentProviderConfig {
        id: "my-custom".to_owned(),
        model: Some("my-model".to_owned()),
        api_key_ref: Some("PENDING_CUSTOM_KEY".to_owned()),
        custom: Some(config::AgentCustomProviderConfig {
            name: "My Custom".to_owned(),
            base_url: "https://example.test/v1".to_owned(),
            api: config::CustomProviderApi::default(),
            context: 128_000,
            output_max_tokens: 8_192,
            model_name: None,
        }),
    });
    let secrets = SecretStore::open_or_create(tempdir.path()).expect("secret store");
    (tempdir, config, registry, secrets)
}

#[cfg(unix)]
#[test]
fn a_pending_provider_credential_skips_discovery_instead_of_spawning() {
    let (tempdir, mut config, registry, secrets) = pending_credential_fixture();
    let args = parse_init_args(&[]);
    let driver = Arc::new(RecordingDriver::default());

    let outcome = prompt::with_hosted_driver(driver.clone(), || {
        configure_model_and_mode_for_init(
            &args,
            tempdir.path(),
            &registry,
            &mut config,
            &tempdir.path().join("acps-config.toml"),
            &secrets,
        )
    })
    .expect("a deferred credential must not fail init");

    assert_eq!(outcome.model_action, ModelModeAction::Skipped);
    assert_eq!(outcome.mode_action, ModelModeAction::Skipped);
    assert!(config.agent.mode.is_none());
}

#[cfg(unix)]
#[test]
fn an_explicit_mode_fails_early_when_the_provider_credential_is_pending() {
    let (tempdir, mut config, registry, secrets) = pending_credential_fixture();
    let args = parse_init_args(&["--mode", "build"]);

    let error = configure_model_and_mode_for_init(
        &args,
        tempdir.path(),
        &registry,
        &mut config,
        &tempdir.path().join("acps-config.toml"),
        &secrets,
    )
    .expect_err("an explicit --mode cannot be validated without a credential");
    let message = error.to_string();
    assert!(message.contains("--mode"), "{message}");
    assert!(message.contains("PENDING_CUSTOM_KEY"), "{message}");
    assert!(message.contains("managed-state extension"), "{message}");
}

#[cfg(unix)]
#[test]
fn testflight_skips_or_errors_on_a_pending_provider_credential() {
    let (_tempdir, config, registry, secrets) = pending_credential_fixture();

    let decision = resolve_testflight_decision(&parse_init_args(&[]), &config, &registry, &secrets)
        .expect("implicit run resolves a decision");
    assert!(matches!(
        decision,
        Some(TestflightDecision::SkipCredentialPending { .. })
    ));

    let error = resolve_testflight_decision(
        &parse_init_args(&["--testflight"]),
        &config,
        &registry,
        &secrets,
    )
    .expect_err("an explicit --testflight cannot run without a credential");
    assert!(error.to_string().contains("PENDING_CUSTOM_KEY"));
}

/// Answers every prompt with one scripted confirm answer, so the testflight
/// resolver sees a hosted client and the deferral flag under test.
struct ScriptedConfirmDriver(prompt::ConfirmAnswer);

impl prompt::HostedPromptDriver for ScriptedConfirmDriver {
    fn select(
        &self,
        _request: prompt::HostedPromptRequest,
    ) -> Result<prompt::HostedPromptOutcome<Option<usize>>> {
        Ok(prompt::HostedPromptOutcome::Unhandled)
    }

    fn confirm(
        &self,
        _request: prompt::HostedPromptRequest,
    ) -> Result<prompt::HostedPromptOutcome<bool>> {
        Ok(prompt::HostedPromptOutcome::Handled(self.0.value))
    }

    fn confirm_with_deferral(
        &self,
        _request: prompt::HostedPromptRequest,
    ) -> Result<prompt::HostedPromptOutcome<prompt::ConfirmAnswer>> {
        Ok(prompt::HostedPromptOutcome::Handled(self.0))
    }

    fn text(
        &self,
        _request: prompt::HostedPromptRequest,
    ) -> Result<prompt::HostedPromptOutcome<Option<String>>> {
        Ok(prompt::HostedPromptOutcome::Unhandled)
    }

    fn password(
        &self,
        _request: prompt::HostedPromptRequest,
    ) -> Result<prompt::HostedPromptOutcome<Option<String>>> {
        Ok(prompt::HostedPromptOutcome::Unhandled)
    }

    fn progress(&self, _message: String) {}

    fn result(&self, _payload: serde_json::Value) {}
}

#[test]
fn a_deferred_testflight_answer_is_not_a_decline() {
    let registry = RegistryCatalog::load_embedded().expect("registry");
    let config = config_for_agent("opencode");
    let tempdir = tempfile::tempdir().expect("tempdir");
    let secrets = SecretStore::open_or_create(tempdir.path()).expect("secret store");
    let args = parse_init_args(&[]);
    let resolve = |answer: prompt::ConfirmAnswer| {
        prompt::with_hosted_driver(Arc::new(ScriptedConfirmDriver(answer)), || {
            resolve_testflight_decision(&args, &config, &registry, &secrets)
        })
        .expect("hosted answer resolves a decision")
    };

    assert_eq!(
        resolve(prompt::ConfirmAnswer::plain(true)),
        Some(TestflightDecision::Run)
    );
    assert_eq!(
        resolve(prompt::ConfirmAnswer::plain(false)),
        Some(TestflightDecision::SkipDeclined)
    );
    assert_eq!(
        resolve(prompt::ConfirmAnswer {
            value: false,
            deferred: true
        }),
        Some(TestflightDecision::SkipDeferred)
    );
    // An accepted answer is a run whatever the flag says: the backend defers by
    // declining, so a `true` that also claims deferral would run it twice.
    assert_eq!(
        resolve(prompt::ConfirmAnswer {
            value: true,
            deferred: true
        }),
        Some(TestflightDecision::Run)
    );
}

#[test]
fn testflight_decisions_report_their_own_finalize_line() {
    assert_eq!(TestflightDecision::Run.skip_message(), None);
    assert_eq!(
        TestflightDecision::SkipDeferred.skip_message().as_deref(),
        Some("testflight: deferred (runs after setup)")
    );
    assert_eq!(
        TestflightDecision::SkipDeclined.skip_message().as_deref(),
        Some("testflight: skipped (declined at prompt)")
    );
}
