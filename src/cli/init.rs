mod headless_snapshot;
mod install;
mod model_mode;
mod provider;
mod registry_apply;
mod resume;
mod starter_config;
mod testflight;

use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use clap::{Args, ValueEnum};

use crate::config::{self, AgentSubagentConfig, Config};
use crate::error::{Result, StackError};
use crate::fs_util::{
    atomic_write_owner_only, create_dir_owner_only, home_dir, parent_dir, pre_create_owner_only,
    set_owner_only_file, write_new_file_owner_only,
};
use crate::runtime::agent::agent_headless_config::OPENCODE_AGENT_ID;
use crate::runtime::init_runner::{StepDisposition, StepOutcome, record_step, step_kind};
use crate::runtime::install::agent_installer::InstallerOutcome;
use crate::runtime::install::agent_registry::RegistryCatalog;
use crate::secrets::{SecretStore, age_key_path, secret_store_path};
use crate::state::{
    INIT_RUN_FAILED, INIT_RUN_SUCCEEDED, INIT_STEP_FAILED, INIT_STEP_PENDING, INIT_STEP_RUNNING,
    StateStore, default_state_path,
};

use self::headless_snapshot::{
    capture_dir_listings_for, capture_path_snapshots, headless_config_candidate_paths,
    headless_config_side_dirs, remove_new_files_in_dirs, restore_headless_snapshots,
};
use self::install::{
    install_configured_agent, local_bin_dir, operator_registry_override, should_install_agent,
};
use self::model_mode::{ModelModeAction, configure_model_and_mode_for_init};
use self::provider::configure_provider_for_init;
use self::registry_apply::{
    apply_edge_profile_to_config, apply_registry_entry_to_config, select_agent_for_init,
};
use self::resume::{
    finalize_with_error, init_complete_event_already_recorded, installer_postcondition_holds,
    perform_secrets_init, recorded_init_args, resolve_init_run, step_needs_resume,
    workspace_postcondition_holds,
};
use self::starter_config::{starter_config, validate_deployment_overrides_match_existing};
use self::testflight::{TestflightDecision, resolve_testflight_decision};

#[derive(Debug, Args)]
pub struct InitArgs {
    /// Select the configured agent non-interactively from the registry.
    #[arg(long)]
    pub(super) agent: Option<String>,
    /// Select the initial provider id for agents that support provider setup.
    #[arg(long)]
    pub(super) provider: Option<String>,
    /// Secret ref to inject for the selected initial provider.
    #[arg(long, requires = "provider")]
    pub(super) api_key_ref: Option<String>,
    /// Configure the selected provider as a custom provider.
    #[arg(long, requires = "provider")]
    pub(super) custom_provider: bool,
    /// Display name for a custom provider.
    #[arg(long = "provider-name", requires = "custom_provider")]
    pub(super) provider_name: Option<String>,
    /// Base URL for a custom provider.
    #[arg(long = "base-url", requires = "custom_provider")]
    pub(super) base_url: Option<String>,
    /// API family for a custom provider: chat-completions or responses.
    #[arg(long = "provider-api", requires = "custom_provider")]
    pub(super) provider_api: Option<String>,
    /// Initial model id. With `--custom-provider`, taken verbatim as the
    /// custom model id. Otherwise validated against the agent's
    /// ACP-advertised `model` values discovered via a provisional
    /// session.
    #[arg(long)]
    pub(super) model: Option<String>,
    /// Initial mode value. Validated against the agent's ACP-advertised
    /// `mode` values discovered via the same provisional session.
    #[arg(long)]
    pub(super) mode: Option<String>,
    /// Display name for a custom model.
    #[arg(long = "model-name", requires = "custom_provider")]
    pub(super) model_name: Option<String>,
    /// Context window in tokens for a custom model.
    #[arg(long, requires = "custom_provider")]
    pub(super) context: Option<String>,
    /// Maximum output tokens for a custom model.
    #[arg(long = "output-max-tokens", requires = "custom_provider")]
    pub(super) output_max_tokens: Option<String>,
    /// Install the selected or already configured agent during init.
    #[arg(long, conflicts_with = "no_install_agent")]
    pub(super) install_agent: bool,
    /// Skip the install prompt in interactive runs.
    #[arg(long)]
    pub(super) no_install_agent: bool,
    /// Configure a public edge profile during init.
    #[arg(long, value_enum)]
    pub(super) edge: Option<EdgeProviderArg>,
    /// Public exposure model for the selected edge provider.
    #[arg(long, value_enum, requires = "edge")]
    pub(super) exposure: Option<EdgeExposureArg>,
    /// Public hostname for the edge profile, for example agent.example.com.
    #[arg(long, requires = "edge")]
    pub(super) hostname: Option<String>,
    /// How cloudflared is expected to run for generated Cloudflare artifacts.
    #[arg(long, value_enum, default_value_t = CloudflaredDeploymentArg::Host)]
    pub(super) cloudflared_deployment: CloudflaredDeploymentArg,
    /// Workspace root to write into a newly-created starter config.
    #[arg(long)]
    pub(super) workspace_root: Option<String>,
    /// Workspace uploads path to write into a newly-created starter config.
    #[arg(long)]
    pub(super) workspace_uploads: Option<String>,
    /// Runtime user to write into a newly-created starter config.
    #[arg(long)]
    pub(super) runtime_user: Option<String>,
    /// Pre-seed `[[workspace.code_sources]]` with one or more git
    /// repositories. Repeatable. Accepts an `https://...`, `git@host:repo`,
    /// or other supported repo URL. Only applied when the starter config is
    /// being created.
    #[arg(long = "code-from", value_name = "URL")]
    pub(super) code_from: Vec<String>,
    /// Pre-seed `[[workspace.data_sources]]` with a local path or an
    /// `https://...` archive URL. Repeatable. Only applied when the starter
    /// config is being created.
    #[arg(long = "data-from", value_name = "PATH_OR_URL")]
    pub(super) data_from: Vec<String>,
    /// Skip the workspace materializer; useful for tests and dev loops that
    /// do not need actual content fetched/cloned.
    #[arg(long)]
    pub(super) skip_workspace_init: bool,
    /// Run the real-prompt agent testflight at the end of init. Warns about
    /// provider credit consumption. Mutually exclusive with `--skip-testflight`.
    #[arg(long, conflicts_with = "skip_testflight")]
    pub(super) testflight: bool,
    /// Suppress the end-of-init testflight even in interactive runs.
    #[arg(long)]
    pub(super) skip_testflight: bool,
    /// Resume the most recent non-terminal init run. With `--run-id`, resume
    /// the specified run. Conflicts with `--fresh`.
    #[arg(long, conflicts_with = "fresh")]
    pub(super) resume: bool,
    /// Force a brand-new init run even if a prior run was incomplete.
    /// Conflicts with `--resume`.
    #[arg(long)]
    pub(super) fresh: bool,
    /// Target a specific init run id when resuming. Implies `--resume`.
    #[arg(long, value_name = "ID", requires = "resume")]
    pub(super) run_id: Option<String>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(super) enum EdgeProviderArg {
    Cloudflare,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(super) enum EdgeExposureArg {
    Tunnel,
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub(super) enum CloudflaredDeploymentArg {
    Host,
    Docker,
    External,
}

impl CloudflaredDeploymentArg {
    pub(super) fn as_config_value(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Docker => "docker",
            Self::External => "external",
        }
    }
}

pub(super) const STARTER_MAX_REQUEST_BYTES: u64 = 104_857_600;
pub(super) const STARTER_RATE_LIMIT_PER_MINUTE: u64 = 120;
pub(super) const STARTER_RATE_LIMIT_BURST: u64 = 30;
pub(super) const STARTER_AUTH_FAILURES_PER_MINUTE: u64 = 5;
pub(super) const STARTER_AUTH_BLOCK_DURATION: &str = "15m";
pub(super) const STARTER_SESSION_KEY_REF: &str = "ACP_STACK_SESSION_KEY";
pub(super) const STARTER_ADMIN_KEY_REF: &str = "ACP_STACK_ADMIN_KEY";
pub(super) const STARTER_DEFAULT_SHELL: &str = "/bin/bash";
pub(super) const STARTER_WORKSPACE_MAX_FILE_BYTES: u64 = 8_388_608;
pub(super) const STARTER_LOCAL_RETENTION_DAYS: u64 = 30;
pub(super) const STARTER_LOG_LEVEL: &str = "info";
pub(super) const STARTER_SUPABASE_URL: &str = "https://example.supabase.co";
pub(super) const STARTER_SUPABASE_SERVICE_ROLE_KEY_REF: &str = "SUPABASE_SERVICE_ROLE_KEY";
pub(super) const STARTER_SUPABASE_SCHEMA: &str = "acp_stack";
pub(super) const STARTER_AGENT_ID: &str = "placeholder";
pub(super) const STARTER_AGENT_NAME: &str = "Placeholder Agent";
pub(super) const STARTER_AGENT_COMMAND: &str = "acp-agent";
pub(super) const STARTER_AGENT_RESTART: &str = "never";
pub(super) const STARTER_AGENT_INSTALL_CREATES: &str = "acp-agent";
pub(super) const STARTER_AGENT_INSTALL_TYPE: &str = "shell";
pub(super) const STARTER_AGENT_INSTALL_COMMAND: &str = "true";

fn configure_subagent_inherit_for_init(
    registry: &RegistryCatalog,
    config: &mut Config,
) -> Result<bool> {
    if config.agent.subagent.is_some() {
        return Ok(false);
    }
    let Some(entry) = registry.lookup(&config.agent.id) else {
        return Ok(false);
    };
    if entry.id != OPENCODE_AGENT_ID || entry.subagent_alias.as_deref() != Some("small_model") {
        return Ok(false);
    }
    let Some(provider) = config.agent.provider.as_ref() else {
        return Ok(false);
    };
    if provider
        .model
        .as_deref()
        .is_none_or(|model| model.trim().is_empty())
    {
        return Ok(false);
    }
    if io::stdin().is_terminal() {
        print!(
            "use main agent provider/model for {}? [Y/n]: ",
            entry.subagent_alias.as_deref().unwrap_or("subagent")
        );
        io::stdout()
            .flush()
            .map_err(|source| StackError::ServeIo { source })?;
        let mut answer = String::new();
        io::stdin()
            .read_line(&mut answer)
            .map_err(|source| StackError::ServeIo { source })?;
        let answer = answer.trim();
        if answer.eq_ignore_ascii_case("n") || answer.eq_ignore_ascii_case("no") {
            println!("subagent provider/model left unset; run `acps subagent set` to configure it");
            return Ok(false);
        }
    }
    config.agent.subagent = Some(AgentSubagentConfig {
        disabled: false,
        provider: Some(provider.clone()),
    });
    Ok(true)
}

pub(super) fn run_init(mut args: InitArgs) -> Result<()> {
    let home = home_dir()?;
    let config_path = config::default_config_path()?;
    let state_path = default_state_path(&home);
    let config_dir = parent_dir(&config_path)?;
    let state_dir = parent_dir(&state_path)?;

    create_dir_owner_only(config_dir)?;
    create_dir_owner_only(state_dir)?;

    // Preflight (untracked): config + state migration must succeed before we
    // have anywhere to record init steps. Both are idempotent and cheap, so
    // a partial failure here will be re-attempted on the next `acps init`
    // without needing resume semantics.
    let config_status = if config_path.exists() {
        // Repair perms before validation so a failure to parse the file does not
        // leave a permissive config on disk; matches the behavior of `acps status`.
        set_owner_only_file(&config_path)?;
        let existing_config = Config::load_from_path(&config_path)?;
        validate_deployment_overrides_match_existing(&args, &existing_config)?;
        "validated existing config"
    } else {
        let starter_config = starter_config(&args)?;
        write_new_file_owner_only(&config_path, starter_config.as_bytes())?;
        Config::load_from_path(&config_path)?;
        "created starter config"
    };

    pre_create_owner_only(&state_path)?;
    let store = StateStore::open(&state_path)?;
    store.migrate()?;
    set_owner_only_file(&state_path)?;

    let registry = RegistryCatalog::load_with_override(&operator_registry_override(&home))?;
    let mut config = Config::load_from_path(&config_path)?;
    let selected_agent = select_agent_for_init(&args, &registry)?;
    if let Some(entry) = selected_agent {
        // Fail fast on agents the runtime cannot drive headlessly (browser
        // OAuth, terminal-only adapters, etc.). Without this check init would
        // happily install the binary and only fail at first session spawn,
        // wasting bandwidth and operator time.
        entry.ensure_supported()?;
        apply_registry_entry_to_config(&mut config, entry);
    }
    let edge_requested = apply_edge_profile_to_config(&args, &mut config)?;

    // Pick the run row: either resume an existing one (explicit `--resume` or
    // auto-detected non-terminal latest) or start fresh. Recording every
    // tracked phase as a step lets `acps init resume` continue from the first
    // unsettled step on the next invocation.
    let init_run = resolve_init_run(&args, &store)?;
    let prior_init_steps = store.query_init_steps(&init_run.id)?;
    let resumed = args.resume;
    if resumed {
        println!("resuming init run {}", init_run.id);
    } else {
        println!("init run {}", init_run.id);
    }

    let recorded_args = if resumed {
        Some(recorded_init_args(&init_run)?)
    } else {
        None
    };
    if step_needs_resume(&prior_init_steps, step_kind::PROVIDER_CONFIGURE) {
        // Restore the original `--model`/`--mode` requests
        // unconditionally so the resumed provider_configure step
        // re-applies the explicit selection even if `--provider`
        // is supplied (or omitted) on the resume invocation.
        // Without this, an operator who corrects an invalid
        // `--model` by passing it again on resume but also
        // forgetting `--provider` could either error or drop the
        // selection.
        if args.model.is_none() {
            args.model = recorded_args
                .as_ref()
                .and_then(|recorded| recorded.model.clone());
        }
        if args.mode.is_none() {
            args.mode = recorded_args
                .as_ref()
                .and_then(|recorded| recorded.mode.clone());
        }
        if args.provider.is_none() {
            args.provider = recorded_args
                .as_ref()
                .and_then(|recorded| recorded.provider.clone())
                .or_else(|| {
                    config
                        .agent
                        .provider
                        .as_ref()
                        .map(|provider| provider.id.clone())
                });
            // A failed provider_configure step that owned ONLY
            // model/mode (no provider was ever set) can legitimately
            // resume without `--provider` — the model/mode lane will
            // still re-run via the orchestrator's normal step flow.
            // Only error when we know provider is required AND
            // absent: the prior args_json captured provider too, so a
            // truly corrupt run shows up as "recorded provider was
            // Some, current is None, config has none". For the
            // provider-less model-only case (e.g. cursor --model),
            // continuing is correct.
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
    }
    if step_needs_resume(&prior_init_steps, step_kind::TESTFLIGHT) {
        args.testflight = true;
        args.skip_testflight = false;
    }

    let mut auth_status: &'static str = "preserved existing API keys";
    let session_ref_str = config.auth.session_key_ref.clone();
    let admin_ref_str = config.auth.admin_key_ref.clone();

    // -----------------------------------------------------------------
    // Step 1: secrets_init — generate or preserve session + admin keys.
    // Verifier: both refs present in the secret store.
    //
    // `store_existed` must be captured BEFORE `open_or_create` so the
    // "fresh store" branch fires when the file doesn't yet exist; the
    // open call writes the empty store and would otherwise make the
    // existence probe always succeed.
    // -----------------------------------------------------------------
    let store_existed_before_open = secret_store_path(&home).exists();
    let mut secret_store = SecretStore::open_or_create(&home)?;
    let verify_session_ref = session_ref_str.clone();
    let verify_admin_ref = admin_ref_str.clone();
    let verify_home = home.clone();
    let step_result = record_step(
        &store,
        &init_run,
        1,
        step_kind::SECRETS_INIT,
        || {
            let store = SecretStore::open(&verify_home)?;
            Ok(store.contains(&verify_session_ref) && store.contains(&verify_admin_ref))
        },
        || {
            let outcome = perform_secrets_init(
                store_existed_before_open,
                &session_ref_str,
                &admin_ref_str,
                &mut secret_store,
                &store,
            )?;
            auth_status = outcome.status;
            Ok(StepOutcome::with_payload(format!(
                r#"{{"session_key_ref":"{}","admin_key_ref":"{}","status":"{}"}}"#,
                session_ref_str, admin_ref_str, outcome.status
            )))
        },
    );
    let disposition = match step_result {
        Ok(d) => d,
        Err(error) => return finalize_with_error(&store, &init_run, error),
    };
    // Honest "auth:" line for the skipped path — we did not generate keys
    // this run, we trusted the verifier instead.
    let auth_status = if matches!(disposition, StepDisposition::Skipped) {
        "preserved existing API keys"
    } else {
        auth_status
    };

    // -----------------------------------------------------------------
    // Step 2: agent_install — install the configured agent if requested.
    // -----------------------------------------------------------------
    let install_requested = should_install_agent(&args, selected_agent.is_some())?;
    let mut install_outcome: Option<InstallerOutcome> = None;
    let install_step_needs_resume = step_needs_resume(&prior_init_steps, step_kind::AGENT_INSTALL);
    if install_requested || install_step_needs_resume {
        let verify_config = config.clone();
        let verify_workspace_root = PathBuf::from(config.workspace.root.clone());
        let verify_local_bin_dir = local_bin_dir(&home);
        let result = record_step(
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
                let outcome = install_configured_agent(&home, &config, &registry, &store)?;
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
    // Step 3: provider_configure — write provider/model into the config
    // and persist canonical TOML if anything changed.
    // -----------------------------------------------------------------
    let result = record_step(
        &store,
        &init_run,
        3,
        step_kind::PROVIDER_CONFIGURE,
        || {
            // Provider config is idempotent only when there's no explicit
            // change requested for any of the three lanes this step
            // now owns (provider, model, mode). We always re-run on
            // resume so partial writes (e.g. missing secret refs) get
            // re-collected, and so a resumed `--model`/`--mode` still
            // gets validated and persisted rather than silently
            // skipped because the prior succeeded row passes the
            // verifier.
            Ok(args.provider.is_none() && args.model.is_none() && args.mode.is_none())
        },
        || {
            let provider_configured = configure_provider_for_init(
                &args,
                &registry,
                &mut config,
                &config_path,
                &mut secret_store,
            )?;
            let model_mode_outcome = configure_model_and_mode_for_init(
                &args,
                &home,
                &registry,
                &mut config,
                &config_path,
            )?;
            let model_mode_changed =
                matches!(model_mode_outcome.model_action, ModelModeAction::Set)
                    || matches!(model_mode_outcome.mode_action, ModelModeAction::Set);
            let subagent_configured = configure_subagent_inherit_for_init(&registry, &mut config)?;
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

    // -----------------------------------------------------------------
    // Step 4: workspace_materialize — clone repos + download/extract
    // data sources into /workspace/usr/. Skipped if --skip-workspace-init.
    // Verifier: every source destination has its sentinel file.
    // -----------------------------------------------------------------
    let workspace_for_verify = config.workspace.clone();
    let mut materialize_report = None;
    if !args.skip_workspace_init
        || step_needs_resume(&prior_init_steps, step_kind::WORKSPACE_MATERIALIZE)
    {
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
        let result = crate::runtime::init_runner::record_step_with_default_log_dir(
            &store,
            &init_run,
            4,
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
    }

    // -----------------------------------------------------------------
    // Step 5: agent_headless_config — write the agent's local config
    // files so the harness can start without first-run prompts.
    // -----------------------------------------------------------------
    let mut provisioned_agent_configs = Vec::new();
    let result = record_step(
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
        let result = record_step(
            &store,
            &init_run,
            6,
            step_kind::EDGE_ARTIFACTS,
            || Ok(false),
            || {
                let config_dir = parent_dir(&config_path)?;
                provisioned_edge_artifacts = match config.edge.cloudflare.as_ref() {
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
    let result = record_step(
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

    println!("initialized acp-stack");
    println!("{config_status}: {}", config_path.display());
    println!("state: {}", state_path.display());
    println!("secrets: {}", secret_store.store_path().display());
    println!("age key: {}", age_key_path(&home).display());
    println!("auth: {auth_status}");
    println!("agent: {} ({})", config.agent.name, config.agent.id);
    if let Some(outcome) = install_outcome {
        println!("agent install: {}", outcome.label());
        println!("agent path: {}", outcome.path().display());
        println!("agent sha256: {}", outcome.sha256());
    }
    for provisioned in provisioned_agent_configs {
        println!("{}: {}", provisioned.label, provisioned.path.display());
    }
    for artifact in provisioned_edge_artifacts {
        println!("{}: {}", artifact.label, artifact.path.display());
    }
    if let Some(materialize) = &materialize_report {
        for entry in &materialize.code {
            println!(
                "code source ({:?}): {}",
                entry.outcome,
                entry.destination.display()
            );
        }
        for entry in &materialize.data {
            println!(
                "data source ({:?}): {}",
                entry.outcome,
                entry.destination.display()
            );
        }
    }

    // -----------------------------------------------------------------
    // Step 8: testflight — optional real-prompt smoke. Decision uses
    // the resolver above; only `Run` actually executes the agent.
    // -----------------------------------------------------------------
    if let Some(decision) = resolve_testflight_decision(&args, &config, &registry)? {
        let result = record_step(
            &store,
            &init_run,
            8,
            step_kind::TESTFLIGHT,
            || Ok(!matches!(decision, TestflightDecision::Run)),
            || {
                match decision {
                    TestflightDecision::Run => {
                        println!("---");
                        println!("running real-prompt agent testflight");
                        crate::cli::agent::run_init_testflight(&home, &config, &registry)?;
                    }
                    TestflightDecision::SkipExplicit => {
                        println!("testflight: skipped (--skip-testflight)");
                    }
                    TestflightDecision::SkipNonInteractive => {
                        println!(
                            "testflight: skipped (non-interactive run; pass --testflight to opt in)"
                        );
                    }
                    TestflightDecision::SkipDeclined => {
                        println!("testflight: skipped (declined at prompt)");
                    }
                    TestflightDecision::SkipUnsupported => {
                        println!(
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
    // flags skipped over it — e.g. `--resume --run-id <id>` without
    // `--install-agent` after the original run failed at `agent_install`),
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
    crate::runtime::init_runner::finalize_run(&store, &init_run.id, INIT_RUN_SUCCEEDED)?;
    Ok(())
}
