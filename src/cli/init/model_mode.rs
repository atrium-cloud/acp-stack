use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::error::{Result, StackError};
use crate::runtime::agent::acp_bridge::AgentSessionConfigCategory;
use crate::runtime::agent::model_discovery::{
    advertised_values_for_category, fetch_session_config, validate_advertised_value,
};
use crate::runtime::install::agent_registry::RegistryCatalog;

use super::InitArgs;
use super::headless_snapshot::{
    capture_dir_listings_for, capture_path_snapshots, headless_config_candidate_paths,
    headless_config_side_dirs, remove_new_files_in_dirs, restore_headless_snapshots,
};

/// Outcome of a single category (model or mode) selection step.
/// `Skipped` covers both "agent doesn't support this category" and
/// "no flag, no resume, no interactive prompt"; `PrintedList` is the
/// L87 path where non-interactive init prints advertised values but
/// declines to mutate config; `Set` triggers a canonical re-write.
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

/// Drives the L84-L87 ACP-discovery flow during `acps init`.
///
/// - L84: spawns one provisional ACP session via `fetch_session_config`
///   when the configured agent supports model or mode setup, so the
///   advertised lists come straight from the installed harness instead
///   of a stale registry snapshot.
/// - L85: reads `model` and `mode` `session/new` config_options before
///   accepting or printing any choice.
/// - L86: explicit `--model`/`--mode` values are validated against the
///   advertised list before being written to canonical config.
/// - L87: non-interactive runs without `--model`/`--mode` print the
///   advertised values and return `PrintedList` so the caller does NOT
///   mutate that field; init continues with the existing config so
///   downstream steps stay usable.
pub(super) fn configure_model_and_mode_for_init(
    args: &InitArgs,
    home: &Path,
    registry: &RegistryCatalog,
    config: &mut Config,
    config_path: &Path,
) -> Result<ModelModeOutcome> {
    let Some(entry) = registry.lookup(&config.agent.id) else {
        return Ok(ModelModeOutcome::default());
    };

    // Capability gate, evaluated before any side effects. Reject
    // explicit --model/--mode for agents whose registry entry says the
    // category is not supported — surfacing this here means the operator
    // gets a precise capability error instead of a downstream "binary
    // not on PATH" / "no advertised values" / silent no-op (audit P1
    // for --model, P2 for --mode).
    if args.model.is_some() && !entry.set_model {
        return Err(StackError::AgentConfigProvision {
            path: config_path.to_path_buf(),
            reason: format!(
                "{} does not support model configuration through `acps init`",
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
    if args.model.is_some()
        && entry.set_provider
        && args.provider.is_none()
        && config.agent.provider.is_none()
    {
        return Err(StackError::InvalidParam {
            field: "model",
            reason: format!(
                "{} stores the model inside [agent.provider]; pass --provider <id> together with --model, or run `acps agent set` after init",
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
    if !entry.set_model && !entry.set_mode {
        return Ok(ModelModeOutcome::default());
    }
    // Custom-provider flow already wrote a literal model id into the
    // provider config and that id is not an ACP-advertised value, so
    // the MODEL lane is skipped for custom-provider runs. The MODE
    // lane is independent of provider choice (the agent advertises the
    // same set of modes regardless), so mode discovery still runs to
    // honor an explicit `--mode` or interactive picker.
    let skip_model_lane = args.custom_provider;

    let interactive = io::stdin().is_terminal();
    let provider_set_this_run = args.provider.is_some();
    // For provider-backed agents, the model belongs inside
    // `[agent.provider]`. If no provider is configured (neither set
    // this run nor pre-existing in the loaded config), suppress the
    // interactive model picker — otherwise it would write into root
    // `agent.model`, which the supervisor prefers and which the
    // provider-backed model-ownership contract explicitly forbids
    // for these agents.
    let provider_present =
        provider_set_this_run || config.agent.provider.is_some() || !entry.set_provider;
    // Each lane is active independently. Discovery runs when at least
    // one lane needs the advertised list — either to validate an
    // explicit value (L86), to drive an interactive picker (L84), or
    // to surface the L87 print-and-skip behavior after a provider was
    // just set non-interactively.
    let model_lane_active = entry.set_model
        && !skip_model_lane
        && provider_present
        && (args.model.is_some() || interactive || provider_set_this_run);
    let mode_lane_active =
        entry.set_mode && (args.mode.is_some() || interactive || provider_set_this_run);
    if !model_lane_active && !mode_lane_active {
        return Ok(ModelModeOutcome::default());
    }
    // `explicit` gates the failure path of the preflight checks below:
    // an explicit `--model` (when the model lane is active, i.e. not
    // custom-provider) or `--mode` must error out rather than silently
    // skip if the binary or cwd is missing.
    let explicit = (args.model.is_some() && model_lane_active) || args.mode.is_some();

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
    // dance with a printed note — the operator gets a working partial
    // config they can finish off with a follow-up `acps init --model`.
    // For explicit `--model`/`--mode` we fail loudly so they're never
    // silently accepted without validation.
    let spawn_cwd: PathBuf = config
        .agent
        .cwd
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(&config.workspace.root));
    let binary_missing =
        crate::runtime::agent::acp_bridge::resolve_command_path(&config.agent.command, &spawn_cwd)
            .is_none();
    let cwd_missing = !spawn_cwd.is_dir();
    if binary_missing || cwd_missing {
        if explicit {
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
                    "spawn cwd `{}` does not exist; create it or run workspace materialize first",
                    spawn_cwd.display(),
                ),
                (false, false) => unreachable!(),
            };
            return Err(StackError::AgentConfigProvision {
                path: config_path.to_path_buf(),
                reason: format!(
                    "cannot validate --model/--mode for {}: {reason}",
                    entry.name
                ),
            });
        }
        if binary_missing {
            println!(
                "model/mode discovery skipped: agent command `{}` not found on PATH",
                config.agent.command,
            );
        } else {
            println!(
                "model/mode discovery skipped: spawn cwd `{}` is not yet provisioned",
                spawn_cwd.display(),
            );
        }
        return Ok(ModelModeOutcome::default());
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
    // post-discovery model/mode shape.
    //
    // Known narrow caveat: Codex provisioners (`provision_codex_openai_config`
    // and the OpenRouter branch) short-circuit with `Ok(None)` when no
    // model is configured yet, so the L87 "print advertised models"
    // path for codex+provider-only discovers against whatever
    // ~/.codex/config.toml looked like before this run. The advertised
    // list itself comes from Codex's built-in catalog rather than the
    // configured provider, so the practical impact is limited to
    // first-run codex when the operator switches providers without
    // also passing --model.
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
        crate::runtime::agent::agent_headless_config::provision_agent_headless_config(
            config, home,
        )?;
        let response = fetch_session_config(home, config)?;
        let mut outcome = ModelModeOutcome::default();
        // Honor the per-lane gates rather than `entry.set_*` alone:
        // when only one lane is active (e.g. `--mode plan` for an
        // agent that advertises both model and mode), running the
        // other lane would print an advertised list the operator
        // never asked for or error out on a category they explicitly
        // omitted.
        if model_lane_active {
            outcome.model_action =
                configure_model_for_init(args, config, config_path, &response, &entry.name)?;
        }
        if mode_lane_active {
            outcome.mode_action =
                configure_mode_for_init(args, config, config_path, &response, &entry.name)?;
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

fn configure_model_for_init(
    args: &InitArgs,
    config: &mut Config,
    config_path: &Path,
    response: &agent_client_protocol::schema::NewSessionResponse,
    agent_name: &str,
) -> Result<ModelModeAction> {
    if let Some(explicit) = args.model.as_deref() {
        validate_advertised_value(response, AgentSessionConfigCategory::Model, explicit).map_err(
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
        write_model_into_config(config, explicit.to_owned());
        return Ok(ModelModeAction::Set);
    }

    let values = advertised_values_for_category(response, AgentSessionConfigCategory::Model)?;
    if !io::stdin().is_terminal() {
        // L87: non-interactive run, no explicit choice. Print the
        // advertised values so the operator can rerun with one, and
        // do NOT mutate config — provider stays set, model stays at
        // whatever it was (most commonly unset, so the agent picks
        // its own default on session/new).
        println!("advertised models for {agent_name}:");
        for value in &values {
            println!("  {value}");
        }
        println!("rerun with `acps init --model <value>` to write a model into config");
        return Ok(ModelModeAction::PrintedList);
    }

    let Some(selected) =
        prompt_session_config_selection(&values, AgentSessionConfigCategory::Model)?
    else {
        return Ok(ModelModeAction::Skipped);
    };
    validate_advertised_value(response, AgentSessionConfigCategory::Model, &selected)?;
    write_model_into_config(config, selected);
    Ok(ModelModeAction::Set)
}

fn configure_mode_for_init(
    args: &InitArgs,
    config: &mut Config,
    config_path: &Path,
    response: &agent_client_protocol::schema::NewSessionResponse,
    agent_name: &str,
) -> Result<ModelModeAction> {
    if let Some(explicit) = args.mode.as_deref() {
        validate_advertised_value(response, AgentSessionConfigCategory::Mode, explicit).map_err(
            |err| {
                let advertised =
                    advertised_values_for_category(response, AgentSessionConfigCategory::Mode)
                        .unwrap_or_default();
                StackError::AgentConfigProvision {
                    path: config_path.to_path_buf(),
                    reason: format!("{err}; advertised modes: [{}]", advertised.join(", "),),
                }
            },
        )?;
        config.agent.mode = Some(explicit.to_owned());
        return Ok(ModelModeAction::Set);
    }

    let values = advertised_values_for_category(response, AgentSessionConfigCategory::Mode)
        .unwrap_or_default();
    if values.is_empty() {
        // Agent supports `set_mode` per registry but did not surface
        // a `mode` config option this session. Treat as skipped rather
        // than erroring so init still completes.
        return Ok(ModelModeAction::Skipped);
    }
    if !io::stdin().is_terminal() {
        println!("advertised modes for {agent_name}:");
        for value in &values {
            println!("  {value}");
        }
        println!("rerun with `acps init --mode <value>` to write a mode into config");
        return Ok(ModelModeAction::PrintedList);
    }
    let Some(selected) =
        prompt_session_config_selection(&values, AgentSessionConfigCategory::Mode)?
    else {
        return Ok(ModelModeAction::Skipped);
    };
    validate_advertised_value(response, AgentSessionConfigCategory::Mode, &selected)?;
    config.agent.mode = Some(selected);
    Ok(ModelModeAction::Set)
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
fn write_model_into_config(config: &mut Config, model: String) {
    if let Some(provider) = config.agent.provider.as_mut() {
        provider.model = Some(model);
        config.agent.model = None;
    } else {
        config.agent.model = Some(model);
    }
}

fn prompt_session_config_selection(
    values: &[String],
    category: AgentSessionConfigCategory,
) -> Result<Option<String>> {
    if values.is_empty() {
        return Ok(None);
    }
    println!("available {} values:", category.id());
    for (index, value) in values.iter().enumerate() {
        println!("  {}. {value}", index + 1);
    }
    print!(
        "select {} [number or value, blank to skip]: ",
        category.id()
    );
    io::stdout()
        .flush()
        .map_err(|source| StackError::ConfigWrite {
            path: PathBuf::from("stdout"),
            source,
        })?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .map_err(|source| StackError::ConfigRead {
            path: PathBuf::from("stdin"),
            source,
        })?;
    let answer = answer.trim();
    if answer.is_empty() {
        return Ok(None);
    }
    if let Ok(index) = answer.parse::<usize>() {
        // Reject `0` explicitly: `saturating_sub(1)` would map it to the
        // first entry, but the menu is 1-indexed and accepting 0 would let
        // the operator confidently pick "0" expecting an out-of-range
        // error and instead silently get the first item.
        if index == 0 {
            return Err(StackError::InvalidParam {
                field: "selection",
                reason: format!(
                    "{} selection `{answer}` is out of range (expected 1..={})",
                    category.id(),
                    values.len()
                ),
            });
        }
        let Some(value) = values.get(index - 1) else {
            return Err(StackError::InvalidParam {
                field: "selection",
                reason: format!(
                    "{} selection `{answer}` is out of range (expected 1..={})",
                    category.id(),
                    values.len()
                ),
            });
        };
        return Ok(Some(value.clone()));
    }
    Ok(Some(answer.to_owned()))
}
