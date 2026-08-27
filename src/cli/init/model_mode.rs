use std::path::{Path, PathBuf};

use crate::cli::agent::{agent_model_is_explicit_without_discovery, model_values_for_cli_display};
use crate::config::Config;
use crate::dev_gates::{
    FIXTURE_CONFIG_OPTIONS_ENV, FIXTURE_NEW_SESSION_RESPONSE_ENV, TEST_SKIP_AGENT_INSTALL_ENV,
};
use crate::error::{Result, StackError};
use crate::runtime::agent::acp_bridge::AgentSessionConfigCategory;
use crate::runtime::agent::acp_bridge::{KIMI_CODE_AGENT_ID, kimi_default_model_for_provider};
use crate::runtime::agent::agent_headless_config::HERMES_AGENT_ID;
use crate::runtime::agent::model_discovery::{
    advertised_values_for_category, fetch_session_config, resolve_advertised_model_value,
    validate_advertised_value,
};
use crate::runtime::agent::provider_keys::{CODEX_AGENT_ID, agent_provider_id_for_provider_id};
use crate::runtime::agent::provider_model_catalog::cached_models;
use crate::runtime::install::agent_registry::RegistryCatalog;
use crate::secrets::{SharedSecretStore, lock_shared_secret_store};

use super::headless_snapshot::{
    capture_dir_listings_for, capture_path_snapshots, headless_config_candidate_paths,
    headless_config_side_dirs, remove_new_files_in_dirs, restore_headless_snapshots,
};
use super::provider::{
    pending_deferred_provider_credential, pending_provider_credential_reason,
    primary_provider_is_custom,
};
use super::registry_apply::is_custom_agent;
use super::state_signal::{ApplicabilitySource, InitCategory, InitStateSignal};
use super::{InitArgs, prompt, prompts_enabled};

/// Option id of the synthetic "Skip" choice; double-underscored to stay clear
/// of the harness-advertised id namespace it shares.
const SKIP_OPTION_ID: &str = "__skip";

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
    let Some(entry) = registry.lookup(&config.agent.id) else {
        return Ok(ModelModeOutcome::default());
    };
    preflight_model_and_mode_for_init(args, registry, config, config_path)?;
    if !entry.set_model && !entry.set_mode && !entry.set_effort {
        return Ok(ModelModeOutcome::default());
    }
    let mut outcome = ModelModeOutcome::default();
    // Kimi cannot initialize its ACP process without a model, so the model
    // lane MUST settle here before the mode lane may spawn the harness.
    let mut model_lane_resolved = false;
    if entry.set_model
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
        if !model_settled {
            // The subscription tier ships `kimi-for-coding`, which does not
            // exist on the Moonshot platform.
            let provider_id = config
                .agent
                .provider
                .as_ref()
                .map(|provider| provider.id.as_str());
            write_model_into_config(
                config,
                kimi_default_model_for_provider(provider_id).to_owned(),
                entry.set_provider,
            );
            outcome.model_action = ModelModeAction::Set;
        }
        model_lane_resolved = true;
    }
    // A custom-provider model id is not an ACP-advertised value, so the model
    // lane is skipped; mode is provider-independent and still runs.
    let skip_model_lane = primary_provider_is_custom(config);
    // Discovery is skipped, but an explicit `--model` still has to land or a
    // rerun over an existing custom-provider config would drop the flag.
    if skip_model_lane
        && entry.set_model
        && let Some(model) = args.model.as_deref()
    {
        write_model_into_config(config, model.to_owned(), entry.set_provider);
        outcome.model_action = ModelModeAction::Set;
    }

    let interactive = prompts_enabled(args);
    let provider_set_this_run = args.provider.is_some();
    // Without a provider the picker would write root `agent.model`, which the
    // supervisor prefers and the provider-backed ownership contract forbids.
    let provider_present =
        provider_set_this_run || config.agent.provider.is_some() || !entry.set_provider;
    let explicit_model_without_discovery = args.model.is_some()
        && !args.custom_provider
        && agent_model_is_explicit_without_discovery(config);
    let mut model_lane_active = entry.set_model
        && !skip_model_lane
        && !model_lane_resolved
        && provider_present
        && (args.model.is_some() || interactive || provider_set_this_run);
    if model_lane_active
        && explicit_model_without_discovery
        && let Some(model) = args.model.as_deref()
    {
        write_model_into_config(config, model.to_owned(), entry.set_provider);
        outcome.model_action = ModelModeAction::Set;
        model_lane_active = false;
    }
    // No print-the-list fallback here, so an unattended run without
    // `--mode`/`--effort` never spawns the harness at all.
    let mode_lane_active =
        entry.set_mode && provider_present && (args.mode.is_some() || interactive);
    let effort_lane_active =
        entry.set_effort && provider_present && (args.effort.is_some() || interactive);
    if !model_lane_active && !mode_lane_active && !effort_lane_active {
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
                    write_model_into_config(config, model.to_owned(), entry.set_provider);
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
                reason: format!("cannot validate {flags} for {}: {reason}", entry.name),
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
                active_lane_label(model_lane_active, mode_lane_active, effort_lane_active)
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
                reason: format!("cannot validate {flags} for {}: {reason}", entry.name),
            };
            signal_lane_failure(
                model_lane_active,
                mode_lane_active,
                effort_lane_active,
                &error,
            );
            return Err(error);
        }
        let lanes = active_lane_label(model_lane_active, mode_lane_active, effort_lane_active);
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
        crate::runtime::agent::provider_model_catalog::refresh_provider_models_best_effort_blocking(
            home, config,
        );
        crate::runtime::agent::agent_headless_config::provision_agent_headless_config(config, home)
            .inspect_err(|error| {
                signal_lane_failure(
                    model_lane_active,
                    mode_lane_active,
                    effort_lane_active,
                    error,
                )
            })?;
        let response = match fetch_session_config(home, config) {
            Ok(response) => response,
            // A mode/effort-only lane with no explicit flag is pure enrichment,
            // so a harness that cannot complete a provisional session must not
            // fail an otherwise good init.
            Err(error) if !model_lane_active && args.mode.is_none() && args.effort.is_none() => {
                let reason = format!(
                    "{} discovery skipped: {error}",
                    active_lane_label(false, mode_lane_active, effort_lane_active)
                );
                init_progress(args, &reason);
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
                        reason: Some(reason.clone()),
                    })
                    .collect()
                });
                return Ok(outcome);
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
        };
        emit_discovery_applicability_corrections(
            &response,
            entry.set_model,
            entry.set_mode,
            entry.set_effort,
        );
        if model_lane_active {
            outcome.model_action = configure_model_for_init(
                args,
                home,
                config,
                config_path,
                &response,
                &entry.name,
                entry.set_provider,
            )
            .inspect_err(|error| signal_lane_failure(true, false, false, error))?;
        }
        if mode_lane_active {
            outcome.mode_action =
                configure_mode_for_init(args, config, config_path, &response, interactive)
                    .inspect_err(|error| signal_lane_failure(false, true, false, error))?;
        }
        if effort_lane_active {
            outcome.effort_action =
                configure_effort_for_init(args, config, config_path, &response, interactive)
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

fn active_lane_label(
    model_lane_active: bool,
    mode_lane_active: bool,
    effort_lane_active: bool,
) -> String {
    let lanes: Vec<&str> = [
        model_lane_active.then_some("model"),
        mode_lane_active.then_some("mode"),
        effort_lane_active.then_some("effort"),
    ]
    .into_iter()
    .flatten()
    .collect();
    match lanes.as_slice() {
        [] => "model".to_owned(),
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

pub(super) fn agent_spawn_preflight(config: &Config, fixture_envs: &[&str]) -> AgentSpawnPreflight {
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
    if crate::runtime::agent::acp_bridge::resolve_command_path(&config.agent.command, &spawn_cwd)
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
    fetch_session_config(home, config)
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
    Probed(crate::runtime::agent::acp_bridge::AgentCapabilitiesDto),
    Unavailable { reason: String },
}

pub(super) fn probe_agent_capabilities_for_init(
    home: &Path,
    config: &Config,
) -> CapabilityProbeOutcome {
    match agent_spawn_preflight(config, &[crate::dev_gates::FIXTURE_AGENT_CAPABILITIES_ENV]) {
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
        Ok(capabilities) => CapabilityProbeOutcome::Probed(capabilities),
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

fn configure_model_for_init(
    args: &InitArgs,
    home: &Path,
    config: &mut Config,
    config_path: &Path,
    response: &agent_client_protocol::schema::v1::NewSessionResponse,
    agent_name: &str,
    provider_backed: bool,
) -> Result<ModelModeAction> {
    if let Some(explicit) = args.model.as_deref() {
        if agent_model_is_explicit_without_discovery(config) {
            write_model_into_config(config, explicit.to_owned(), provider_backed);
            return Ok(ModelModeAction::Set);
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
        return Ok(ModelModeAction::Set);
    }

    // codex-acp advertises codex-core's bundled OpenAI preset catalog whatever
    // the configured provider, and Hermes speaks pre-1.0 ACP and advertises
    // nothing, so neither list is a truthful pickable set; substitute the
    // provider's live catalog instead.
    let provider_catalog_lane = agent_model_is_explicit_without_discovery(config)
        && (config.agent.id == CODEX_AGENT_ID || config.agent.id == HERMES_AGENT_ID);
    let values: Vec<String> = if provider_catalog_lane {
        let catalog = config
            .agent
            .provider
            .as_ref()
            .filter(|provider| provider.custom.is_none())
            .and_then(|provider| cached_models(home, &provider.id));
        match catalog {
            Some(models) => models.into_iter().map(|model| model.value).collect(),
            None => {
                if !args.handoff_json {
                    println!(
                        "no live model catalog available for {agent_name}; \
                         rerun with `acps init --model <value>` to write a model into config"
                    );
                }
                return Ok(ModelModeAction::Skipped);
            }
        }
    } else {
        model_values_for_cli_display(
            config,
            advertised_values_for_category(response, AgentSessionConfigCategory::Model)
                .unwrap_or_default(),
        )
    };
    if values.is_empty() {
        return Ok(ModelModeAction::Skipped);
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
        return Ok(ModelModeAction::PrintedList);
    }

    let Some(selected) = prompt_session_config_selection(
        prompt::HostedPromptKind::Model,
        interactive,
        &values,
        AgentSessionConfigCategory::Model,
    )?
    else {
        return Ok(ModelModeAction::Skipped);
    };
    if !agent_model_is_explicit_without_discovery(config) {
        validate_advertised_value(response, AgentSessionConfigCategory::Model, &selected)?;
    }
    write_model_into_config(config, selected, provider_backed);
    Ok(ModelModeAction::Set)
}

/// Mode counterpart to `configure_model_for_init`, sharing the caller's one
/// provisional session.
fn configure_mode_for_init(
    args: &InitArgs,
    config: &mut Config,
    config_path: &Path,
    response: &agent_client_protocol::schema::v1::NewSessionResponse,
    interactive: bool,
) -> Result<ModelModeAction> {
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
        return Ok(ModelModeAction::Set);
    }
    let Some(selected) = prompt_session_config_selection(
        prompt::HostedPromptKind::Mode,
        interactive,
        &values,
        AgentSessionConfigCategory::Mode,
    )?
    else {
        return Ok(ModelModeAction::Skipped);
    };
    write_mode_into_config(config, selected);
    Ok(ModelModeAction::Set)
}

/// Modes always live at the config root, never in the provider block.
fn write_mode_into_config(config: &mut Config, mode: String) {
    prompt::emit_state_signal(|| InitStateSignal::CategorySettled {
        category: InitCategory::Mode,
        value: Some(mode.clone()),
    });
    config.agent.mode = Some(mode);
}

/// Effort counterpart to `configure_mode_for_init`, sharing the caller's one
/// provisional session.
fn configure_effort_for_init(
    args: &InitArgs,
    config: &mut Config,
    config_path: &Path,
    response: &agent_client_protocol::schema::v1::NewSessionResponse,
    interactive: bool,
) -> Result<ModelModeAction> {
    let values = advertised_values_for_category(response, AgentSessionConfigCategory::Effort)
        .unwrap_or_default();
    if let Some(explicit) = args.effort.as_deref() {
        validate_advertised_value(response, AgentSessionConfigCategory::Effort, explicit).map_err(
            |err| StackError::AgentConfigProvision {
                path: config_path.to_path_buf(),
                reason: format!("{err}; advertised efforts: [{}]", values.join(", ")),
            },
        )?;
        write_effort_into_config(config, explicit.to_owned());
        return Ok(ModelModeAction::Set);
    }
    let Some(selected) = prompt_session_config_selection(
        prompt::HostedPromptKind::Effort,
        interactive,
        &values,
        AgentSessionConfigCategory::Effort,
    )?
    else {
        return Ok(ModelModeAction::Skipped);
    };
    write_effort_into_config(config, selected);
    Ok(ModelModeAction::Set)
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

fn prompt_session_config_selection(
    kind: prompt::HostedPromptKind,
    interactive: bool,
    values: &[String],
    category: AgentSessionConfigCategory,
) -> Result<Option<String>> {
    if values.is_empty() || !interactive {
        return Ok(None);
    }
    #[derive(Clone, PartialEq, Eq)]
    enum ConfigChoice {
        Value(String),
        Skip,
    }
    // Option ids are answerable over the wire, so a duplicate advertised value
    // would make one unreachable and one colliding with the Skip sentinel would
    // leave the operator no way out of the prompt.
    let mut seen = std::collections::BTreeSet::new();
    let mut items: Vec<prompt::PromptItem<ConfigChoice>> = values
        .iter()
        .filter(|value| value.as_str() != SKIP_OPTION_ID)
        .filter(|value| seen.insert(value.as_str()))
        .map(|value| {
            prompt::item(
                ConfigChoice::Value(value.clone()),
                value.clone(),
                value.clone(),
                "",
            )
        })
        .collect();
    items.push(prompt::item(ConfigChoice::Skip, SKIP_OPTION_ID, "Skip", ""));
    match prompt::searchable_select(
        kind,
        interactive,
        &format!("select {}", category.id()),
        &items,
    )? {
        None => Ok(None),
        Some(ConfigChoice::Value(value)) => Ok(Some(value)),
        Some(ConfigChoice::Skip) => Ok(None),
    }
}

#[cfg(test)]
mod tests;
