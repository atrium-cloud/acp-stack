use std::path::{Path, PathBuf};

use crate::cli::agent::{agent_model_is_explicit_without_discovery, model_values_for_cli_display};
use crate::config::Config;
use crate::dev_gates::{
    FIXTURE_CONFIG_OPTIONS_ENV, FIXTURE_NEW_SESSION_RESPONSE_ENV, TEST_SKIP_AGENT_INSTALL_ENV,
};
use crate::error::{Result, StackError};
use crate::runtime::agent::acp_bridge::AgentSessionConfigCategory;
use crate::runtime::agent::acp_bridge::{KIMI_CODE_AGENT_ID, KIMI_CODE_DEFAULT_MODEL};
use crate::runtime::agent::model_discovery::{
    advertised_values_for_category, fetch_session_config, resolve_advertised_model_value,
    validate_advertised_value,
};
use crate::runtime::agent::provider_keys::{CODEX_AGENT_ID, agent_provider_id_for_provider_id};
use crate::runtime::agent::provider_model_catalog::cached_models;
use crate::runtime::install::agent_registry::RegistryCatalog;
use crate::secrets::SecretStore;

use super::headless_snapshot::{
    capture_dir_listings_for, capture_path_snapshots, headless_config_candidate_paths,
    headless_config_side_dirs, remove_new_files_in_dirs, restore_headless_snapshots,
};
use super::provider::{
    pending_custom_provider_credential, pending_provider_credential_reason,
    primary_provider_is_custom,
};
use super::registry_apply::is_custom_agent;
use super::state_signal::{ApplicabilitySource, InitCategory, InitStateSignal};
use super::{InitArgs, prompt, prompts_enabled};

/// Option id of the synthetic "Skip" choice in a session-config selection.
/// It shares the id namespace with the harness-advertised values, so it is
/// double-underscored to stay out of their way and filtered out of them below.
const SKIP_OPTION_ID: &str = "__skip";

/// Outcome of one init session-config selection lane.
/// `Skipped` covers "agent doesn't support this category", "no flag, no
/// resume, no interactive prompt", and the codex non-OpenAI lane when no
/// live provider catalog is available; `PrintedList` is the
/// L87 path where non-interactive init prints advertised values but
/// declines to mutate config; `Set` triggers a canonical re-write.
///
/// The mode lane never yields `PrintedList`: it has no print-and-skip
/// behavior, because an unattended run without `--mode` never enters it.
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
}

/// Capability gates for both lanes, evaluated before any side effects. An
/// explicit flag is never silently dropped: the operator gets a precise
/// capability error instead of a downstream "binary not on PATH" / "no
/// advertised values" / silent no-op.
///
/// Also called at the top of `configure_model_and_mode_for_init` so the gates
/// hold for every caller of the lane, not just the run that preflighted it.
pub(super) fn preflight_model_and_mode_for_init(
    args: &InitArgs,
    registry: &RegistryCatalog,
    config: &Config,
    config_path: &Path,
) -> Result<()> {
    // The clap conflict catches `--custom-agent-id` paired with these flags;
    // this catches the config-driven path, where an existing custom-agent
    // config is re-inited with `--model`/`--mode` and no custom-agent flags.
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
    // For provider-backed agents the model belongs inside
    // `[agent.provider]`. Allowing `--model` without `--provider` would
    // either silently write to the root `agent.model` slot (which the
    // headless provisioners and `acps agent set` deliberately avoid for
    // these agents) or pair the new model with a stale provider block.
    // Require the operator to pair them explicitly; for model-only
    // agents (set_provider=false) a bare `--model` is still fine.
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
    // Mode lives at the config root, so the reason differs from the model's:
    // the advertised mode list comes from a provisional session, and a
    // provider-backed harness with no provider cannot be launched to produce
    // one. Validating `--mode` against nothing would accept any string.
    if args.mode.is_some() && provider_missing {
        return Err(StackError::InvalidParam {
            field: "mode",
            reason: format!(
                "{} needs a configured provider before its modes can be discovered; pass --provider <id> together with --mode, or run `acps agent set` after init",
                entry.name,
            ),
        });
    }
    Ok(())
}

/// Drives the model and mode ACP-discovery flows during `acps init`.
///
/// - L84: spawns one provisional ACP session via `fetch_session_config`
///   when the configured agent supports model or mode setup, so the advertised
///   lists come straight from the installed harness instead
///   of a stale registry snapshot. Both lanes share the one session.
/// - L85: reads `model` and `mode` `session/new` config_options before
///   accepting or printing any choice.
/// - L86: explicit `--model`/`--mode` values are validated against the
///   advertised list before being written to canonical config.
/// - L87: non-interactive runs without `--model` print the
///   advertised values and return `PrintedList` so the caller does NOT
///   mutate that field; init continues with the existing config so
///   downstream steps stay usable. Mode has no such lane: a non-interactive
///   run without `--mode` never enters it and prints nothing.
///
/// Lane resolution is deliberately fall-through rather than early-return: a
/// model lane that is skipped, pinned, or written without discovery must still
/// let the mode lane reach the same session (amp, set_model=false/set_mode=true,
/// has nothing but a mode lane).
pub(super) fn configure_model_and_mode_for_init(
    args: &InitArgs,
    home: &Path,
    registry: &RegistryCatalog,
    config: &mut Config,
    config_path: &Path,
    secrets: &SecretStore,
) -> Result<ModelModeOutcome> {
    let Some(entry) = registry.lookup(&config.agent.id) else {
        return Ok(ModelModeOutcome::default());
    };
    preflight_model_and_mode_for_init(args, registry, config, config_path)?;
    if !entry.set_model && !entry.set_mode {
        return Ok(ModelModeOutcome::default());
    }
    let mut outcome = ModelModeOutcome::default();
    // Kimi requires a model before its ACP process can initialize, so its
    // model is an init input rather than a discovered value: without
    // `--model`, pin the tier-universal default instead of spawning the
    // agent for a picker. A model already present in config (from a prior
    // init or `agent set`) is kept; the operator can re-select any time
    // with `acps agent set --model`. Either way the model lane is settled
    // before the mode lane may spawn the harness, which is what makes that
    // spawn legal for kimi at all.
    let mut model_lane_resolved = false;
    if entry.set_model && config.agent.id == KIMI_CODE_AGENT_ID && args.model.is_none() {
        if config.agent.model.is_none() {
            write_model_into_config(
                config,
                KIMI_CODE_DEFAULT_MODEL.to_owned(),
                entry.set_provider,
            );
            outcome.model_action = ModelModeAction::Set;
        }
        model_lane_resolved = true;
    }
    // Custom-provider flow already wrote a literal model id into the
    // provider config and that id is not an ACP-advertised value, so
    // the model lane is skipped for custom-provider runs. Mode is
    // provider-independent, so its lane still runs.
    let skip_model_lane = primary_provider_is_custom(config);
    // Discovery is skipped, but an explicit `--model` still has to land: a
    // rerun over an existing custom-provider config (no `--custom-provider`
    // this run, so nothing else writes it) would otherwise drop the flag.
    // Custom-provider model ids are accepted as supplied.
    if skip_model_lane
        && entry.set_model
        && let Some(model) = args.model.as_deref()
    {
        write_model_into_config(config, model.to_owned(), entry.set_provider);
        outcome.model_action = ModelModeAction::Set;
    }

    let interactive = prompts_enabled(args);
    let provider_set_this_run = args.provider.is_some();
    // For provider-backed agents, the model belongs inside
    // `[agent.provider]`. If no provider is configured (neither set
    // this run nor pre-existing in the loaded config), suppress the
    // interactive model picker — otherwise it would write into root
    // `agent.model`, which the supervisor prefers and which the
    // provider-backed model-ownership contract explicitly forbids
    // for these agents. The mode lane shares the gate because a
    // provider-backed harness with no provider cannot be launched to
    // advertise anything.
    let provider_present =
        provider_set_this_run || config.agent.provider.is_some() || !entry.set_provider;
    let explicit_model_without_discovery = args.model.is_some()
        && !args.custom_provider
        && agent_model_is_explicit_without_discovery(config);
    // Discovery runs when the model lane needs the advertised list — either to
    // validate an explicit value (L86), to drive an interactive picker (L84), or
    // to surface the L87 print-and-skip behavior after a provider was just set
    // non-interactively.
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
    // The mode lane has no "print the list" fallback, so an unattended run
    // without `--mode` stays out of it entirely: no spawn, no output.
    let mode_lane_active =
        entry.set_mode && provider_present && (args.mode.is_some() || interactive);
    if !model_lane_active && !mode_lane_active {
        return Ok(outcome);
    }
    // `explicit_flags` gates and names the failure path of the preflight checks
    // below: only a flag whose lane is still live can be validated by this
    // session, and the operator must be told which one could not be honored.
    let explicit_flags = match (
        model_lane_active && args.model.is_some(),
        mode_lane_active && args.mode.is_some(),
    ) {
        (true, true) => Some("--model and --mode"),
        (true, false) => Some("--model"),
        (false, true) => Some("--mode"),
        (false, false) => None,
    };

    let fixture_discovery = std::env::var_os(FIXTURE_CONFIG_OPTIONS_ENV).is_some()
        || std::env::var_os(FIXTURE_NEW_SESSION_RESPONSE_ENV).is_some();

    // A hosted init accepts a custom provider whose api-key ref has not landed
    // yet, expecting a managed credential push once init finishes. Until it
    // lands the agent cannot resolve its environment, so the discovery spawn
    // would fail on a state that is pending by design. Checked before the
    // binary/cwd preconditions so the attribution names the credential rather
    // than whatever else happens to be unready.
    if !fixture_discovery
        && let Some((provider_id, api_key_ref)) =
            pending_custom_provider_credential(config, secrets)
    {
        let reason = pending_provider_credential_reason(&provider_id, &api_key_ref);
        if let Some(flags) = explicit_flags {
            let error = StackError::AgentConfigProvision {
                path: config_path.to_path_buf(),
                reason: format!("cannot validate {flags} for {}: {reason}", entry.name),
            };
            signal_lane_failure(model_lane_active, mode_lane_active, &error);
            return Err(error);
        }
        init_progress(
            args,
            &format!(
                "{} discovery skipped: {reason}",
                active_lane_label(model_lane_active, mode_lane_active)
            ),
        );
        return Ok(outcome);
    }

    // Two preconditions must hold before we spawn the agent for
    // session/new:
    //   1. The agent binary must resolve on PATH so the spawn won't
    //      hit ENOENT at the exec syscall. `resolve_command_path` is
    //      run with the same cwd `fetch_session_config` will use so
    //      relative commands resolve consistently.
    //   2. The spawn cwd directory must exist because the bridge's
    //      `current_dir(&cwd)` setup fails with ENOENT otherwise.
    //      `fetch_session_config` prefers `config.agent.cwd` over
    //      `workspace.root`, so we must mirror that selection or the
    //      preflight can pass on a directory the spawn never visits
    //      (audit P2).
    // When either is missing on a non-explicit call we skip the L84-L87
    // dance with a printed note — the operator gets a working partial config
    // they can finish off with a follow-up `acps init --model`. For an explicit
    // `--model`/`--mode` we fail loudly so the value is never silently accepted
    // without validation.
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
            signal_lane_failure(model_lane_active, mode_lane_active, &error);
            return Err(error);
        }
        let lanes = active_lane_label(model_lane_active, mode_lane_active);
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

    // Provision the agent's headless config so the spawned harness
    // sees the NEW provider rather than whatever was on disk before.
    // For codex/pi/goose the advertised model list can vary by
    // configured provider, so discovering against a stale headless
    // config would surface the wrong options.
    //
    // To keep the "rejection writes nothing" guarantee, snapshot every
    // candidate file's PRIOR contents (or None for "did not exist")
    // BEFORE provisioning runs. The provisioners are per-agent and
    // map to known paths; we walk those candidates up-front so a
    // post-provision restore can roll back to true prior state on
    // discovery/validation failure. On success the provision stays;
    // step 5 (agent_headless_config) will re-provision with the final
    // post-discovery model shape.
    //
    // Known narrow caveat: Codex provisioners (`provision_codex_openai_config`
    // and the OpenRouter branch) short-circuit with `Ok(None)` when no
    // model is configured yet, so discovery for codex+provider-only runs
    // against whatever ~/.codex/config.toml looked like before this run.
    // Harmless in practice: the codex non-OpenAI lane prints the provider
    // catalog instead of the advertised list, and the codex+openai lane
    // advertises the same built-in catalog either way.
    let candidate_paths = headless_config_candidate_paths(&config.agent.id, home);
    let snapshots = capture_path_snapshots(&candidate_paths)?;
    // Also record directory listings so rollback can remove side files
    // the provisioner created out-of-band:
    //   - codex OpenAI writes `~/.codex/config.<provider>.toml`
    //     backup files alongside the primary config.
    //   - Goose custom provider writes
    //     `~/.config/goose/custom_providers/<operator-id>.json`,
    //     whose name is operator-supplied so it can't be enumerated
    //     via candidate_paths.
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
            .inspect_err(|error| signal_lane_failure(model_lane_active, mode_lane_active, error))?;
        let response = match fetch_session_config(home, config) {
            Ok(response) => response,
            // A mode-only lane with no explicit `--mode` is pure enrichment:
            // before the mode lane existed these agents never spawned here at
            // all, so a harness that cannot complete a provisional session must
            // not turn an otherwise good init into a failure. Swallowing inside
            // the closure also keeps the headless provision, which step 5
            // re-runs unconditionally; the outer snapshot restore exists for
            // rejected explicit values, which still propagate.
            Err(error) if !model_lane_active && args.mode.is_none() => {
                let reason = format!("mode discovery skipped: {error}");
                init_progress(args, &reason);
                prompt::emit_state_signal(|| InitStateSignal::CategoryApplicability {
                    category: InitCategory::Mode,
                    applicable: false,
                    // The session never completed, so this says nothing about
                    // whether the agent has modes — only that this run cannot
                    // find out. A mode already in config outlives it.
                    source: ApplicabilitySource::DiscoveryUnavailable,
                    reason: Some(reason),
                });
                return Ok(outcome);
            }
            Err(error) => {
                signal_lane_failure(model_lane_active, mode_lane_active, &error);
                return Err(error);
            }
        };
        emit_discovery_applicability_corrections(&response, entry.set_model, entry.set_mode);
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
            .inspect_err(|error| signal_lane_failure(true, false, error))?;
        }
        if mode_lane_active {
            outcome.mode_action =
                configure_mode_for_init(args, config, config_path, &response, interactive)
                    .inspect_err(|error| signal_lane_failure(false, true, error))?;
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

/// Name the lanes that were about to run, so an amp-class agent with nothing
/// but a mode lane is not told its model discovery was skipped.
fn active_lane_label(model_lane_active: bool, mode_lane_active: bool) -> &'static str {
    match (model_lane_active, mode_lane_active) {
        (false, true) => "mode",
        (true, true) => "model and mode",
        _ => "model",
    }
}

/// Shared precondition check before spawning the configured agent from init.
/// Both preconditions mirror what the bridge itself needs: the command must
/// resolve on PATH (exec would ENOENT) and the spawn cwd must exist
/// (`current_dir(&cwd)` fails otherwise). Fixture discovery bypasses both
/// because no process is spawned.
pub(super) enum AgentSpawnPreflight {
    Ready,
    Fixture,
    CwdMissing(PathBuf),
    BinaryMissing,
}

pub(super) fn agent_spawn_preflight(config: &Config, fixture_envs: &[&str]) -> AgentSpawnPreflight {
    // `fixture_enabled` (not raw env reads) so a stray fixture var in a build
    // without `test-fixtures` cannot skip the preflight while the consumer
    // ignores the fixture and really spawns.
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

/// Connection gate: confirm the configured agent launches and completes an ACP
/// session. Registry agents are verified implicitly by model discovery,
/// which spawns the same provisional session; this gate exists for agents that
/// do not run discovery (custom agents), so a non-ACP or broken binary is
/// caught during init rather than at first session. Skips quietly when the
/// binary is not yet on PATH or the spawn cwd is missing — the same
/// preconditions discovery uses — so a partial or `--skip-workspace-init` run is
/// not failed here.
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

/// Handshake-only capability probe for the `capability_probe` init step.
/// Never fails: init must not die because a probe could not run — an
/// unavailable probe just means no ignore claims are made and MCP prompting
/// is not offered.
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

    // codex-acp advertises codex-core's bundled OpenAI preset catalog
    // regardless of the configured provider (see
    // `model_value_is_explicit_without_discovery`), so for codex lanes the
    // advertised list is not a truthful pickable set. Substitute the
    // provider's live catalog — refreshed best-effort by the caller right
    // before discovery — and when no catalog exists (custom provider or an
    // offline fetch) skip the lane with a hint rather than surface the
    // misleading presets.
    let codex_catalog_lane =
        agent_model_is_explicit_without_discovery(config) && config.agent.id == CODEX_AGENT_ID;
    let values: Vec<String> = if codex_catalog_lane {
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
            advertised_values_for_category(response, AgentSessionConfigCategory::Model)?,
        )
    };
    let interactive = prompts_enabled(args);
    if !interactive {
        // L87: non-interactive run, no explicit choice. Print the
        // advertised values so the operator can rerun with one, and
        // do NOT mutate config — provider stays set, model stays at
        // whatever it was (most commonly unset, so the agent picks
        // its own default on session/new).
        if !args.handoff_json {
            if codex_catalog_lane {
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
/// provisional session. There is no L87 print-and-skip lane: an unattended run
/// without `--mode` never reaches here, and a set_mode agent that turns out to
/// advertise nothing is reported through the discovery correction rather than
/// printed at the operator.
fn configure_mode_for_init(
    args: &InitArgs,
    config: &mut Config,
    config_path: &Path,
    response: &agent_client_protocol::schema::v1::NewSessionResponse,
    interactive: bool,
) -> Result<ModelModeAction> {
    // An agent that advertises no `mode` option errors here rather than
    // returning an empty list; for the picker that is simply "nothing to pick",
    // while an explicit `--mode` still fails through `validate_advertised_value`
    // below with the same rejection wording it would have produced.
    let values = advertised_values_for_category(response, AgentSessionConfigCategory::Mode)
        .unwrap_or_default();
    if let Some(explicit) = args.mode.as_deref() {
        // Validation runs against the response already in hand:
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

/// Modes always live at the config root — unlike models they are not owned by
/// the provider block. Settlement is signalled here, at the write, for the same
/// reason `write_model_into_config` does it: the report cannot drift from what
/// actually landed in config.
fn write_mode_into_config(config: &mut Config, mode: String) {
    prompt::emit_state_signal(|| InitStateSignal::CategorySettled {
        category: InitCategory::Mode,
        value: Some(mode.clone()),
    });
    config.agent.mode = Some(mode);
}

/// Live corrections to the registry's applicability verdict, from the one
/// thing that actually knows: the installed harness's `session/new`
/// config_options. The correction runs one way only. `applicable` promises the
/// client that init will configure the lane, and the registry is the authority
/// for writes — init still refuses to write a mode for a `set_mode = false`
/// agent — so a harness advertising values the registry denies changes nothing
/// this run will do.
fn emit_discovery_applicability_corrections(
    response: &agent_client_protocol::schema::v1::NewSessionResponse,
    registry_set_model: bool,
    registry_set_mode: bool,
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

/// The durable `provider_configure` step holds both lanes, so a step-level
/// failure alone cannot say which one broke. Badge the lanes that were live
/// when the error surfaced, before it propagates.
fn signal_lane_failure(model_lane: bool, mode_lane: bool, error: &StackError) {
    for (live, category) in [
        (model_lane, InitCategory::Model),
        (mode_lane, InitCategory::Mode),
    ] {
        if live {
            prompt::emit_state_signal(|| InitStateSignal::CategoryFailed {
                category,
                code: error.error_code().to_owned(),
            });
        }
    }
}

/// `init_println!` lives in run.rs and needs that run's output mode, which this
/// lane never receives; the three-way split is reproduced from what the args
/// imply so a swallowed discovery failure still reaches hosted clients as
/// progress instead of only stdout.
fn init_progress(args: &InitArgs, message: &str) {
    if prompt::hosted_driver_active() {
        prompt::emit_progress(message.to_owned());
    } else if !args.handoff_json {
        println!("{message}");
    }
}

/// Write the chosen model into whichever config slot the agent uses.
/// Provider-backed agents (`set_provider = true`) store the model under
/// `[agent.provider]` so it travels with provider+api_key_ref as one atomic
/// group; provider-less agents (e.g. set_provider=false) store it at the
/// agent root. Matches what `acps agent set` does.
///
/// When writing into the provider slot, also clear any stray root
/// `agent.model` that a prior model-only flow may have left behind —
/// runtime selection in supervisor.rs prefers the root slot, so a leftover
/// value there would silently override the newly chosen provider model.
///
/// Settlement is signalled here rather than at the five call sites because
/// this is the write: a lane that grows a sixth path cannot forget to report.
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
    // Option ids are answerable over the wire, so a harness that advertises the
    // same value twice would make one of the two unreachable. Dedup here rather
    // than trusting agent-supplied data, keeping first-occurrence order. A value
    // colliding with the Skip sentinel is dropped for the same reason: it would
    // otherwise shadow Skip and leave the operator no way out of the prompt.
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
mod tests {
    use agent_client_protocol::schema::v1::{NewSessionResponse, SessionConfigOption};
    use clap::Parser;

    use super::*;

    #[derive(clap::Parser)]
    struct TestInitArgs {
        #[command(flatten)]
        args: InitArgs,
    }

    fn parse_init_args(argv: &[&str]) -> InitArgs {
        let mut all = vec!["init-test"];
        all.extend_from_slice(argv);
        TestInitArgs::parse_from(all).args
    }

    /// Mirrors the shape `fetch_session_config` returns for a fixture run.
    fn response_with(models: &[&str], modes: &[&str]) -> NewSessionResponse {
        let mut options = Vec::new();
        for (id, values) in [("model", models), ("mode", modes)] {
            if values.is_empty() {
                continue;
            }
            let option: SessionConfigOption = serde_json::from_value(serde_json::json!({
                "id": id,
                "name": id,
                "category": id,
                "type": "select",
                "currentValue": values[0],
                "options": values
                    .iter()
                    .map(|value| serde_json::json!({ "value": value, "name": value }))
                    .collect::<Vec<_>>(),
            }))
            .expect("session config option fixture parses");
            options.push(option);
        }
        NewSessionResponse::new("test").config_options(options)
    }

    fn amp_config() -> Config {
        let mut config = crate::config::load_config_from_str(include_str!(
            "../../../tests/fixtures/valid-opencode-stack.toml"
        ))
        .expect("fixture config");
        config.agent.id = "amp".to_owned();
        config
    }

    #[test]
    fn explicit_mode_is_written_and_settled_at_the_write() {
        let mut config = amp_config();
        let args = parse_init_args(&["--mode", "deep"]);
        let driver = std::sync::Arc::new(prompt::RecordingPromptDriver::default());

        let action = prompt::with_hosted_driver(driver.clone(), || {
            configure_mode_for_init(
                &args,
                &mut config,
                Path::new("acps-config.toml"),
                &response_with(&[], &["smart", "rush", "deep"]),
                true,
            )
        })
        .expect("advertised mode is accepted");

        assert_eq!(action, ModelModeAction::Set);
        assert_eq!(config.agent.mode.as_deref(), Some("deep"));
        assert_eq!(
            driver.recorded(),
            vec![InitStateSignal::CategorySettled {
                category: InitCategory::Mode,
                value: Some("deep".to_owned()),
            }],
        );
    }

    #[test]
    fn explicit_mode_that_is_not_advertised_lists_the_advertised_modes() {
        let mut config = amp_config();
        let args = parse_init_args(&["--mode", "bogus"]);

        let error = configure_mode_for_init(
            &args,
            &mut config,
            Path::new("acps-config.toml"),
            &response_with(&[], &["smart", "rush", "deep"]),
            true,
        )
        .expect_err("unadvertised mode must be rejected");

        assert!(
            error
                .to_string()
                .contains("advertised modes: [deep, rush, smart]"),
            "error: {error}"
        );
        assert!(config.agent.mode.is_none());
    }

    // Nothing to pick is not an error: the operator is told through the state
    // report, not by failing an init that had no explicit request.
    #[test]
    fn a_mode_picker_with_nothing_advertised_skips_without_writing() {
        let mut config = amp_config();
        let args = parse_init_args(&[]);

        let action = configure_mode_for_init(
            &args,
            &mut config,
            Path::new("acps-config.toml"),
            &response_with(&["openai/gpt-5.5"], &[]),
            true,
        )
        .expect("an empty mode list is not a failure");

        assert_eq!(action, ModelModeAction::Skipped);
        assert!(config.agent.mode.is_none());
    }

    /// Answers every picker with its first option and keeps what was offered,
    /// so a test can assert on the option ids that reached the wire.
    #[derive(Default)]
    struct FirstChoiceDriver {
        offered: std::sync::Mutex<Vec<Vec<String>>>,
    }

    impl prompt::HostedPromptDriver for FirstChoiceDriver {
        fn select(
            &self,
            request: prompt::HostedPromptRequest,
        ) -> Result<prompt::HostedPromptOutcome<Option<usize>>> {
            self.offered
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(
                    request
                        .items
                        .iter()
                        .map(|item| item.value.clone())
                        .collect(),
                );
            Ok(prompt::HostedPromptOutcome::Handled(Some(0)))
        }

        fn confirm(
            &self,
            _request: prompt::HostedPromptRequest,
        ) -> Result<prompt::HostedPromptOutcome<bool>> {
            Ok(prompt::HostedPromptOutcome::Unhandled)
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

    // Option ids are answerable over the wire and the shared prompt entry point
    // asserts they are unique, so a harness advertising the same mode twice
    // would take a debug build down with it.
    #[test]
    fn a_repeated_advertised_value_is_offered_once() {
        let mut config = amp_config();
        let args = parse_init_args(&[]);
        let driver = std::sync::Arc::new(FirstChoiceDriver::default());

        let action = prompt::with_hosted_driver(driver.clone(), || {
            configure_mode_for_init(
                &args,
                &mut config,
                Path::new("acps-config.toml"),
                &response_with(&[], &["smart", "deep", "smart"]),
                true,
            )
        })
        .expect("a duplicated advertised value is not a failure");

        assert_eq!(action, ModelModeAction::Set);
        assert_eq!(config.agent.mode.as_deref(), Some("deep"));
        assert_eq!(
            driver
                .offered
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .as_slice(),
            [vec![
                "deep".to_owned(),
                "smart".to_owned(),
                "__skip".to_owned()
            ]]
        );
    }

    #[test]
    fn discovery_only_retracts_a_lane_the_registry_claimed() {
        let driver = std::sync::Arc::new(prompt::RecordingPromptDriver::default());
        prompt::with_hosted_driver(driver.clone(), || {
            // Registry says both lanes exist; the harness advertises neither.
            emit_discovery_applicability_corrections(&response_with(&[], &[]), true, true);
            // Registry says neither; the harness advertises modes anyway. Init
            // will not write a mode for such an agent, so nothing is claimed.
            emit_discovery_applicability_corrections(&response_with(&[], &["plan"]), false, false);
        });

        let recorded = driver.recorded();
        assert_eq!(
            recorded,
            [InitCategory::Model, InitCategory::Mode].map(|category| {
                InitStateSignal::CategoryApplicability {
                    category,
                    applicable: false,
                    source: ApplicabilitySource::Discovery,
                    reason: Some(format!(
                        "agent advertised no `{}` values on session/new",
                        match category {
                            InitCategory::Model => "model",
                            _ => "mode",
                        }
                    )),
                }
            }),
            "only the registry-claimed lanes the harness contradicts are corrected"
        );
    }
}
