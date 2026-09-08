use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Duration;

use agent_client_protocol::schema::v1::NewSessionResponse;

use crate::cli::agent::{agent_model_is_explicit_without_discovery, model_values_for_cli_display};
use crate::config::{AgentConfigOptionValue, Config};
use crate::dev_gates::{
    FIXTURE_CONFIG_OPTIONS_ENV, FIXTURE_NEW_SESSION_RESPONSE_ENV, TEST_SKIP_AGENT_INSTALL_ENV,
};
use crate::error::{Result, StackError};
use crate::runtime::agent::acp_bridge::AgentSessionConfigCategory;
use crate::runtime::agent::acp_bridge::{KIMI_CODE_AGENT_ID, kimi_lane_for_provider_id};
use crate::runtime::agent::agent_headless_config::{
    HERMES_AGENT_ID, provision_agent_headless_config,
};
use crate::runtime::agent::config_options::{SessionConfigOptionSnapshot, project_config_options};
use crate::runtime::agent::model_discovery::{
    DiscoveredSessionConfig, advertised_values_for_category, catalog_effort_values,
    catalog_model_values, discovery_is_blocked_without_a_model,
    effort_value_is_explicit_without_discovery, fetch_session_config,
    resolve_advertised_model_value, session_new_requires_a_configured_model,
    validate_advertised_value, validate_catalog_effort_value,
};
use crate::runtime::agent::provider_keys::{CODEX_AGENT_ID, agent_provider_id_for_provider_id};
use crate::runtime::agent::provider_model_catalog::cached_models;
use crate::runtime::install::agent_registry::RegistryCatalog;
use crate::secrets::{SharedSecretStore, lock_shared_secret_store};

use super::headless_snapshot::{
    capture_dir_listings_for, capture_path_snapshots, headless_config_candidate_paths,
    headless_config_side_dirs, remove_new_files_in_dirs, restore_headless_snapshots,
};
use super::prompt::{
    DiscoveryRevision, DiscoveryWait, HostedPromptKind, RevisableOutcome, RevisedAnswer,
    SKIP_OPTION_ID,
};
use super::provider::{
    pending_deferred_provider_credential, pending_provider_credential_reason,
    primary_provider_is_custom,
};
use super::registry_apply::is_custom_agent;
use super::state_signal::{ApplicabilitySource, InitCategory, InitStateSignal};
use super::{InitArgs, prompt, prompts_enabled};

// CONSTANTS — discovery pickers and probe retry.

/// Display label of the skip choice, which is also the label a hosted answer may
/// address it by.
const SKIP_OPTION_LABEL: &str = "Skip";
/// Total tries for one discovery probe, the first included.
const DISCOVERY_PROBE_ATTEMPTS: u32 = 3;
const DISCOVERY_PROBE_RETRY_BASE_DELAY: Duration = Duration::from_secs(2);
const DISCOVERY_PROBE_RETRY_MAX_DELAY: Duration = Duration::from_secs(8);
const DISCOVERY_PROBE_RETRY_MAX_EXPONENT: u32 = 2;

/// Outcome of one init session-config selection lane.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) enum ModelModeAction {
    #[default]
    Skipped,
    Set,
    PrintedList,
}

#[derive(Debug, Default, Clone, Copy)]
pub(super) struct ModelModeOutcome {
    pub(super) model_action: ModelModeAction,
    pub(super) mode_action: ModelModeAction,
    pub(super) effort_action: ModelModeAction,
    pub(super) config_options_changed: bool,
    pub(super) acp_verified: bool,
}

/// Capability gates for all three lanes, evaluated before any side effects.
pub(super) fn preflight_model_and_mode_for_init(
    args: &InitArgs,
    registry: &RegistryCatalog,
    config: &Config,
    config_path: &Path,
) -> Result<()> {
    // Clap already rejects `--custom-agent-id` with these flags; this covers a
    // re-init over an existing custom-agent config, which passes no such flag.
    if is_custom_agent(config, registry) {
        if args.model.is_some() {
            return Err(StackError::InvalidParam {
                field: "--model",
                reason: "custom agents configure models through their own environment; `--model` applies only to supported registry agents".to_owned(),
            });
        }
        if args.mode.is_some() {
            return Err(StackError::InvalidParam {
                field: "--mode",
                reason: "custom agents configure modes through their own environment; `--mode` applies only to supported registry agents".to_owned(),
            });
        }
        if args.effort.is_some() {
            return Err(StackError::InvalidParam {
                field: "--effort",
                reason: "custom agents configure reasoning effort through their own environment; `--effort` applies only to supported registry agents".to_owned(),
            });
        }
    }
    let Some(entry) = registry.lookup(&config.agent.id) else {
        return Ok(());
    };
    if args.model.is_some() && !entry.set_model {
        return Err(StackError::AgentConfigProvision {
            path: config_path.to_path_buf(),
            reason: format!(
                "{} does not support model configuration through `acps init`",
                entry.name,
            ),
        });
    }
    if args.mode.is_some() && !entry.set_mode {
        return Err(StackError::AgentConfigProvision {
            path: config_path.to_path_buf(),
            reason: format!(
                "{} does not support mode configuration through `acps init`",
                entry.name,
            ),
        });
    }
    if args.effort.is_some() && !entry.set_effort {
        return Err(StackError::AgentConfigProvision {
            path: config_path.to_path_buf(),
            reason: format!(
                "{} does not support reasoning-effort configuration through `acps init`",
                entry.name,
            ),
        });
    }
    // Provider-backed agents keep the model inside `[agent.provider]`, so
    // `--model` must be paired with `--provider` rather than silently writing
    // the root slot or pairing with a stale provider block.
    let provider_missing =
        entry.set_provider && args.provider.is_none() && config.agent.provider.is_none();
    if args.model.is_some() && provider_missing {
        return Err(StackError::InvalidParam {
            field: "model",
            reason: format!(
                "{} stores the model inside [agent.provider]; pass --provider <id> together with --model, or run `acps agent set` after init",
                entry.name,
            ),
        });
    }
    // The advertised mode list comes from a provisional session, which a
    // provider-backed harness with no provider cannot be launched to produce.
    if args.mode.is_some() && provider_missing {
        return Err(StackError::InvalidParam {
            field: "mode",
            reason: format!(
                "{} needs a configured provider before its modes can be discovered; pass --provider <id> together with --mode, or run `acps agent set` after init",
                entry.name,
            ),
        });
    }
    if args.effort.is_some() && provider_missing {
        return Err(StackError::InvalidParam {
            field: "effort",
            reason: format!(
                "{} needs a configured provider before its reasoning-effort values can be discovered; pass --provider <id> together with --effort, or run `acps agent set` after init",
                entry.name,
            ),
        });
    }
    // A harness that resolves its model while starting a session cannot open the
    // provisional session the mode and effort lanes read until one is configured.
    if args.model.is_none() && discovery_is_blocked_without_a_model(&config.agent) {
        for (field, requested) in [
            ("mode", args.mode.is_some()),
            ("effort", args.effort.is_some()),
        ] {
            if requested {
                return Err(StackError::InvalidParam {
                    field,
                    reason: format!(
                        "{} needs a configured model before its {field} values can be discovered; pass --model <id> together with --{field}, or run `acps agent set` after init",
                        entry.name,
                    ),
                });
            }
        }
    }
    Ok(())
}

/// Drives the model, mode, and effort ACP-discovery flows during `acps init`.
pub(super) fn configure_model_and_mode_for_init(
    args: &InitArgs,
    home: &Path,
    registry: &RegistryCatalog,
    config: &mut Config,
    config_path: &Path,
    secrets: &SharedSecretStore,
) -> Result<ModelModeOutcome> {
    preflight_model_and_mode_for_init(args, registry, config, config_path)?;
    let entry = registry.lookup(&config.agent.id);
    let set_model = entry.is_some_and(|entry| entry.set_model);
    let set_mode = entry.is_some_and(|entry| entry.set_mode);
    let set_effort = entry.is_some_and(|entry| entry.set_effort);
    let set_provider = entry.is_some_and(|entry| entry.set_provider);
    let default_mode = entry.and_then(|entry| entry.default_mode.as_deref());
    let agent_name = entry
        .map(|entry| entry.name.as_str())
        .unwrap_or(config.agent.name.as_str())
        .to_owned();
    let interactive = prompts_enabled(args);
    if !set_model && !set_mode && !set_effort && !interactive {
        return Ok(ModelModeOutcome::default());
    }
    let mut outcome = ModelModeOutcome::default();
    // Kimi cannot initialize its ACP process without a model, so the model
    // lane MUST settle here before the mode lane may spawn the harness. A lane
    // without a seeded default falls through to the provider-catalog lane.
    let mut model_lane_resolved = false;
    if set_model
        && config.agent.id == KIMI_CODE_AGENT_ID
        && args.model.is_none()
        && config.agent.provider.is_some()
    {
        let model_settled = config.agent.model.is_some()
            || config
                .agent
                .provider
                .as_ref()
                .is_some_and(|provider| provider.model.is_some());
        let lane_default = config
            .agent
            .provider
            .as_ref()
            .filter(|provider| provider.custom.is_none())
            .and_then(|provider| kimi_lane_for_provider_id(Some(&provider.id)))
            .and_then(|lane| lane.default_model);
        if model_settled {
            model_lane_resolved = true;
        } else if let Some(default_model) = lane_default {
            write_model_into_config(config, default_model.to_owned(), set_provider);
            outcome.model_action = ModelModeAction::Set;
            model_lane_resolved = true;
        }
    }
    // A custom-provider model id is not an ACP-advertised value, so the model
    // lane is skipped; mode is provider-independent and still runs.
    let skip_model_lane = primary_provider_is_custom(config);
    // Discovery is skipped, but an explicit `--model` still has to land or a
    // rerun over an existing custom-provider config would drop the flag.
    if skip_model_lane
        && set_model
        && let Some(model) = args.model.as_deref()
    {
        write_model_into_config(
            config,
            validated_explicit_model(model, &agent_name)?,
            set_provider,
        );
        outcome.model_action = ModelModeAction::Set;
    }

    let provider_set_this_run = args.provider.is_some();
    // Without a provider the picker would write root `agent.model`, which the
    // supervisor prefers and the provider-backed ownership contract forbids.
    let provider_present =
        provider_set_this_run || config.agent.provider.is_some() || !set_provider;
    let explicit_model_without_discovery = args.model.is_some()
        && !args.custom_provider
        && agent_model_is_explicit_without_discovery(config);
    let mut model_lane_active = set_model
        && !skip_model_lane
        && !model_lane_resolved
        && provider_present
        && (args.model.is_some() || interactive || provider_set_this_run);
    if model_lane_active
        && explicit_model_without_discovery
        && let Some(model) = args.model.as_deref()
    {
        write_model_into_config(
            config,
            validated_explicit_model(model, &agent_name)?,
            set_provider,
        );
        outcome.model_action = ModelModeAction::Set;
        model_lane_active = false;
    }
    // No print-the-list fallback here, so an unattended run without
    // `--mode`/`--effort` never spawns the harness at all.
    let mode_lane_active = set_mode
        && provider_present
        && (args.mode.is_some() || interactive || default_mode.is_some());
    let effort_lane_active =
        set_effort && provider_present && (args.effort.is_some() || interactive);
    let generic_lane_active = interactive && provider_present;
    if !model_lane_active && !mode_lane_active && !effort_lane_active && !generic_lane_active {
        return Ok(outcome);
    }
    let live_flags: Vec<&str> = [
        (model_lane_active && args.model.is_some()).then_some("--model"),
        (mode_lane_active && args.mode.is_some()).then_some("--mode"),
        (effort_lane_active && args.effort.is_some()).then_some("--effort"),
    ]
    .into_iter()
    .flatten()
    .collect();
    let explicit_flags = match live_flags.as_slice() {
        [] => None,
        flags => Some(flags.join(" and ")),
    };
    let explicit_flags = explicit_flags.as_deref();

    let fixture_discovery = std::env::var_os(FIXTURE_CONFIG_OPTIONS_ENV).is_some()
        || std::env::var_os(FIXTURE_NEW_SESSION_RESPONSE_ENV).is_some();

    // A hosted init may still be awaiting a managed credential push, so the
    // spawn would fail on a state that is pending by design. Checked before the
    // binary/cwd preconditions so the attribution names the credential. The
    // read locks briefly and releases: the discovery spawn below must never run
    // with the store lock held, since a deposit needs that same lock to land.
    if !fixture_discovery
        && let Some((provider_id, api_key_ref)) =
            pending_deferred_provider_credential(config, &lock_shared_secret_store(secrets))
    {
        let reason = pending_provider_credential_reason(&provider_id, &api_key_ref);
        if let Some(flags) = explicit_flags {
            // With `defer_provider_credentials` the missing credential is
            // expected, so explicit values land unvalidated and a wrong one
            // surfaces at the first real session instead.
            if prompt::defer_provider_credentials() {
                if model_lane_active && let Some(model) = args.model.as_deref() {
                    write_model_into_config(config, model.to_owned(), set_provider);
                    outcome.model_action = ModelModeAction::Set;
                }
                if mode_lane_active && let Some(mode) = args.mode.as_deref() {
                    write_mode_into_config(config, mode.to_owned());
                    outcome.mode_action = ModelModeAction::Set;
                }
                if effort_lane_active && let Some(effort) = args.effort.as_deref() {
                    write_effort_into_config(config, effort.to_owned());
                    outcome.effort_action = ModelModeAction::Set;
                }
                init_progress(
                    args,
                    &format!("{flags} accepted without discovery validation: {reason}"),
                );
                return Ok(outcome);
            }
            let error = StackError::AgentConfigProvision {
                path: config_path.to_path_buf(),
                reason: format!("cannot validate {flags} for {agent_name}: {reason}"),
            };
            signal_lane_failure(
                model_lane_active,
                mode_lane_active,
                effort_lane_active,
                &error,
            );
            return Err(error);
        }
        init_progress(
            args,
            &format!(
                "{} discovery skipped: {reason}",
                active_lane_label(
                    model_lane_active,
                    mode_lane_active,
                    effort_lane_active,
                    generic_lane_active,
                )
            ),
        );
        return Ok(outcome);
    }

    // Preconditions mirror the spawn: the command must resolve on PATH, and the
    // cwd must exist and be selected exactly as `fetch_session_config` selects
    // it (`agent.cwd` over `workspace.root`) or the preflight can pass on a
    // directory the spawn never visits.
    let spawn_cwd: PathBuf = config
        .agent
        .cwd
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(&config.workspace.root));
    let binary_missing = !fixture_discovery
        && crate::runtime::agent::acp_bridge::resolve_command_path(
            &config.agent.command,
            &spawn_cwd,
            home,
        )
        .is_none();
    let cwd_missing = !fixture_discovery && !spawn_cwd.is_dir();
    if !fixture_discovery && (binary_missing || cwd_missing) {
        if let Some(flags) = explicit_flags {
            let reason = match (binary_missing, cwd_missing) {
                (true, true) => format!(
                    "agent command `{}` is not on PATH and spawn cwd `{}` does not exist",
                    config.agent.command,
                    spawn_cwd.display(),
                ),
                (true, false) => {
                    format!("agent command `{}` is not on PATH", config.agent.command,)
                }
                (false, true) => format!(
                    "spawn cwd `{}` does not exist; create it or run `acps workspace sync` first",
                    spawn_cwd.display(),
                ),
                (false, false) => unreachable!(),
            };
            let error = StackError::AgentConfigProvision {
                path: config_path.to_path_buf(),
                reason: format!("cannot validate {flags} for {agent_name}: {reason}"),
            };
            signal_lane_failure(
                model_lane_active,
                mode_lane_active,
                effort_lane_active,
                &error,
            );
            return Err(error);
        }
        let lanes = active_lane_label(
            model_lane_active,
            mode_lane_active,
            effort_lane_active,
            generic_lane_active,
        );
        let skip_reason = if binary_missing {
            format!(
                "{lanes} discovery skipped: agent command `{}` not found on PATH",
                config.agent.command,
            )
        } else {
            format!(
                "{lanes} discovery skipped: spawn cwd `{}` is not yet provisioned",
                spawn_cwd.display(),
            )
        };
        init_progress(args, &skip_reason);
        return Ok(outcome);
    }

    // Provisioning the headless config makes the spawned harness see the NEW
    // provider, whose advertised model list can differ. Snapshotting every
    // candidate file's prior contents BEFORE that runs is what keeps the
    // "rejection writes nothing" guarantee: a discovery or validation failure
    // rolls back to true prior state.
    let candidate_paths = headless_config_candidate_paths(&config.agent.id, home);
    let snapshots = capture_path_snapshots(&candidate_paths)?;
    // Directory listings let rollback also remove side files the provisioners
    // write out-of-band under operator-supplied names `candidate_paths` cannot
    // enumerate.
    let mut dir_scan = candidate_paths
        .iter()
        .filter_map(|path| path.parent().map(Path::to_path_buf))
        .collect::<Vec<_>>();
    dir_scan.extend(headless_config_side_dirs(&config.agent.id, home));
    let dir_listings = capture_dir_listings_for(&dir_scan)?;
    let discovery_outcome = (|| {
        // The pre-spawn catalog lane is part of the phase, so the flag is armed
        // before it, not before the loop.
        reset_discovery_prompt_issued();
        crate::runtime::agent::provider_model_catalog::refresh_provider_models_best_effort_blocking(
            home, config,
        );
        // A harness that resolves its model while starting a session cannot advertise one, so its
        // lane reads the provider catalog and must settle before anything spawns it.
        let mut model_lane_active = model_lane_active;
        if model_lane_active && session_new_requires_a_configured_model(&config.agent) {
            outcome.model_action = configure_model_from_catalog_for_init(
                args,
                home,
                config,
                &agent_name,
                set_provider,
            )
            .inspect_err(|error| signal_lane_failure(true, false, false, error))?;
            model_lane_active = false;
        }
        // Both skip arms below owe the client a close: the catalog model lane may
        // already have streamed a prompt, which opens the phase session-side, and
        // returning past it would strand the phase open. A model revision at that
        // wait re-enters the probe from the top instead.
        let discovered = 'probe: loop {
            if discovery_is_blocked_without_a_model(&config.agent) {
                let reason = format!(
                    "{} discovery skipped: {agent_name} needs a configured model before a session can be opened",
                    active_lane_label(
                        false,
                        mode_lane_active,
                        effort_lane_active,
                        generic_lane_active
                    )
                );
                init_progress(args, &reason);
                match await_phase_close_or_model_revision(
                    args,
                    home,
                    config,
                    &mut outcome,
                    set_provider,
                )? {
                    PhaseExit::Revised => continue 'probe,
                    PhaseExit::Closed => {
                        withdraw_undiscovered_lanes(mode_lane_active, effort_lane_active, &reason);
                        return Ok(outcome);
                    }
                }
            }
            crate::runtime::agent::agent_headless_config::provision_agent_headless_config(
                config, home,
            )
            .inspect_err(|error| {
                signal_lane_failure(
                    model_lane_active,
                    mode_lane_active,
                    effort_lane_active,
                    error,
                )
            })?;
            let probe_model = configured_model_value(config);
            match fetch_session_config_with_retry(
                home,
                config,
                probe_model.as_deref(),
                "model discovery",
                &|message| init_progress(args, message),
            ) {
                Ok(discovered) => break 'probe discovered,
                // A mode/effort-only lane with no explicit flag is pure enrichment,
                // so a harness that cannot complete a provisional session must not
                // fail an otherwise good init.
                Err(error)
                    if !model_lane_active && args.mode.is_none() && args.effort.is_none() =>
                {
                    let reason = format!(
                        "{} discovery skipped: {error}",
                        active_lane_label(
                            false,
                            mode_lane_active,
                            effort_lane_active,
                            generic_lane_active
                        )
                    );
                    init_progress(args, &reason);
                    match await_phase_close_or_model_revision(
                        args,
                        home,
                        config,
                        &mut outcome,
                        set_provider,
                    )? {
                        PhaseExit::Revised => continue 'probe,
                        PhaseExit::Closed => {
                            withdraw_undiscovered_lanes(
                                mode_lane_active,
                                effort_lane_active,
                                &reason,
                            );
                            return Ok(outcome);
                        }
                    }
                }
                Err(error) => {
                    signal_lane_failure(
                        model_lane_active,
                        mode_lane_active,
                        effort_lane_active,
                        &error,
                    );
                    return Err(error);
                }
            }
        };
        // A harness that pins the effort on disk advertises none over ACP, so
        // the empty advertisement is expected there, not a registry correction.
        let catalog_effort_lane = effort_value_is_explicit_without_discovery(&config.agent);
        // The picker lists every model `session/new` offered; a post-set option list may carry
        // only the applied one.
        let model_options = discovered.response.clone();
        emit_discovery_applicability_corrections(
            &model_options,
            set_model,
            set_mode,
            set_effort && !catalog_effort_lane,
        );
        let mut phase = DiscoveryPhase {
            args,
            home,
            config_path,
            agent_name: &agent_name,
            set_provider,
            default_mode,
            interactive,
            model_options,
            response: discovered.applied(),
            model_lane_active,
            mode_lane_active,
            effort_lane_active,
            effort_lane_live: effort_lane_active,
            generic_prompts_live: interactive,
            downstream_withdrawn: false,
            config_options_before: config.agent.config_options.clone(),
        };
        let mut queue: VecDeque<Lane> = if phase.model_lane_active {
            VecDeque::from([Lane::Model])
        } else {
            phase.downstream_queue()
        };
        loop {
            while let Some(lane) = queue.pop_front() {
                if let Some(revision) =
                    phase.run_lane(lane.clone(), config, &mut outcome, &mut queue)?
                {
                    // The abandoned prompt is re-issued unless the revision it
                    // yielded to settles that same lane.
                    queue.push_front(lane);
                    phase.apply_revision(revision, config, &mut outcome, &mut queue)?;
                }
            }
            // A run that asked nothing opened no phase, so no client can close one.
            if !prompt::hosted_driver_active() || !discovery_prompt_issued() {
                break;
            }
            match prompt::await_discovery_close()? {
                DiscoveryWait::Closed => break,
                DiscoveryWait::Revised(revision) => {
                    phase.apply_revision(revision, config, &mut outcome, &mut queue)?;
                }
            }
        }
        outcome.acp_verified = true;
        // A phase-level diff, so a revision that restores a pre-phase value
        // reports no change at all.
        outcome.config_options_changed = config.agent.config_options != phase.config_options_before;
        if catalog_effort_lane && outcome.effort_action == ModelModeAction::Set {
            // The pin reaches the harness through its own config file.
            provision_agent_headless_config(config, home)
                .inspect_err(|error| signal_lane_failure(false, false, true, error))?;
        }
        Ok::<ModelModeOutcome, StackError>(outcome)
    })();

    match discovery_outcome {
        Ok(outcome) => Ok(outcome),
        Err(err) => {
            restore_headless_snapshots(snapshots);
            remove_new_files_in_dirs(dir_listings);
            Err(err)
        }
    }
}

/// The mode and effort lanes never got an advertisement to read, so the ones the
/// registry claimed are withdrawn with the reason that stopped discovery.
fn withdraw_undiscovered_lanes(mode_lane_active: bool, effort_lane_active: bool, reason: &str) {
    prompt::emit_state_signals(|| {
        [
            (mode_lane_active, InitCategory::Mode),
            (effort_lane_active, InitCategory::Effort),
        ]
        .into_iter()
        .filter(|(live, _)| *live)
        .map(|(_, category)| InitStateSignal::CategoryApplicability {
            category,
            applicable: false,
            source: ApplicabilitySource::DiscoveryUnavailable,
            reason: Some(reason.to_owned()),
        })
        .collect()
    });
}

/// How a pre-advertisement close wait ended.
enum PhaseExit {
    Closed,
    /// The client re-answered the model, so discovery is worth another try.
    Revised,
}

/// Park for the client's close signal when discovery gives up before any
/// advertisement exists. Without a hosted client, or with no prompt yet issued,
/// there is no phase and this returns at once.
fn await_phase_close_or_model_revision(
    args: &InitArgs,
    home: &Path,
    config: &mut Config,
    outcome: &mut ModelModeOutcome,
    set_provider: bool,
) -> Result<PhaseExit> {
    if !prompt::hosted_driver_active() || !discovery_prompt_issued() {
        return Ok(PhaseExit::Closed);
    }
    loop {
        match prompt::await_discovery_close()? {
            DiscoveryWait::Closed => return Ok(PhaseExit::Closed),
            DiscoveryWait::Revised(revision) => {
                if apply_catalog_model_revision(
                    args,
                    home,
                    config,
                    outcome,
                    set_provider,
                    &revision,
                ) {
                    return Ok(PhaseExit::Revised);
                }
            }
        }
    }
}

/// Apply a model re-answer made before any advertisement exists. Only the
/// pre-spawn catalog lane can have been recorded at this point, so the provider
/// catalog is the value source. Returns whether discovery is worth retrying.
fn apply_catalog_model_revision(
    args: &InitArgs,
    home: &Path,
    config: &mut Config,
    outcome: &mut ModelModeOutcome,
    set_provider: bool,
    revision: &DiscoveryRevision,
) -> bool {
    let withdraw = |reason: &str| {
        init_progress(
            args,
            &format!(
                "`{}` revision not applied: {reason}",
                revision.kind.as_str()
            ),
        );
    };
    if revision.kind != HostedPromptKind::Model {
        withdraw("only the model lane is answerable before discovery opens");
        return false;
    }
    let values = match catalog_model_values(home, config) {
        Ok(values) => values,
        Err(error) => {
            withdraw(&error.to_string());
            return false;
        }
    };
    let selected = match revised_selection(&revision.answer, &values) {
        Ok(selected) => selected,
        Err(error) => {
            withdraw(&error.to_string());
            return false;
        }
    };
    let Some(selected) = selected else {
        // Still no model, so discovery stays blocked and the wait resumes.
        clear_model_from_config(config);
        outcome.model_action = ModelModeAction::Skipped;
        return false;
    };
    write_model_into_config(config, selected.to_owned(), set_provider);
    outcome.model_action = ModelModeAction::Set;
    true
}

/// One question the discovery phase can put to a client. A generic config option
/// is addressed by its advertised id, since its snapshot is re-read from the
/// response current when the lane runs.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Lane {
    Model,
    Mode,
    Effort,
    ConfigOption(String),
}

/// The discovery phase's own state: the two advertisements the lanes read, the
/// lane liveness flags, and the pre-phase override map every close diffs against.
struct DiscoveryPhase<'a> {
    args: &'a InitArgs,
    home: &'a Path,
    config_path: &'a Path,
    agent_name: &'a str,
    set_provider: bool,
    default_mode: Option<&'a str>,
    interactive: bool,
    /// The `session/new` advertisement before any model was applied: the model
    /// picker's value source, which no re-probe refreshes.
    model_options: NewSessionResponse,
    /// The advertisement the mode, effort, and generic lanes read, replaced by
    /// every successful re-probe.
    response: NewSessionResponse,
    model_lane_active: bool,
    mode_lane_active: bool,
    effort_lane_active: bool,
    effort_lane_live: bool,
    generic_prompts_live: bool,
    /// Set when a failed re-probe withdrew the downstream lanes, so a repeated
    /// model answer still re-probes instead of leaving them withdrawn forever.
    downstream_withdrawn: bool,
    config_options_before: BTreeMap<String, AgentConfigOptionValue>,
}

impl DiscoveryPhase<'_> {
    /// The lanes that follow the model, in the order the forward pass asks them.
    /// Generic options are enumerated from the advertisement current at the call,
    /// so a model change decides which options are prompted at all.
    fn downstream_queue(&self) -> VecDeque<Lane> {
        let mut queue = VecDeque::new();
        // Mode, effort, and the generic options all read the model's own
        // advertisement, so a re-probe that left `self.response` stale withdraws
        // them together rather than re-asking from the previous model's list.
        if self.mode_lane_active && !self.downstream_withdrawn {
            queue.push_back(Lane::Mode);
        }
        if self.effort_lane_live {
            queue.push_back(Lane::Effort);
        }
        if self.generic_prompts_live {
            for option in self.generic_options() {
                queue.push_back(Lane::ConfigOption(option.id));
            }
        }
        queue
    }

    fn generic_options(&self) -> Vec<SessionConfigOptionSnapshot> {
        project_config_options(self.response.config_options.as_deref().unwrap_or_default())
            .into_iter()
            .filter(|option| !typed_lane_owns(option))
            .collect()
    }

    fn advertised_generic_option(&self, id: &str) -> Option<SessionConfigOptionSnapshot> {
        self.generic_options()
            .into_iter()
            .find(|option| option.id == id)
    }

    /// Ask one lane. `Some(revision)` means the client re-answered an earlier
    /// prompt instead of this one, which the caller applies before re-asking.
    fn run_lane(
        &mut self,
        lane: Lane,
        config: &mut Config,
        outcome: &mut ModelModeOutcome,
        queue: &mut VecDeque<Lane>,
    ) -> Result<Option<DiscoveryRevision>> {
        match lane {
            Lane::Model => {
                let model_before = configured_model_value(config);
                let answered = configure_model_lane(
                    self.args,
                    self.home,
                    config,
                    self.config_path,
                    &self.model_options,
                    self.agent_name,
                    self.set_provider,
                    true,
                )
                .inspect_err(|error| signal_lane_failure(true, false, false, error))?;
                match answered {
                    RevisableOutcome::Revised(revision) => return Ok(Some(revision)),
                    RevisableOutcome::Answered(action) => outcome.model_action = action,
                }
                // Adapters advertise effort levels (and some generic options)
                // for the model they read from disk, so a changed model needs a
                // fresh advertisement before those lanes read it.
                if configured_model_value(config) != model_before {
                    self.refetch_after_model_change(config)?;
                }
                *queue = self.downstream_queue();
            }
            Lane::Mode => {
                let answered = configure_mode_lane(
                    self.args,
                    config,
                    self.config_path,
                    &self.response,
                    self.interactive,
                    self.default_mode,
                    true,
                )
                .inspect_err(|error| signal_lane_failure(false, true, false, error))?;
                match answered {
                    RevisableOutcome::Revised(revision) => return Ok(Some(revision)),
                    RevisableOutcome::Answered(action) => outcome.mode_action = action,
                }
            }
            Lane::Effort => {
                let answered = configure_effort_lane(
                    self.args,
                    self.home,
                    config,
                    self.config_path,
                    &self.response,
                    self.interactive,
                    true,
                )
                .inspect_err(|error| signal_lane_failure(false, false, true, error))?;
                match answered {
                    RevisableOutcome::Revised(revision) => return Ok(Some(revision)),
                    RevisableOutcome::Answered(action) => outcome.effort_action = action,
                }
            }
            Lane::ConfigOption(id) => {
                // A re-probe may have dropped the option between queueing and
                // asking, which leaves nothing to ask about.
                let Some(option) = self.advertised_generic_option(&id) else {
                    return Ok(None);
                };
                match apply_generic_config_option(config, option, self.generic_prompts_live, true)?
                {
                    RevisableOutcome::Revised(revision) => return Ok(Some(revision)),
                    RevisableOutcome::Answered(()) => {}
                }
            }
        }
        Ok(None)
    }

    /// Re-provision and re-probe after the model changed. Extracted so the
    /// forward model lane and a model revision take the identical path.
    fn refetch_after_model_change(&mut self, config: &mut Config) -> Result<()> {
        if !(self.effort_lane_live || self.generic_prompts_live) {
            return Ok(());
        }
        let args = self.args;
        let home = self.home;
        let effort_lane_live = self.effort_lane_live;
        provision_agent_headless_config(config, home)
            .inspect_err(|error| signal_lane_failure(false, false, effort_lane_live, error))?;
        // The model just chosen, so the refreshed effort list belongs to it.
        let chosen_model = configured_model_value(config);
        let probed = fetch_session_config_with_retry(
            home,
            config,
            chosen_model.as_deref(),
            "model discovery",
            &|message| init_progress(args, message),
        );
        match probed {
            Ok(refreshed) => {
                self.response = refreshed.applied();
                self.downstream_withdrawn = false;
            }
            Err(error) if args.effort.is_none() => {
                let reason = format!(
                    "{} rediscovery after the model change skipped: {error}",
                    active_lane_label(false, false, effort_lane_live, self.generic_prompts_live)
                );
                init_progress(args, &reason);
                if effort_lane_live {
                    prompt::emit_state_signal(|| InitStateSignal::CategoryApplicability {
                        category: InitCategory::Effort,
                        applicable: false,
                        source: ApplicabilitySource::DiscoveryUnavailable,
                        reason: Some(reason.clone()),
                    });
                }
                self.effort_lane_live = false;
                self.generic_prompts_live = false;
                self.downstream_withdrawn = true;
            }
            Err(error) => {
                signal_lane_failure(false, false, true, &error);
                return Err(error);
            }
        }
        Ok(())
    }

    /// Apply one accepted revision. A value this wizard can still prove wrong
    /// withdraws its lane and leaves the earlier answer standing; only a re-probe
    /// an explicit flag depends on may fail the run.
    fn apply_revision(
        &mut self,
        revision: DiscoveryRevision,
        config: &mut Config,
        outcome: &mut ModelModeOutcome,
        queue: &mut VecDeque<Lane>,
    ) -> Result<()> {
        match revision.kind {
            HostedPromptKind::Model => self.apply_model_revision(&revision, config, outcome, queue),
            HostedPromptKind::Mode => {
                self.apply_mode_revision(&revision, config, outcome, queue);
                Ok(())
            }
            HostedPromptKind::Effort => {
                self.apply_effort_revision(&revision, config, outcome, queue);
                Ok(())
            }
            HostedPromptKind::ConfigOption => {
                self.apply_config_option_revision(&revision, config, queue);
                Ok(())
            }
            _ => {
                self.withdraw_revision(&revision, "that prompt is not part of the discovery phase");
                Ok(())
            }
        }
    }

    fn apply_model_revision(
        &mut self,
        revision: &DiscoveryRevision,
        config: &mut Config,
        outcome: &mut ModelModeOutcome,
        queue: &mut VecDeque<Lane>,
    ) -> Result<()> {
        let previous = configured_model_value(config);
        // A harness that cannot advertise models over a provisional session
        // picked from the provider catalog, so a re-answer resolves there too.
        let values = if session_new_requires_a_configured_model(&config.agent) {
            match catalog_model_values(self.home, config) {
                Ok(values) => values,
                Err(error) => {
                    self.withdraw_revision(revision, &error.to_string());
                    return Ok(());
                }
            }
        } else {
            match model_picker_values(self.home, config, &self.model_options) {
                Some(values) => values,
                None => {
                    self.withdraw_revision(
                        revision,
                        "no model values are available to validate against",
                    );
                    return Ok(());
                }
            }
        };
        let resolved = match revised_selection(&revision.answer, &values) {
            Ok(resolved) => resolved,
            Err(error) => {
                self.withdraw_revision(revision, &error.to_string());
                return Ok(());
            }
        };
        match resolved {
            None => {
                clear_model_from_config(config);
                outcome.model_action = ModelModeAction::Skipped;
            }
            Some(model) => {
                write_model_into_config(config, model.to_owned(), self.set_provider);
                outcome.model_action = ModelModeAction::Set;
            }
        }
        queue.retain(|lane| *lane != Lane::Model);
        // A repeated answer still re-probes while the downstream lanes are
        // withdrawn, which is how a client recovers from an exhausted retry.
        if configured_model_value(config) == previous && !self.downstream_withdrawn {
            return Ok(());
        }
        self.clear_downstream_selections(config, outcome);
        prompt::supersede_discovery_lanes(&[
            HostedPromptKind::Mode,
            HostedPromptKind::Effort,
            HostedPromptKind::ConfigOption,
        ]);
        self.effort_lane_live = self.effort_lane_active;
        self.generic_prompts_live = self.interactive;
        self.refetch_after_model_change(config)?;
        *queue = self.downstream_queue();
        Ok(())
    }

    fn apply_mode_revision(
        &mut self,
        revision: &DiscoveryRevision,
        config: &mut Config,
        outcome: &mut ModelModeOutcome,
        queue: &mut VecDeque<Lane>,
    ) {
        let values =
            advertised_values_for_category(&self.response, AgentSessionConfigCategory::Mode)
                .unwrap_or_default();
        let resolved = match revised_selection(&revision.answer, &values) {
            Ok(resolved) => resolved,
            Err(error) => return self.withdraw_revision(revision, &error.to_string()),
        };
        match resolved {
            None => {
                prompt::emit_state_signal(|| InitStateSignal::CategorySettled {
                    category: InitCategory::Mode,
                    value: None,
                });
                config.agent.mode = None;
                outcome.mode_action = ModelModeAction::Skipped;
            }
            Some(mode) => {
                write_mode_into_config(config, mode.to_owned());
                outcome.mode_action = ModelModeAction::Set;
            }
        }
        queue.retain(|lane| *lane != Lane::Mode);
    }

    fn apply_effort_revision(
        &mut self,
        revision: &DiscoveryRevision,
        config: &mut Config,
        outcome: &mut ModelModeOutcome,
        queue: &mut VecDeque<Lane>,
    ) {
        // A harness that pins the effort on disk advertises none over ACP, so
        // its values come from the provider catalog.
        let values = if effort_value_is_explicit_without_discovery(&config.agent) {
            match catalog_effort_values(self.home, config) {
                Ok(values) => values,
                Err(error) => return self.withdraw_revision(revision, &error.to_string()),
            }
        } else {
            advertised_values_for_category(&self.response, AgentSessionConfigCategory::Effort)
                .unwrap_or_default()
        };
        let resolved = match revised_selection(&revision.answer, &values) {
            Ok(resolved) => resolved,
            Err(error) => return self.withdraw_revision(revision, &error.to_string()),
        };
        match resolved {
            None => {
                prompt::emit_state_signal(|| InitStateSignal::CategorySettled {
                    category: InitCategory::Effort,
                    value: None,
                });
                config.agent.effort = None;
                outcome.effort_action = ModelModeAction::Skipped;
            }
            Some(effort) => {
                write_effort_into_config(config, effort.to_owned());
                outcome.effort_action = ModelModeAction::Set;
            }
        }
        queue.retain(|lane| *lane != Lane::Effort);
    }

    fn apply_config_option_revision(
        &mut self,
        revision: &DiscoveryRevision,
        config: &mut Config,
        queue: &mut VecDeque<Lane>,
    ) {
        let Some(config_id) = revision.config_id.clone() else {
            return self.withdraw_revision(revision, "the revision names no config option");
        };
        let Some(option) = self.advertised_generic_option(&config_id) else {
            return self.withdraw_revision(
                revision,
                &format!("`{config_id}` is not an advertised config option"),
            );
        };
        let RevisedAnswer::ConfigOption(resolved) = &revision.answer else {
            return self.withdraw_revision(
                revision,
                "a config-option lane needs a config-option answer",
            );
        };
        // The advertised option is still consulted, because a re-probe may have
        // narrowed its choices since the prompt the session validated against.
        if let Some(AgentConfigOptionValue::Text(selected)) = resolved
            && option.kind == crate::runtime::agent::config_options::SNAPSHOT_KIND_SELECT
            && !option
                .options
                .as_deref()
                .unwrap_or_default()
                .iter()
                .any(|choice| choice.value == *selected)
        {
            return self.withdraw_revision(
                revision,
                &format!("`{config_id}` no longer advertises `{selected}`"),
            );
        }
        let mut candidate = config.agent.config_options.clone();
        match resolved.clone() {
            // Dropping the override hands the option back to the agent's default.
            None => {
                candidate.remove(&config_id);
            }
            Some(value) => {
                candidate.insert(config_id.clone(), value);
            }
        }
        if let Err(error) = crate::config::validate_agent_config_options(&candidate) {
            return self.withdraw_revision(revision, &error.to_string());
        }
        config.agent.config_options = candidate;
        queue.retain(|lane| !matches!(lane, Lane::ConfigOption(id) if *id == config_id));
    }

    /// Drop everything the previous model decided, so a re-probe that fails
    /// cannot leave the old model's effort or overrides on screen.
    fn clear_downstream_selections(&self, config: &mut Config, outcome: &mut ModelModeOutcome) {
        // The reference fold refuses to withdraw an already-settled category, so
        // effort is settled to nothing through the path the fold does accept. A
        // model that advertised no effort never opened that category client-side,
        // so re-settling it there would invent a lane the client never saw.
        let effort_was_advertised =
            advertised_values_for_category(&self.response, AgentSessionConfigCategory::Effort)
                .map(|values| !values.is_empty())
                .unwrap_or(false);
        if effort_was_advertised {
            prompt::emit_state_signal(|| InitStateSignal::CategorySettled {
                category: InitCategory::Effort,
                value: None,
            });
        }
        config.agent.effort = None;
        outcome.effort_action = ModelModeAction::Skipped;
        // Only keys this advertisement carried; an override an operator brought
        // in from a resumed config or a native import is not the model's to drop.
        for option in self.generic_options() {
            config.agent.config_options.remove(&option.id);
        }
    }

    fn withdraw_revision(&self, revision: &DiscoveryRevision, reason: &str) {
        init_progress(
            self.args,
            &format!(
                "`{}` revision not applied: {reason}",
                revision.kind.as_str()
            ),
        );
    }
}

/// Ask one generic config option and persist an explicit selection. The
/// per-option half of the old whole-map pass, so the phase loop can re-ask a
/// single option without replaying the rest.
fn apply_generic_config_option(
    config: &mut Config,
    option: SessionConfigOptionSnapshot,
    interactive: bool,
    revisable: bool,
) -> Result<RevisableOutcome<()>> {
    let id = option.id.clone();
    mark_discovery_prompt_issued();
    let answered = if revisable {
        prompt::config_option_revisable(option, interactive)?
    } else {
        RevisableOutcome::Answered(prompt::config_option(option, interactive)?)
    };
    let value = match answered {
        RevisableOutcome::Revised(revision) => return Ok(RevisableOutcome::Revised(revision)),
        RevisableOutcome::Answered(value) => value,
    };
    if let Some(value) = value
        && config.agent.config_options.get(&id) != Some(&value)
    {
        let mut candidate = config.agent.config_options.clone();
        candidate.insert(id, value);
        crate::config::validate_agent_config_options(&candidate)?;
        config.agent.config_options = candidate;
    }
    Ok(RevisableOutcome::Answered(()))
}

/// Forward pass over every generic config option, kept as the direct entry point
/// the lane's own tests drive.
#[cfg(test)]
fn configure_generic_config_options_for_init(
    config: &mut Config,
    response: &NewSessionResponse,
    interactive: bool,
) -> Result<bool> {
    let before = config.agent.config_options.clone();
    let advertised = project_config_options(response.config_options.as_deref().unwrap_or_default());
    for option in advertised
        .into_iter()
        .filter(|option| !typed_lane_owns(option))
    {
        match apply_generic_config_option(config, option, interactive, false)? {
            RevisableOutcome::Answered(()) => {}
            RevisableOutcome::Revised(revision) => return Err(unexpected_revision(&revision)),
        }
    }
    Ok(config.agent.config_options != before)
}

thread_local! {
    /// Whether a discovery picker actually streamed to a hosted client. The
    /// wizard blocks for a close signal only when a phase exists to close, and
    /// the prompt helpers are free functions with no phase to write into.
    static DISCOVERY_PROMPT_ISSUED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn reset_discovery_prompt_issued() {
    DISCOVERY_PROMPT_ISSUED.with(|issued| issued.set(false));
}

fn mark_discovery_prompt_issued() {
    if prompt::hosted_driver_active() {
        DISCOVERY_PROMPT_ISSUED.with(|issued| issued.set(true));
    }
}

fn discovery_prompt_issued() -> bool {
    DISCOVERY_PROMPT_ISSUED.with(std::cell::Cell::get)
}

/// A revision reached a call that cannot service one. Unreachable by
/// construction, reported rather than panicked so the run still settles.
#[cfg(test)]
fn unexpected_revision(revision: &DiscoveryRevision) -> StackError {
    StackError::InvalidParam {
        field: "init",
        reason: format!(
            "a discovery revision of `{}` reached a non-revisable lane",
            revision.kind.as_str()
        ),
    }
}

/// The `(id, label)` pairs a typed picker offers, in prompt order. Shared with
/// the revision resolver so a client may address a re-answer exactly as it
/// addressed the first one.
fn selection_prompt_options(values: &[String]) -> Vec<(String, String)> {
    // Option ids are answerable over the wire, so a duplicate advertised value
    // would make one unreachable and one colliding with the Skip sentinel would
    // leave the operator no way out of the prompt.
    let mut seen = std::collections::BTreeSet::new();
    let mut options: Vec<(String, String)> = values
        .iter()
        .filter(|value| value.as_str() != SKIP_OPTION_ID)
        .filter(|value| seen.insert(value.as_str()))
        .map(|value| (value.clone(), value.clone()))
        .collect();
    options.push((SKIP_OPTION_ID.to_owned(), SKIP_OPTION_LABEL.to_owned()));
    options
}

/// Match a re-answered option id against the values its lane offers now. The
/// session already validated the id against the prompt that offered it, so a
/// miss here means a re-probe moved the option set underneath the client.
fn revised_selection<'a>(answer: &'a RevisedAnswer, values: &[String]) -> Result<Option<&'a str>> {
    let RevisedAnswer::Select(selected) = answer else {
        return Err(StackError::InvalidParam {
            field: "init",
            reason: "a model, mode, or effort lane needs a select answer".to_owned(),
        });
    };
    let Some(selected) = selected.as_deref() else {
        return Ok(None);
    };
    if !values.iter().any(|value| value == selected) {
        return Err(StackError::InvalidParam {
            field: "init",
            reason: format!("`{selected}` is no longer offered by this lane"),
        });
    }
    Ok(Some(selected))
}

/// One discovery probe with bounded exponential-backoff retry. A provisional
/// session is a process spawn against a harness that may still be settling, so a
/// single refusal is not yet evidence that a lane has to be withdrawn.
fn fetch_session_config_with_retry(
    home: &Path,
    config: &Config,
    model: Option<&str>,
    label: &str,
    report: &dyn Fn(&str),
) -> Result<DiscoveredSessionConfig> {
    retry_discovery_probe(
        |_| fetch_session_config(home, config, model),
        |attempt, error, delay| {
            tracing::warn!(
                agent = %config.agent.id,
                attempt,
                %error,
                "{label} probe failed; retrying"
            );
            report(&format!(
                "{label} attempt {attempt} of {DISCOVERY_PROBE_ATTEMPTS} failed: {error}; retrying in {}s",
                delay.as_secs()
            ));
            // Fixture-driven probes never spawn anything, so a real sleep would
            // only slow the suite down.
            if !discovery_fixture_active() {
                std::thread::sleep(delay);
            }
        },
    )
}

/// Whether a failed probe is worth spawning the harness again for. Only the
/// spawn, transport, and timeout shapes are, so an allow-list rather than a
/// deny-list: a credential the agent rejects or a config it cannot parse fails
/// identically every time, and waiting on it would cost every real init the full
/// backoff budget.
fn discovery_failure_is_retryable(error: &StackError) -> bool {
    matches!(
        error,
        // Carries the discovery timeout as well as a probe that died mid-handshake.
        StackError::AgentInitializeFailed { .. }
            | StackError::AgentSpawnFailed { .. }
            | StackError::AgentRequestFailed { .. }
            | StackError::AgentNotRunning
            | StackError::InferenceRequestFailed { .. }
            | StackError::ServeIo { .. }
    )
}

/// The retry loop itself, generic over its closures so the schedule is testable
/// without a harness or a real sleep.
fn retry_discovery_probe<T>(
    mut attempt_probe: impl FnMut(u32) -> Result<T>,
    mut on_retry: impl FnMut(u32, &StackError, Duration),
) -> Result<T> {
    let mut attempt = 1u32;
    loop {
        match attempt_probe(attempt) {
            Ok(probed) => return Ok(probed),
            Err(error) => {
                if attempt >= DISCOVERY_PROBE_ATTEMPTS || !discovery_failure_is_retryable(&error) {
                    return Err(error);
                }
                let delay = crate::time_util::exponential_backoff_delay(
                    attempt,
                    DISCOVERY_PROBE_RETRY_BASE_DELAY,
                    DISCOVERY_PROBE_RETRY_MAX_DELAY,
                    DISCOVERY_PROBE_RETRY_MAX_EXPONENT,
                );
                on_retry(attempt, &error, delay);
                attempt += 1;
            }
        }
    }
}

fn discovery_fixture_active() -> bool {
    crate::dev_gates::fixture_enabled(FIXTURE_CONFIG_OPTIONS_ENV)
        || crate::dev_gates::fixture_enabled(FIXTURE_NEW_SESSION_RESPONSE_ENV)
}

fn typed_lane_owns(option: &SessionConfigOptionSnapshot) -> bool {
    option.kind == crate::runtime::agent::config_options::SNAPSHOT_KIND_SELECT
        && (matches!(
            option.category.as_deref(),
            Some("model" | "mode" | "thought_level")
        ) || [
            AgentSessionConfigCategory::Model,
            AgentSessionConfigCategory::Mode,
            AgentSessionConfigCategory::Effort,
        ]
        .into_iter()
        .any(|category| category.matches_id(&option.id)))
}

fn active_lane_label(
    model_lane_active: bool,
    mode_lane_active: bool,
    effort_lane_active: bool,
    generic_lane_active: bool,
) -> String {
    let lanes: Vec<&str> = [
        model_lane_active.then_some("model"),
        mode_lane_active.then_some("mode"),
        effort_lane_active.then_some("effort"),
        generic_lane_active.then_some("config-option"),
    ]
    .into_iter()
    .flatten()
    .collect();
    match lanes.as_slice() {
        [] => "session config".to_owned(),
        lanes => lanes.join(" and "),
    }
}

/// Shared precondition check before spawning the configured agent from init.
pub(super) enum AgentSpawnPreflight {
    Ready,
    Fixture,
    CwdMissing(PathBuf),
    BinaryMissing,
}

pub(super) fn agent_spawn_preflight(
    home: &Path,
    config: &Config,
    fixture_envs: &[&str],
) -> AgentSpawnPreflight {
    // `fixture_enabled` rather than a raw env read: in a build without
    // `test-fixtures` a stray fixture var must not skip the preflight while the
    // consumer ignores the fixture and really spawns.
    if fixture_envs
        .iter()
        .any(|name| crate::dev_gates::fixture_enabled(name))
    {
        return AgentSpawnPreflight::Fixture;
    }
    let spawn_cwd: PathBuf = config
        .agent
        .cwd
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(&config.workspace.root));
    if !spawn_cwd.is_dir() {
        return AgentSpawnPreflight::CwdMissing(spawn_cwd);
    }
    if crate::runtime::agent::acp_bridge::resolve_command_path(
        &config.agent.command,
        &spawn_cwd,
        home,
    )
    .is_none()
    {
        return AgentSpawnPreflight::BinaryMissing;
    }
    AgentSpawnPreflight::Ready
}

/// Connection gate for agents that do not run model discovery: confirm the
/// configured agent launches and completes an ACP session.
pub(super) fn verify_agent_acp_connection(
    home: &Path,
    config: &Config,
    print_progress: bool,
) -> Result<()> {
    match agent_spawn_preflight(
        home,
        config,
        &[FIXTURE_CONFIG_OPTIONS_ENV, FIXTURE_NEW_SESSION_RESPONSE_ENV],
    ) {
        AgentSpawnPreflight::Ready | AgentSpawnPreflight::Fixture => {}
        AgentSpawnPreflight::CwdMissing(spawn_cwd) => {
            if print_progress {
                println!(
                    "acp connection check skipped: spawn cwd `{}` is not yet provisioned",
                    spawn_cwd.display(),
                );
            }
            return Ok(());
        }
        AgentSpawnPreflight::BinaryMissing => {
            if crate::dev_gates::fixture_enabled(TEST_SKIP_AGENT_INSTALL_ENV) {
                if print_progress {
                    println!(
                        "acp connection check skipped: agent command `{}` not found on PATH",
                        config.agent.command,
                    );
                }
                return Ok(());
            }
            return Err(StackError::AgentInitializeFailed {
                reason: format!(
                    "agent command `{}` did not resolve after custom agent install",
                    config.agent.command,
                ),
            });
        }
    }
    fetch_session_config_with_retry(
        home,
        config,
        configured_model_value(config).as_deref(),
        "acp connection",
        &|message| {
            if prompt::hosted_driver_active() {
                prompt::emit_progress(message.to_owned());
            } else if print_progress {
                println!("{message}");
            }
        },
    )
    .map(|_| ())
    .map_err(|error| StackError::AgentInitializeFailed {
        reason: format!(
            "agent `{}` failed to complete an ACP session during init: {error}",
            config.agent.command,
        ),
    })
}

/// Handshake-only capability probe for the `capability_probe` init step; never
/// fails, so an unavailable probe simply makes no capability claims.
pub(super) enum CapabilityProbeOutcome {
    // Boxed to keep the variants balanced for clippy's `large_enum_variant`.
    Probed(Box<crate::runtime::agent::acp_bridge::AgentCapabilitiesDto>),
    Unavailable { reason: String },
}

pub(super) fn probe_agent_capabilities_for_init(
    home: &Path,
    config: &Config,
) -> CapabilityProbeOutcome {
    match agent_spawn_preflight(
        home,
        config,
        &[crate::dev_gates::FIXTURE_AGENT_CAPABILITIES_ENV],
    ) {
        AgentSpawnPreflight::Ready | AgentSpawnPreflight::Fixture => {}
        AgentSpawnPreflight::CwdMissing(spawn_cwd) => {
            return CapabilityProbeOutcome::Unavailable {
                reason: format!("spawn cwd `{}` is not provisioned", spawn_cwd.display()),
            };
        }
        AgentSpawnPreflight::BinaryMissing => {
            return CapabilityProbeOutcome::Unavailable {
                reason: format!("agent command `{}` not found on PATH", config.agent.command),
            };
        }
    }
    match crate::runtime::agent::model_discovery::fetch_agent_capabilities(home, config) {
        Ok(capabilities) => CapabilityProbeOutcome::Probed(Box::new(capabilities)),
        Err(error) => {
            tracing::warn!(
                agent = %config.agent.id,
                %error,
                "capability probe failed; continuing without capability evidence"
            );
            CapabilityProbeOutcome::Unavailable {
                reason: format!("capability probe failed: {error}"),
            }
        }
    }
}

/// Forward model lane, kept as the direct entry point the lane's own tests drive.
#[cfg(test)]
fn configure_model_for_init(
    args: &InitArgs,
    home: &Path,
    config: &mut Config,
    config_path: &Path,
    response: &NewSessionResponse,
    agent_name: &str,
    provider_backed: bool,
) -> Result<ModelModeAction> {
    match configure_model_lane(
        args,
        home,
        config,
        config_path,
        response,
        agent_name,
        provider_backed,
        false,
    )? {
        RevisableOutcome::Answered(action) => Ok(action),
        RevisableOutcome::Revised(revision) => Err(unexpected_revision(&revision)),
    }
}

/// The values the model picker offers, or `None` when a catalog-substituted lane
/// has no catalog to read.
fn model_picker_values(
    home: &Path,
    config: &Config,
    response: &NewSessionResponse,
) -> Option<Vec<String>> {
    // codex-acp advertises codex-core's bundled OpenAI preset catalog whatever
    // the configured provider, and hermes-agent-acp advertises composite
    // `provider/model` ids rather than the bare ids config.yaml pins, so
    // neither list is a truthful pickable set; substitute the provider's live
    // catalog instead.
    let provider_catalog_lane = agent_model_is_explicit_without_discovery(config)
        && (config.agent.id == CODEX_AGENT_ID || config.agent.id == HERMES_AGENT_ID);
    if !provider_catalog_lane {
        return Some(model_values_for_cli_display(
            config,
            advertised_values_for_category(response, AgentSessionConfigCategory::Model)
                .unwrap_or_default(),
        ));
    }
    let catalog = config
        .agent
        .provider
        .as_ref()
        .filter(|provider| provider.custom.is_none())
        .and_then(|provider| cached_models(home, &provider.id))?;
    Some(catalog.into_iter().map(|model| model.value).collect())
}

#[allow(clippy::too_many_arguments)]
fn configure_model_lane(
    args: &InitArgs,
    home: &Path,
    config: &mut Config,
    config_path: &Path,
    response: &NewSessionResponse,
    agent_name: &str,
    provider_backed: bool,
    revisable: bool,
) -> Result<RevisableOutcome<ModelModeAction>> {
    if let Some(explicit) = args.model.as_deref() {
        if agent_model_is_explicit_without_discovery(config) {
            write_model_into_config(
                config,
                validated_explicit_model(explicit, agent_name)?,
                provider_backed,
            );
            return Ok(RevisableOutcome::Answered(ModelModeAction::Set));
        }
        let agent_provider_id = provider_backed
            .then(|| {
                config.agent.provider.as_ref().and_then(|provider| {
                    agent_provider_id_for_provider_id(&config.agent.id, &provider.id)
                })
            })
            .flatten();
        let model = resolve_advertised_model_value(response, agent_provider_id, explicit).map_err(
            |err| {
                let advertised =
                    advertised_values_for_category(response, AgentSessionConfigCategory::Model)
                        .unwrap_or_default();
                StackError::AgentConfigProvision {
                    path: config_path.to_path_buf(),
                    reason: format!("{err}; advertised models: [{}]", advertised.join(", "),),
                }
            },
        )?;
        write_model_into_config(config, model, provider_backed);
        return Ok(RevisableOutcome::Answered(ModelModeAction::Set));
    }

    let provider_catalog_lane = agent_model_is_explicit_without_discovery(config)
        && (config.agent.id == CODEX_AGENT_ID || config.agent.id == HERMES_AGENT_ID);
    let Some(values) = model_picker_values(home, config, response) else {
        if !args.handoff_json {
            println!(
                "no live model catalog available for {agent_name}; \
                 rerun with `acps init --model <value>` to write a model into config"
            );
        }
        return Ok(RevisableOutcome::Answered(ModelModeAction::Skipped));
    };
    if values.is_empty() {
        return Ok(RevisableOutcome::Answered(ModelModeAction::Skipped));
    }
    let interactive = prompts_enabled(args);
    if !interactive {
        // Print the advertised values and leave config untouched, so the agent
        // still picks its own default on session/new.
        if !args.handoff_json {
            if provider_catalog_lane {
                println!("provider catalog models for {agent_name}:");
            } else {
                println!("advertised models for {agent_name}:");
            }
            for value in &values {
                println!("  {value}");
            }
            println!("rerun with `acps init --model <value>` to write a model into config");
        }
        return Ok(RevisableOutcome::Answered(ModelModeAction::PrintedList));
    }

    let selected = match prompt_session_config_selection(
        HostedPromptKind::Model,
        interactive,
        &values,
        AgentSessionConfigCategory::Model,
        revisable,
    )? {
        RevisableOutcome::Revised(revision) => return Ok(RevisableOutcome::Revised(revision)),
        RevisableOutcome::Answered(None) => {
            return Ok(RevisableOutcome::Answered(ModelModeAction::Skipped));
        }
        RevisableOutcome::Answered(Some(selected)) => selected,
    };
    if !agent_model_is_explicit_without_discovery(config) {
        validate_advertised_value(response, AgentSessionConfigCategory::Model, &selected)?;
    }
    write_model_into_config(config, selected, provider_backed);
    Ok(RevisableOutcome::Answered(ModelModeAction::Set))
}

/// An explicit model that skips advertisement validation is still a value the harness has to
/// resolve, so an empty one is rejected instead of pinned.
fn validated_explicit_model(model: &str, agent_name: &str) -> Result<String> {
    let trimmed = model.trim();
    if trimmed.is_empty() {
        return Err(StackError::InvalidParam {
            field: "model",
            reason: format!("{agent_name} needs a non-empty model id"),
        });
    }
    Ok(trimmed.to_owned())
}

/// Model lane for a harness that cannot advertise models over a provisional session: the values
/// come from the provider catalog, and a provider that publishes no catalog leaves `--model` as
/// the only lane.
fn configure_model_from_catalog_for_init(
    args: &InitArgs,
    home: &Path,
    config: &mut Config,
    agent_name: &str,
    provider_backed: bool,
) -> Result<ModelModeAction> {
    let values = match catalog_model_values(home, config) {
        Ok(values) => values,
        Err(error) => {
            init_progress(
                args,
                &format!(
                    "model discovery skipped: {error}; rerun with `acps init --model <value>` to write a model into config"
                ),
            );
            return Ok(ModelModeAction::Skipped);
        }
    };
    if !prompts_enabled(args) {
        // Print the catalog and leave config untouched; the operator names the
        // model on a rerun.
        if !args.handoff_json {
            println!("provider catalog models for {agent_name}:");
            for value in &values {
                println!("  {value}");
            }
            println!("rerun with `acps init --model <value>` to write a model into config");
        }
        return Ok(ModelModeAction::PrintedList);
    }
    let selection = prompt_session_config_selection(
        HostedPromptKind::Model,
        true,
        &values,
        AgentSessionConfigCategory::Model,
        true,
    )?;
    let selected = match selection {
        RevisableOutcome::Answered(Some(selected)) => selected,
        RevisableOutcome::Answered(None) => return Ok(ModelModeAction::Skipped),
        // This is the first discovery prompt of the run, so no earlier answer
        // exists to revise; a stray one leaves the lane unset.
        RevisableOutcome::Revised(revision) => {
            init_progress(
                args,
                &format!(
                    "`{}` revision arrived before any answer was accepted",
                    revision.kind.as_str()
                ),
            );
            return Ok(ModelModeAction::Skipped);
        }
    };
    write_model_into_config(config, selected, provider_backed);
    Ok(ModelModeAction::Set)
}

/// Forward mode lane, kept as the direct entry point the lane's own tests drive.
#[cfg(test)]
fn configure_mode_for_init(
    args: &InitArgs,
    config: &mut Config,
    config_path: &Path,
    response: &NewSessionResponse,
    interactive: bool,
    default_mode: Option<&str>,
) -> Result<ModelModeAction> {
    match configure_mode_lane(
        args,
        config,
        config_path,
        response,
        interactive,
        default_mode,
        false,
    )? {
        RevisableOutcome::Answered(action) => Ok(action),
        RevisableOutcome::Revised(revision) => Err(unexpected_revision(&revision)),
    }
}

/// Mode counterpart to `configure_model_lane`, sharing the caller's one
/// provisional session.
fn configure_mode_lane(
    args: &InitArgs,
    config: &mut Config,
    config_path: &Path,
    response: &NewSessionResponse,
    interactive: bool,
    default_mode: Option<&str>,
    revisable: bool,
) -> Result<RevisableOutcome<ModelModeAction>> {
    let values = advertised_values_for_category(response, AgentSessionConfigCategory::Mode)
        .unwrap_or_default();
    if let Some(explicit) = args.mode.as_deref() {
        // Validate against the response already in hand;
        // `validate_agent_session_config_value` would spawn a second session.
        validate_advertised_value(response, AgentSessionConfigCategory::Mode, explicit).map_err(
            |err| StackError::AgentConfigProvision {
                path: config_path.to_path_buf(),
                reason: format!("{err}; advertised modes: [{}]", values.join(", ")),
            },
        )?;
        write_mode_into_config(config, explicit.to_owned());
        return Ok(RevisableOutcome::Answered(ModelModeAction::Set));
    }
    // The registry default only lands unattended and only when the harness still
    // advertises it; a stale catalog value must not survive validation by accident.
    if !interactive
        && config.agent.mode.is_none()
        && let Some(default_mode) = default_mode
        && values.iter().any(|value| value == default_mode)
    {
        write_mode_into_config(config, default_mode.to_owned());
        return Ok(RevisableOutcome::Answered(ModelModeAction::Set));
    }
    let selected = match prompt_session_config_selection(
        HostedPromptKind::Mode,
        interactive,
        &values,
        AgentSessionConfigCategory::Mode,
        revisable,
    )? {
        RevisableOutcome::Revised(revision) => return Ok(RevisableOutcome::Revised(revision)),
        RevisableOutcome::Answered(None) => {
            return Ok(RevisableOutcome::Answered(ModelModeAction::Skipped));
        }
        RevisableOutcome::Answered(Some(selected)) => selected,
    };
    write_mode_into_config(config, selected);
    Ok(RevisableOutcome::Answered(ModelModeAction::Set))
}

/// Modes always live at the config root, never in the provider block.
fn write_mode_into_config(config: &mut Config, mode: String) {
    prompt::emit_state_signal(|| InitStateSignal::CategorySettled {
        category: InitCategory::Mode,
        value: Some(mode.clone()),
    });
    config.agent.mode = Some(mode);
}

/// Forward effort lane, kept as the direct entry point the lane's own tests drive.
#[cfg(test)]
fn configure_effort_for_init(
    args: &InitArgs,
    home: &Path,
    config: &mut Config,
    config_path: &Path,
    response: &NewSessionResponse,
    interactive: bool,
) -> Result<ModelModeAction> {
    match configure_effort_lane(
        args,
        home,
        config,
        config_path,
        response,
        interactive,
        false,
    )? {
        RevisableOutcome::Answered(action) => Ok(action),
        RevisableOutcome::Revised(revision) => Err(unexpected_revision(&revision)),
    }
}

/// Effort counterpart to `configure_mode_lane`, sharing the caller's
/// provisional session. A harness that pins the effort on disk takes its
/// values from the provider catalog instead of the advertisement.
fn configure_effort_lane(
    args: &InitArgs,
    home: &Path,
    config: &mut Config,
    config_path: &Path,
    response: &NewSessionResponse,
    interactive: bool,
    revisable: bool,
) -> Result<RevisableOutcome<ModelModeAction>> {
    let catalog_effort_lane = effort_value_is_explicit_without_discovery(&config.agent);
    let values = if catalog_effort_lane {
        match catalog_effort_values(home, config) {
            Ok(values) => values,
            Err(error) => {
                if let Some(explicit) = args.effort.as_deref() {
                    return Err(StackError::AgentConfigProvision {
                        path: config_path.to_path_buf(),
                        reason: format!("cannot validate --effort {explicit}: {error}"),
                    });
                }
                init_progress(args, &format!("effort discovery skipped: {error}"));
                return Ok(RevisableOutcome::Answered(ModelModeAction::Skipped));
            }
        }
    } else {
        advertised_values_for_category(response, AgentSessionConfigCategory::Effort)
            .unwrap_or_default()
    };
    if let Some(explicit) = args.effort.as_deref() {
        if catalog_effort_lane {
            validate_catalog_effort_value(home, config, explicit).map_err(|err| {
                StackError::AgentConfigProvision {
                    path: config_path.to_path_buf(),
                    reason: err.to_string(),
                }
            })?;
        } else {
            validate_advertised_value(response, AgentSessionConfigCategory::Effort, explicit)
                .map_err(|err| StackError::AgentConfigProvision {
                    path: config_path.to_path_buf(),
                    reason: format!("{err}; advertised efforts: [{}]", values.join(", ")),
                })?;
        }
        write_effort_into_config(config, explicit.to_owned());
        return Ok(RevisableOutcome::Answered(ModelModeAction::Set));
    }
    let selected = match prompt_session_config_selection(
        HostedPromptKind::Effort,
        interactive,
        &values,
        AgentSessionConfigCategory::Effort,
        revisable,
    )? {
        RevisableOutcome::Revised(revision) => return Ok(RevisableOutcome::Revised(revision)),
        RevisableOutcome::Answered(None) => {
            return Ok(RevisableOutcome::Answered(ModelModeAction::Skipped));
        }
        RevisableOutcome::Answered(Some(selected)) => selected,
    };
    write_effort_into_config(config, selected);
    Ok(RevisableOutcome::Answered(ModelModeAction::Set))
}

fn configured_model_value(config: &Config) -> Option<String> {
    config.agent.model.clone().or_else(|| {
        config
            .agent
            .provider
            .as_ref()
            .and_then(|provider| provider.model.clone())
    })
}

/// Effort lives at the config root like `mode`.
fn write_effort_into_config(config: &mut Config, effort: String) {
    prompt::emit_state_signal(|| InitStateSignal::CategorySettled {
        category: InitCategory::Effort,
        value: Some(effort.clone()),
    });
    config.agent.effort = Some(effort);
}

/// One-way corrections to the registry's applicability verdict from the
/// harness's `session/new` config_options; the registry stays the write
/// authority, so a harness advertising values the registry denies changes
/// nothing.
fn emit_discovery_applicability_corrections(
    response: &agent_client_protocol::schema::v1::NewSessionResponse,
    registry_set_model: bool,
    registry_set_mode: bool,
    registry_set_effort: bool,
) {
    prompt::emit_state_signals(|| {
        [
            (
                InitCategory::Model,
                AgentSessionConfigCategory::Model,
                registry_set_model,
            ),
            (
                InitCategory::Mode,
                AgentSessionConfigCategory::Mode,
                registry_set_mode,
            ),
            (
                InitCategory::Effort,
                AgentSessionConfigCategory::Effort,
                registry_set_effort,
            ),
        ]
        .into_iter()
        .filter_map(|(category, acp_category, registry_says)| {
            let advertised_empty = advertised_values_for_category(response, acp_category)
                .unwrap_or_default()
                .is_empty();
            (registry_says && advertised_empty).then(|| InitStateSignal::CategoryApplicability {
                category,
                applicable: false,
                source: ApplicabilitySource::Discovery,
                reason: Some(format!(
                    "agent advertised no `{}` values on session/new",
                    acp_category.id()
                )),
            })
        })
        .collect()
    });
}

/// The durable `provider_configure` step holds all three lanes, so badge the
/// lanes that were live when the error surfaced before it propagates.
fn signal_lane_failure(model_lane: bool, mode_lane: bool, effort_lane: bool, error: &StackError) {
    for (live, category) in [
        (model_lane, InitCategory::Model),
        (mode_lane, InitCategory::Mode),
        (effort_lane, InitCategory::Effort),
    ] {
        if live {
            prompt::emit_state_signal(|| InitStateSignal::CategoryFailed {
                category,
                code: error.error_code().to_owned(),
            });
        }
    }
}

/// Reproduces `init_println!`'s output-mode split from the args, so a
/// swallowed discovery failure still reaches hosted clients as progress.
fn init_progress(args: &InitArgs, message: &str) {
    if prompt::hosted_driver_active() {
        prompt::emit_progress(message.to_owned());
    } else if !args.handoff_json {
        println!("{message}");
    }
}

/// Write the chosen model into whichever config slot the agent uses, clearing
/// the other one. Runtime selection in supervisor.rs prefers root
/// `agent.model`, so a stray value left there would silently override a
/// newly chosen provider model.
fn write_model_into_config(config: &mut Config, model: String, provider_backed: bool) {
    prompt::emit_state_signal(|| InitStateSignal::CategorySettled {
        category: InitCategory::Model,
        value: Some(model.clone()),
    });
    if provider_backed && let Some(provider) = config.agent.provider.as_mut() {
        provider.model = Some(model);
        config.agent.model = None;
    } else {
        config.agent.model = Some(model);
        if let Some(provider) = config.agent.provider.as_mut() {
            provider.model = None;
        }
    }
}

/// Undo an accepted model, which a re-answer of the model lane to nothing asks
/// for. Both slots go, since either one alone would still pin a model.
fn clear_model_from_config(config: &mut Config) {
    prompt::emit_state_signal(|| InitStateSignal::CategorySettled {
        category: InitCategory::Model,
        value: None,
    });
    config.agent.model = None;
    if let Some(provider) = config.agent.provider.as_mut() {
        provider.model = None;
    }
}

fn prompt_session_config_selection(
    kind: HostedPromptKind,
    interactive: bool,
    values: &[String],
    category: AgentSessionConfigCategory,
    revisable: bool,
) -> Result<RevisableOutcome<Option<String>>> {
    if values.is_empty() || !interactive {
        return Ok(RevisableOutcome::Answered(None));
    }
    #[derive(Clone, PartialEq, Eq)]
    enum ConfigChoice {
        Value(String),
        Skip,
    }
    let items: Vec<prompt::PromptItem<ConfigChoice>> = selection_prompt_options(values)
        .into_iter()
        .map(|(id, label)| {
            let choice = if id == SKIP_OPTION_ID {
                ConfigChoice::Skip
            } else {
                ConfigChoice::Value(id.clone())
            };
            prompt::item(choice, id, label, "")
        })
        .collect();
    mark_discovery_prompt_issued();
    let prompt_text = format!("select {}", category.id());
    let selected = if revisable {
        prompt::searchable_select_revisable(kind, interactive, &prompt_text, &items)?
    } else {
        RevisableOutcome::Answered(prompt::searchable_select(
            kind,
            interactive,
            &prompt_text,
            &items,
        )?)
    };
    Ok(match selected {
        RevisableOutcome::Revised(revision) => RevisableOutcome::Revised(revision),
        RevisableOutcome::Answered(Some(ConfigChoice::Value(value))) => {
            RevisableOutcome::Answered(Some(value))
        }
        RevisableOutcome::Answered(Some(ConfigChoice::Skip)) | RevisableOutcome::Answered(None) => {
            RevisableOutcome::Answered(None)
        }
    })
}

#[cfg(test)]
mod tests;
