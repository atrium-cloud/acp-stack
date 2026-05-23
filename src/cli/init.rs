use std::collections::HashMap;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use clap::{Args, ValueEnum};

use crate::agent_installer::{InstallerOutcome, install_resolved, run_installer};
use crate::agent_registry::{RegistryCatalog, RegistryEntry, RegistryKind};
use crate::auth::generate_api_key;
use crate::config::{
    self, AgentConfig, AgentCustomProviderConfig, AgentInstallConfig, AgentProviderConfig,
    ApiConfig, AuthConfig, CloudflareEdgeConfig, CodeSourceConfig, Config, CustomProviderApi,
    DEFAULT_CUSTOM_MODEL_CONTEXT, DEFAULT_CUSTOM_MODEL_OUTPUT_MAX_TOKENS, DataSourceConfig,
    DependencyEntry, EdgeConfig, LoggingConfig, SecurityConfig, SecurityHttpConfig,
    SupabaseLoggingConfig, WorkspaceConfig,
};
use crate::error::{Result, StackError};
use crate::fs_util::{
    atomic_write_owner_only, create_dir_owner_only, home_dir, parent_dir, pre_create_owner_only,
    set_owner_only_file, write_new_file_owner_only,
};
use crate::runtime::init_runner::{
    self, StepDisposition, StepOutcome, begin_run, finalize_run, find_resumable_run, record_step,
    step_kind,
};
use crate::runtime::provider_keys::{
    env_refs_for_agent_id, env_var_for_agent_provider_id, provider_id_is_known,
    provider_id_supports_agent, required_env_refs_for_provider_id,
};
use crate::secrets::{SecretStore, age_key_path, secret_store_path};
use crate::state::{
    INIT_RUN_FAILED, INIT_RUN_SUCCEEDED, INIT_STEP_FAILED, INIT_STEP_PENDING, INIT_STEP_RUNNING,
    InitRunRecord, InitStepRecord, StateStore, default_state_path,
};

#[derive(Debug, Args)]
pub struct InitArgs {
    /// Select the configured agent non-interactively from the registry.
    #[arg(long)]
    agent: Option<String>,
    /// Select the initial provider id for agents that support provider setup.
    #[arg(long)]
    provider: Option<String>,
    /// Secret ref to inject for the selected initial provider.
    #[arg(long, requires = "provider")]
    api_key_ref: Option<String>,
    /// Configure the selected provider as a custom provider.
    #[arg(long, requires = "provider")]
    custom_provider: bool,
    /// Display name for a custom provider.
    #[arg(long = "provider-name", requires = "custom_provider")]
    provider_name: Option<String>,
    /// Base URL for a custom provider.
    #[arg(long = "base-url", requires = "custom_provider")]
    base_url: Option<String>,
    /// API family for a custom provider: chat-completions or responses.
    #[arg(long = "provider-api", requires = "custom_provider")]
    provider_api: Option<String>,
    /// Initial custom model id.
    #[arg(long, requires = "custom_provider")]
    model: Option<String>,
    /// Display name for a custom model.
    #[arg(long = "model-name", requires = "custom_provider")]
    model_name: Option<String>,
    /// Context window in tokens for a custom model.
    #[arg(long, requires = "custom_provider")]
    context: Option<String>,
    /// Maximum output tokens for a custom model.
    #[arg(long = "output-max-tokens", requires = "custom_provider")]
    output_max_tokens: Option<String>,
    /// Install the selected or already configured agent during init.
    #[arg(long, conflicts_with = "no_install_agent")]
    install_agent: bool,
    /// Skip the install prompt in interactive runs.
    #[arg(long)]
    no_install_agent: bool,
    /// Configure a public edge profile during init.
    #[arg(long, value_enum)]
    edge: Option<EdgeProviderArg>,
    /// Public exposure model for the selected edge provider.
    #[arg(long, value_enum, requires = "edge")]
    exposure: Option<EdgeExposureArg>,
    /// Public hostname for the edge profile, for example agent.example.com.
    #[arg(long, requires = "edge")]
    hostname: Option<String>,
    /// How cloudflared is expected to run for generated Cloudflare artifacts.
    #[arg(long, value_enum, default_value_t = CloudflaredDeploymentArg::Host)]
    cloudflared_deployment: CloudflaredDeploymentArg,
    /// Workspace root to write into a newly-created starter config.
    #[arg(long)]
    workspace_root: Option<String>,
    /// Workspace uploads path to write into a newly-created starter config.
    #[arg(long)]
    workspace_uploads: Option<String>,
    /// Runtime user to write into a newly-created starter config.
    #[arg(long)]
    runtime_user: Option<String>,
    /// Pre-seed `[[workspace.code_sources]]` with one or more git
    /// repositories. Repeatable. Accepts an `https://...`, `git@host:repo`,
    /// or other supported repo URL. Only applied when the starter config is
    /// being created.
    #[arg(long = "code-from", value_name = "URL")]
    code_from: Vec<String>,
    /// Pre-seed `[[workspace.data_sources]]` with a local path or an
    /// `https://...` archive URL. Repeatable. Only applied when the starter
    /// config is being created.
    #[arg(long = "data-from", value_name = "PATH_OR_URL")]
    data_from: Vec<String>,
    /// Skip the workspace materializer; useful for tests and dev loops that
    /// do not need actual content fetched/cloned.
    #[arg(long)]
    skip_workspace_init: bool,
    /// Run the real-prompt agent testflight at the end of init. Warns about
    /// provider credit consumption. Mutually exclusive with `--skip-testflight`.
    #[arg(long, conflicts_with = "skip_testflight")]
    testflight: bool,
    /// Suppress the end-of-init testflight even in interactive runs.
    #[arg(long)]
    skip_testflight: bool,
    /// Resume the most recent non-terminal init run. With `--run-id`, resume
    /// the specified run. Conflicts with `--fresh`.
    #[arg(long, conflicts_with = "fresh")]
    resume: bool,
    /// Force a brand-new init run even if a prior run was incomplete.
    /// Conflicts with `--resume`.
    #[arg(long)]
    fresh: bool,
    /// Target a specific init run id when resuming. Implies `--resume`.
    #[arg(long, value_name = "ID", requires = "resume")]
    run_id: Option<String>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum EdgeProviderArg {
    Cloudflare,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum EdgeExposureArg {
    Tunnel,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CloudflaredDeploymentArg {
    Host,
    Docker,
    External,
}

impl CloudflaredDeploymentArg {
    fn as_config_value(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Docker => "docker",
            Self::External => "external",
        }
    }
}

const STARTER_MAX_REQUEST_BYTES: u64 = 104_857_600;
const STARTER_RATE_LIMIT_PER_MINUTE: u64 = 120;
const STARTER_RATE_LIMIT_BURST: u64 = 30;
const STARTER_AUTH_FAILURES_PER_MINUTE: u64 = 5;
const STARTER_AUTH_BLOCK_DURATION: &str = "15m";
const STARTER_SESSION_KEY_REF: &str = "ACP_STACK_SESSION_KEY";
const STARTER_ADMIN_KEY_REF: &str = "ACP_STACK_ADMIN_KEY";
const STARTER_DEFAULT_SHELL: &str = "/bin/bash";
const STARTER_WORKSPACE_MAX_FILE_BYTES: u64 = 8_388_608;
const STARTER_LOCAL_RETENTION_DAYS: u64 = 30;
const STARTER_LOG_LEVEL: &str = "info";
const STARTER_SUPABASE_URL: &str = "https://example.supabase.co";
const STARTER_SUPABASE_SERVICE_ROLE_KEY_REF: &str = "SUPABASE_SERVICE_ROLE_KEY";
const STARTER_SUPABASE_SCHEMA: &str = "acp_stack";
const STARTER_AGENT_ID: &str = "placeholder";
const STARTER_AGENT_NAME: &str = "Placeholder Agent";
const STARTER_AGENT_COMMAND: &str = "acp-agent";
const STARTER_AGENT_RESTART: &str = "never";
const STARTER_AGENT_INSTALL_CREATES: &str = "acp-agent";
const STARTER_AGENT_INSTALL_TYPE: &str = "shell";
const STARTER_AGENT_INSTALL_COMMAND: &str = "true";

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
    if args.provider.is_none()
        && step_needs_resume(&prior_init_steps, step_kind::PROVIDER_CONFIGURE)
    {
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
        if args.provider.is_none() {
            return finalize_with_error(
                &store,
                &init_run,
                StackError::InitRunCorrupted {
                    reason: format!(
                        "init run {} has a failed provider_configure step but no provider id is available; pass --provider on resume",
                        init_run.id
                    ),
                },
            );
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
            // change requested. We always re-run on resume so partial writes
            // (e.g. missing secret refs) get re-collected.
            Ok(args.provider.is_none())
        },
        || {
            let provider_configured = configure_provider_for_init(
                &args,
                &registry,
                &mut config,
                &config_path,
                &mut secret_store,
            )?;
            if selected_agent.is_some() || provider_configured || edge_requested {
                let canonical = config.to_canonical_toml()?;
                config = config::load_config_from_str(&canonical)?;
                atomic_write_owner_only(&config_path, canonical.as_bytes())?;
            }
            Ok(StepOutcome::with_payload(format!(
                r#"{{"provider_configured":{provider_configured}}}"#
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
        let log_paths = crate::runtime::workspace_init::WorkspaceLogPaths::for_run(
            &crate::runtime::workspace_init::default_workspace_init_log_base(&home),
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
                let report = crate::runtime::workspace_init::materialize_workspace(
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
            provisioned_agent_configs =
                crate::runtime::agent_headless_config::provision_agent_headless_config(
                    &config, &home,
                )?;
            Ok(StepOutcome::empty())
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
            || Ok(matches!(decision, TestflightDecision::Run).not()),
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
        finalize_run(&store, &init_run.id, INIT_RUN_FAILED)?;
        return Err(StackError::InitRunCorrupted {
            reason: format!(
                "init run {} has unsettled steps {unsettled:?}; re-run with the original flags to drive them to completion",
                init_run.id,
            ),
        });
    }
    finalize_run(&store, &init_run.id, INIT_RUN_SUCCEEDED)?;
    Ok(())
}

/// Pick the run row for this invocation. `--resume [--run-id <id>]` resumes
/// an existing row; `--fresh` always begins a new one; without either flag
/// the orchestrator begins fresh too — auto-resume would otherwise change
/// the meaning of unrelated `acps init` calls without warning.
fn resolve_init_run(args: &InitArgs, store: &StateStore) -> Result<InitRunRecord> {
    let args_json = serde_json::json!({
        "agent": args.agent,
        "provider": args.provider,
        "testflight": args.testflight,
        "skip_testflight": args.skip_testflight,
        "fresh": args.fresh,
        "resume": args.resume,
    })
    .to_string();

    if args.resume {
        let existing = if let Some(id) = args.run_id.as_deref() {
            init_runner::lookup_run(store, id)?.ok_or_else(|| StackError::InitRunCorrupted {
                reason: format!("no init run with id `{id}`"),
            })?
        } else {
            find_resumable_run(store)?.ok_or_else(|| StackError::InitRunCorrupted {
                reason: "no resumable init run found; re-run without --resume".to_owned(),
            })?
        };
        return Ok(existing);
    }

    begin_run(store, None, args.agent.as_deref(), &args_json)
}

#[derive(Default, serde::Deserialize)]
struct RecordedInitArgs {
    provider: Option<String>,
}

fn recorded_init_args(run: &InitRunRecord) -> Result<RecordedInitArgs> {
    serde_json::from_str(&run.args_json).map_err(|source| StackError::InitRunCorrupted {
        reason: format!("init run {} has invalid args_json: {source}", run.id),
    })
}

fn step_needs_resume(steps: &[InitStepRecord], kind: &str) -> bool {
    steps.iter().any(|step| {
        step.kind == kind
            && matches!(
                step.status.as_str(),
                INIT_STEP_PENDING | INIT_STEP_RUNNING | INIT_STEP_FAILED
            )
    })
}

fn finalize_with_error(store: &StateStore, run: &InitRunRecord, error: StackError) -> Result<()> {
    finalize_run(store, &run.id, INIT_RUN_FAILED)?;
    Err(error)
}

struct SecretsInitOutcome {
    status: &'static str,
}

fn perform_secrets_init(
    store_existed: bool,
    session_ref: &str,
    admin_ref: &str,
    secret_store: &mut SecretStore,
    store: &StateStore,
) -> Result<SecretsInitOutcome> {
    let session_present = secret_store.contains(session_ref);
    let admin_present = secret_store.contains(admin_ref);
    if store_existed {
        if !admin_present {
            return Err(StackError::MissingAdminKey {
                name: admin_ref.to_owned(),
            });
        }
        if !session_present {
            return Err(StackError::MissingSessionKey {
                name: session_ref.to_owned(),
            });
        }
        return Ok(SecretsInitOutcome {
            status: "preserved existing API keys",
        });
    }
    let session_value = generate_api_key();
    let admin_value = generate_api_key();
    println!("---");
    println!("session key ({session_ref}): {session_value}");
    println!("admin key ({admin_ref}): {admin_value}");
    println!(
        "save the admin key now; it is never regenerable. use `acps reset --yes` to rotate it."
    );
    println!("---");
    secret_store.set_many([
        (session_ref, session_value.as_str()),
        (admin_ref, admin_value.as_str()),
    ])?;
    store.append_event_with_source(
        "info",
        "auth.keys_generated",
        crate::state::EVENT_SOURCE_CLI,
        "generated session and admin API keys",
        &serde_json::json!({
            "session_key_ref": session_ref,
            "admin_key_ref": admin_ref,
        })
        .to_string(),
    )?;
    Ok(SecretsInitOutcome {
        status: "generated session and admin API keys",
    })
}

fn installer_postcondition_holds(
    config: &Config,
    workspace_root: &Path,
    local_bin_dir: &Path,
) -> bool {
    let (target, extra_path_dirs): (&str, Vec<&Path>) =
        if let Some(install) = config.agent.install.as_ref() {
            (install.creates.as_str(), Vec::new())
        } else {
            (config.agent.command.as_str(), vec![local_bin_dir])
        };
    crate::runtime::agent_installer::resolve_creates_for_init_resume(
        target,
        workspace_root,
        &extra_path_dirs,
    )
    .is_some()
}

fn workspace_postcondition_holds(workspace: &crate::config::WorkspaceConfig) -> bool {
    crate::runtime::workspace_init::all_sources_have_sentinel(workspace).unwrap_or(false)
}

fn init_complete_event_already_recorded(store: &StateStore, run_id: &str) -> bool {
    let Ok(events) = store.query_events(crate::state::EventFilter {
        limit: 64,
        kind: Some("init.completed"),
        ..crate::state::EventFilter::default()
    }) else {
        return false;
    };
    events
        .iter()
        .any(|event| event.payload_json.contains(run_id))
}

/// Tiny inverter so a `match` arm doesn't need its own helper.
trait BoolNot {
    fn not(self) -> bool;
}

impl BoolNot for bool {
    fn not(self) -> bool {
        !self
    }
}

/// What `acps init` should do with the post-init testflight phase. Resolved
/// from the operator's flags + TTY state + agent registry support so the
/// outer flow can render a clear log line for every path, and the test suite
/// can assert each case without exercising the real ACP bridge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TestflightDecision {
    /// All preconditions met and the operator opted in (explicit flag or
    /// interactive yes).
    Run,
    /// Operator passed `--skip-testflight`.
    SkipExplicit,
    /// Non-interactive run and `--testflight` was not passed.
    SkipNonInteractive,
    /// Interactive run and the operator answered no at the credit-warning
    /// prompt.
    SkipDeclined,
    /// Selected agent isn't headless-compatible; the testflight would fail
    /// at spawn. Surface the skip so the operator isn't surprised.
    SkipUnsupported,
}

fn resolve_testflight_decision(
    args: &InitArgs,
    config: &Config,
    registry: &RegistryCatalog,
) -> Result<Option<TestflightDecision>> {
    if args.skip_testflight {
        return Ok(Some(TestflightDecision::SkipExplicit));
    }
    let Some(entry) = registry.lookup(&config.agent.id) else {
        // Operator's `[agent].id` doesn't match the registry (e.g., escape
        // hatch). No registry entry means we don't know the testflight
        // capabilities, so don't auto-run. Surface as a separate state only
        // if the operator explicitly asked.
        if args.testflight {
            return Err(StackError::AgentRegistryMissing {
                id: config.agent.id.clone(),
            });
        }
        return Ok(None);
    };
    if !entry.headless_compatible {
        if args.testflight {
            return Err(StackError::AgentUnsupported {
                name: entry.name.clone(),
            });
        }
        return Ok(Some(TestflightDecision::SkipUnsupported));
    }
    if args.testflight {
        print_testflight_credit_warning(entry);
        return Ok(Some(TestflightDecision::Run));
    }
    if !io::stdin().is_terminal() {
        return Ok(Some(TestflightDecision::SkipNonInteractive));
    }
    if confirm_testflight_credit_warning(entry)? {
        Ok(Some(TestflightDecision::Run))
    } else {
        Ok(Some(TestflightDecision::SkipDeclined))
    }
}

fn confirm_testflight_credit_warning(entry: &RegistryEntry) -> Result<bool> {
    print_testflight_credit_warning(entry);
    print!("run testflight now? [y/N]: ");
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
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "YES" | "Yes"))
}

fn print_testflight_credit_warning(entry: &RegistryEntry) {
    println!("---");
    println!(
        "init testflight will start `{}` and send a real prompt to the configured provider.",
        entry.name
    );
    println!("this may consume provider credits.");
}

fn apply_edge_profile_to_config(args: &InitArgs, config: &mut Config) -> Result<bool> {
    let Some(edge) = args.edge else {
        return Ok(false);
    };
    match edge {
        EdgeProviderArg::Cloudflare => {}
    }
    if !matches!(args.exposure, Some(EdgeExposureArg::Tunnel)) {
        return Err(StackError::MissingField {
            field: "--exposure tunnel",
        });
    }
    let hostname = args
        .hostname
        .as_ref()
        .ok_or(StackError::MissingField {
            field: "--hostname",
        })?
        .trim()
        .to_owned();
    let public_url = format!("https://{hostname}");
    config.api.bind = "127.0.0.1:7700".to_owned();
    config.api.public_url = Some(public_url.clone());
    config.security.http.allowed_origins = vec![public_url];
    config.security.http.trust_proxy_headers = true;
    config.security.http.trusted_proxies = vec!["127.0.0.1".to_owned(), "::1".to_owned()];
    config.edge = EdgeConfig {
        cloudflare: Some(CloudflareEdgeConfig {
            enabled: true,
            mode: "generated".to_owned(),
            exposure: "tunnel".to_owned(),
            hostname,
            tunnel_name: Some("acp-stack".to_owned()),
            tunnel_id: None,
            cloudflared_deployment: args.cloudflared_deployment.as_config_value().to_owned(),
        }),
    };
    if matches!(args.cloudflared_deployment, CloudflaredDeploymentArg::Host)
        && !config
            .dependencies
            .commands
            .iter()
            .any(|entry| entry.name == "cloudflared")
    {
        config.dependencies.commands.push(DependencyEntry {
            name: "cloudflared".to_owned(),
            required: true,
            feature: Some("cloudflare-tunnel".to_owned()),
        });
    }
    Ok(true)
}

fn select_agent_for_init<'a>(
    args: &InitArgs,
    registry: &'a RegistryCatalog,
) -> Result<Option<&'a RegistryEntry>> {
    if let Some(id) = &args.agent {
        return registry
            .lookup(id)
            .ok_or_else(|| StackError::AgentRegistryMissing { id: id.clone() })
            .map(Some);
    }
    if !io::stdin().is_terminal() {
        return Ok(None);
    }
    let entries = registry.entries();
    if entries.is_empty() {
        return Ok(None);
    }
    println!("Select an agent to configure:");
    for (index, entry) in entries.iter().enumerate() {
        println!("  {}. {} ({})", index + 1, entry.name, entry.id);
    }
    print!("agent [1-{}, blank to keep current]: ", entries.len());
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
    let index = answer
        .parse::<usize>()
        .map_err(|_| StackError::InvalidParam {
            field: "agent",
            reason: format!("invalid selection `{answer}`"),
        })?;
    entries
        .get(index.saturating_sub(1))
        .ok_or_else(|| StackError::InvalidParam {
            field: "agent",
            reason: format!("selection `{answer}` is out of range"),
        })
        .map(Some)
}

fn apply_registry_entry_to_config(config: &mut Config, entry: &RegistryEntry) {
    config.agent.id = entry.id.clone();
    config.agent.name = entry.name.clone();
    config.agent.cwd = Some(config.workspace.root.clone());
    config.agent.env = default_agent_env_refs(&entry.id);
    config.agent.expected_sha256 = None;
    config.agent.restart = "on-crash".to_owned();
    config.agent.mode = None;
    config.agent.model = None;
    config.agent.harness_version = None;
    config.agent.adapter = None;
    config.agent.provider = None;
    config.agent.install = None;

    match entry.kind {
        RegistryKind::Native => {
            let harness = entry.harness.as_ref().expect("validated registry harness");
            config.agent.command = harness.id.clone();
            config.agent.args = vec!["acp".to_owned()];
        }
        RegistryKind::Adapter => {
            let adapter = entry.adapter.as_ref().expect("validated registry adapter");
            config.agent.command = adapter.id.clone();
            config.agent.args = Vec::new();
        }
    }
}

fn configure_provider_for_init(
    args: &InitArgs,
    registry: &RegistryCatalog,
    config: &mut Config,
    config_path: &Path,
    secret_store: &mut SecretStore,
) -> Result<bool> {
    let Some(provider_id) = select_provider_for_init(args, registry, config)? else {
        return Ok(false);
    };
    let required_refs = apply_provider_to_config(args, registry, config, config_path, provider_id)?;
    collect_missing_provider_refs(secret_store, &required_refs)?;
    Ok(true)
}

fn select_provider_for_init(
    args: &InitArgs,
    registry: &RegistryCatalog,
    config: &Config,
) -> Result<Option<String>> {
    if let Some(provider_id) = &args.provider {
        return Ok(Some(provider_id.clone()));
    }
    if !io::stdin().is_terminal() {
        return Ok(None);
    }
    let Some(entry) = registry.lookup(&config.agent.id) else {
        return Ok(None);
    };
    if !entry.set_provider {
        return Ok(None);
    }

    print!("provider id [blank to skip]: ");
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
        Ok(None)
    } else {
        Ok(Some(answer.to_owned()))
    }
}

fn apply_provider_to_config(
    args: &InitArgs,
    registry: &RegistryCatalog,
    config: &mut Config,
    config_path: &Path,
    provider_id: String,
) -> Result<Vec<String>> {
    let entry =
        registry
            .lookup(&config.agent.id)
            .ok_or_else(|| StackError::AgentRegistryMissing {
                id: config.agent.id.clone(),
            })?;
    if !entry.set_provider {
        return Err(StackError::AgentConfigProvision {
            path: config_path.to_path_buf(),
            reason: format!(
                "{} does not support provider configuration during init",
                config.agent.name
            ),
        });
    }
    if args.custom_provider {
        if !entry.allow_custom_provider {
            return Err(StackError::AgentConfigProvision {
                path: config_path.to_path_buf(),
                reason: format!(
                    "{} does not support custom provider setup",
                    config.agent.name
                ),
            });
        }
        if !entry.allow_custom_model {
            return Err(StackError::AgentConfigProvision {
                path: config_path.to_path_buf(),
                reason: format!("{} does not support custom model setup", config.agent.name),
            });
        }
        return apply_custom_provider_to_config(args, config, config_path, provider_id);
    }
    if !provider_id_is_known(&provider_id) {
        return Err(StackError::InvalidParam {
            field: "provider",
            reason: format!("provider `{provider_id}` is not listed in provider/env mapping"),
        });
    }
    if provider_id_is_known(&provider_id)
        && !provider_id_supports_agent(&provider_id, &config.agent.id)
    {
        return Err(StackError::InvalidParam {
            field: "provider",
            reason: format!(
                "provider `{provider_id}` is not supported for agent `{}`",
                config.agent.id
            ),
        });
    }
    if config.agent.id == "codex" && provider_id == "openai" {
        if args.api_key_ref.is_some() {
            return Err(StackError::AgentConfigProvision {
                path: config_path.to_path_buf(),
                reason: "Codex OpenAI uses Codex-native auth; do not pass --api-key-ref".to_owned(),
            });
        }
        config.agent.provider = Some(AgentProviderConfig {
            id: provider_id,
            model: None,
            api_key_ref: None,
            custom: None,
        });
        return Ok(Vec::new());
    }
    let default_api_key_ref = env_var_for_agent_provider_id(&config.agent.id, &provider_id);
    if default_api_key_ref.is_none() {
        if !entry.allow_custom_provider {
            return Err(StackError::AgentConfigProvision {
                path: config_path.to_path_buf(),
                reason: format!(
                    "{} does not support custom provider setup",
                    config.agent.name
                ),
            });
        }
        if !entry.allow_custom_model {
            return Err(StackError::AgentConfigProvision {
                path: config_path.to_path_buf(),
                reason: format!("{} does not support custom model setup", config.agent.name),
            });
        }
        if !args.custom_provider && !confirm_custom_provider_setup(&provider_id)? {
            return Err(StackError::AgentConfigProvision {
                path: config_path.to_path_buf(),
                reason: format!(
                    "provider `{provider_id}` has no API-key env mapping for agent `{}`",
                    config.agent.id
                ),
            });
        }
        return apply_custom_provider_to_config(args, config, config_path, provider_id);
    }
    let api_key_ref = args
        .api_key_ref
        .clone()
        .or_else(|| default_api_key_ref.map(str::to_owned))
        .expect("default API-key ref checked");

    let required_refs = required_env_refs_for_provider_id(&provider_id, &api_key_ref);
    for env_ref in &required_refs {
        if !config.agent.env.iter().any(|name| name == env_ref) {
            config.agent.env.push(env_ref.clone());
        }
    }
    config.agent.provider = Some(AgentProviderConfig {
        id: provider_id,
        model: None,
        api_key_ref: Some(api_key_ref),
        custom: None,
    });
    Ok(required_refs)
}

fn apply_custom_provider_to_config(
    args: &InitArgs,
    config: &mut Config,
    config_path: &Path,
    provider_id: String,
) -> Result<Vec<String>> {
    let provider_name = required_init_custom_value("provider-name", args.provider_name.clone())?;
    let base_url = required_init_custom_value("base-url", args.base_url.clone())?;
    let api_key_ref = required_init_custom_value("api-key-ref", args.api_key_ref.clone())?;
    let model = required_init_custom_value("model", args.model.clone())?;
    let model_name = args.model_name.clone().unwrap_or_else(|| model.clone());
    let api = parse_init_custom_provider_api(
        args.provider_api.as_deref(),
        default_init_custom_provider_api(&config.agent.id),
    )?;
    if config.agent.id == "codex" && api != CustomProviderApi::Responses {
        return Err(StackError::InvalidParam {
            field: "provider-api",
            reason: "Codex custom providers only support responses".to_owned(),
        });
    }
    let context = parse_init_custom_token_limit(
        "context",
        args.context.as_deref(),
        DEFAULT_CUSTOM_MODEL_CONTEXT,
    )?;
    let output_max_tokens = parse_init_custom_token_limit(
        "output-max-tokens",
        args.output_max_tokens.as_deref(),
        DEFAULT_CUSTOM_MODEL_OUTPUT_MAX_TOKENS,
    )?;
    if !config.agent.env.iter().any(|name| name == &api_key_ref) {
        config.agent.env.push(api_key_ref.clone());
    }
    config.agent.provider = Some(AgentProviderConfig {
        id: provider_id,
        model: Some(model),
        api_key_ref: Some(api_key_ref.clone()),
        custom: Some(AgentCustomProviderConfig {
            name: provider_name,
            base_url,
            api,
            model_name: Some(model_name),
            context,
            output_max_tokens,
        }),
    });
    let canonical = config.to_canonical_toml()?;
    let validated = config::load_config_from_str(&canonical)?;
    *config = validated;
    if config
        .agent
        .provider
        .as_ref()
        .and_then(|provider| provider.custom.as_ref())
        .is_none()
    {
        return Err(StackError::AgentConfigProvision {
            path: config_path.to_path_buf(),
            reason: "custom provider config was not retained".to_owned(),
        });
    }
    Ok(vec![api_key_ref])
}

fn confirm_custom_provider_setup(provider_id: &str) -> Result<bool> {
    if !io::stdin().is_terminal() {
        return Ok(false);
    }
    print!(
        "provider `{provider_id}` has no default API-key env mapping; configure it as a custom provider? [y/N]: "
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
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "YES" | "Yes"))
}

fn required_init_custom_value(field: &'static str, value: Option<String>) -> Result<String> {
    if let Some(value) = value
        && !value.trim().is_empty()
        && value.trim().len() == value.len()
    {
        return Ok(value);
    }
    if !io::stdin().is_terminal() {
        return Err(StackError::InvalidParam {
            field,
            reason: format!("--{field} is required for custom provider init"),
        });
    }
    print!("{field}: ");
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
    let answer = answer.trim().to_owned();
    if answer.is_empty() {
        return Err(StackError::InvalidParam {
            field,
            reason: format!("--{field} is required for custom provider init"),
        });
    }
    Ok(answer)
}

fn default_init_custom_provider_api(agent_id: &str) -> CustomProviderApi {
    if agent_id == "codex" {
        CustomProviderApi::Responses
    } else {
        CustomProviderApi::ChatCompletions
    }
}

fn parse_init_custom_provider_api(
    value: Option<&str>,
    default: CustomProviderApi,
) -> Result<CustomProviderApi> {
    match value {
        None => Ok(default),
        Some("chat-completions") => Ok(CustomProviderApi::ChatCompletions),
        Some("responses") => Ok(CustomProviderApi::Responses),
        Some(_) => Err(StackError::InvalidParam {
            field: "provider-api",
            reason: "must be `chat-completions` or `responses`".to_owned(),
        }),
    }
}

fn parse_init_custom_token_limit(
    field: &'static str,
    value: Option<&str>,
    default: u64,
) -> Result<u64> {
    let Some(value) = value else {
        return Ok(default);
    };
    if value.contains(',') {
        return Err(StackError::InvalidParam {
            field,
            reason: "must be a plain integer without commas".to_owned(),
        });
    }
    let parsed = value.parse::<u64>().map_err(|_| StackError::InvalidParam {
        field,
        reason: "must be a positive integer".to_owned(),
    })?;
    if parsed == 0 {
        return Err(StackError::InvalidParam {
            field,
            reason: "must be greater than 0".to_owned(),
        });
    }
    Ok(parsed)
}

fn collect_missing_provider_refs(
    secret_store: &mut SecretStore,
    required_refs: &[String],
) -> Result<()> {
    if io::stdin().is_terminal() {
        let mut collected = Vec::new();
        for env_ref in required_refs {
            if secret_store.contains(env_ref) {
                continue;
            }
            print!("{env_ref}: ");
            io::stdout()
                .flush()
                .map_err(|source| StackError::ConfigWrite {
                    path: PathBuf::from("stdout"),
                    source,
                })?;
            let mut value = String::new();
            io::stdin()
                .read_line(&mut value)
                .map_err(|source| StackError::StdinRead { source })?;
            let value = value.trim_end_matches(['\n', '\r']).to_owned();
            if !value.is_empty() {
                collected.push((env_ref.as_str(), value));
            }
        }
        secret_store.set_many(
            collected
                .iter()
                .map(|(name, value)| (*name, value.as_str())),
        )?;
    }
    for env_ref in required_refs {
        if !secret_store.contains(env_ref) {
            return Err(StackError::SecretNotFound {
                name: env_ref.clone(),
            });
        }
    }
    Ok(())
}

fn default_agent_env_refs(agent_id: &str) -> Vec<String> {
    env_refs_for_agent_id(agent_id)
        .into_iter()
        .map(str::to_owned)
        .collect()
}

fn should_install_agent(args: &InitArgs, selected_agent: bool) -> Result<bool> {
    if args.install_agent {
        return Ok(true);
    }
    if args.no_install_agent || !selected_agent || !io::stdin().is_terminal() {
        return Ok(false);
    }
    print!("Install the selected agent now? [y/N]: ");
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
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "YES" | "Yes"))
}

/// Run the installer for the configured agent. The TTY-only "try the next
/// install path?" prompt that used to live here is gone: `install_resolved`
/// already walks `shell → npm → github_release` in sequence, and any
/// remaining failure is captured by the init orchestrator's
/// `agent_install` step. The operator re-attempts by running
/// `acps init --resume`, which re-executes the failed step using the
/// current registry — picking up a newer harness version, a now-reachable
/// npm registry, or a freshly released GitHub artifact without ever
/// requiring a TTY.
fn install_configured_agent(
    home: &Path,
    config: &Config,
    registry: &RegistryCatalog,
    store: &StateStore,
) -> Result<InstallerOutcome> {
    let workspace_root = PathBuf::from(config.workspace.root.clone());
    let log_base = crate::state::default_installer_log_base(home);
    if let Some(install) = config.agent.install.as_ref() {
        let env = resolve_agent_env(home, config)?;
        return run_installer(
            &config.agent.id,
            install,
            config.agent.expected_sha256.as_deref(),
            env,
            &workspace_root,
            store,
            Some(&log_base),
        );
    }
    let entry =
        registry
            .lookup(&config.agent.id)
            .ok_or_else(|| StackError::AgentRegistryMissing {
                id: config.agent.id.clone(),
            })?;
    install_resolved(
        &config.agent,
        entry,
        Default::default(),
        &workspace_root,
        &local_bin_dir(home),
        store,
        Some(&log_base),
    )
}

fn resolve_agent_env(home: &Path, config: &Config) -> Result<HashMap<String, String>> {
    if config.agent.env.is_empty() {
        return Ok(HashMap::new());
    }
    let store = SecretStore::open(home)?;
    let mut env = HashMap::with_capacity(config.agent.env.len());
    for name in &config.agent.env {
        let value = store.get(name)?;
        env.insert(name.clone(), value.to_owned());
    }
    Ok(env)
}

fn operator_registry_override(home: &Path) -> PathBuf {
    home.join(".config").join("acp-stack").join("agents.toml")
}

fn local_bin_dir(home: &Path) -> PathBuf {
    home.join(".local").join("bin")
}

fn validate_deployment_overrides_match_existing(args: &InitArgs, config: &Config) -> Result<()> {
    reject_conflicting_deployment_override(
        "--workspace-root",
        args.workspace_root.as_deref(),
        &config.workspace.root,
    )?;
    reject_conflicting_deployment_override(
        "--workspace-uploads",
        args.workspace_uploads.as_deref(),
        &config.workspace.uploads,
    )?;
    reject_conflicting_deployment_override(
        "--runtime-user",
        args.runtime_user.as_deref(),
        &config.workspace.runtime_user,
    )
}

fn reject_conflicting_deployment_override(
    field: &'static str,
    requested: Option<&str>,
    existing: &str,
) -> Result<()> {
    let Some(requested) = requested else {
        return Ok(());
    };
    if requested == existing {
        return Ok(());
    }
    Err(StackError::InvalidParam {
        field,
        reason: format!(
            "deployment override applies only when creating a starter config; existing config has `{existing}`. Edit the config first or re-run with the existing value."
        ),
    })
}

fn starter_config(args: &InitArgs) -> Result<String> {
    let workspace_root = args
        .workspace_root
        .clone()
        .unwrap_or_else(|| config::DEFAULT_WORKSPACE_ROOT.to_owned());
    let workspace_uploads = args.workspace_uploads.clone().unwrap_or_else(|| {
        if args.workspace_root.is_some() {
            Path::new(&workspace_root)
                .join("uploads")
                .display()
                .to_string()
        } else {
            config::DEFAULT_WORKSPACE_UPLOADS.to_owned()
        }
    });
    let runtime_user = args
        .runtime_user
        .clone()
        .unwrap_or_else(|| config::DEFAULT_RUNTIME_USER.to_owned());

    let starter = Config {
        config_version: config::SUPPORTED_CONFIG_VERSION,
        api: ApiConfig {
            bind: config::DEFAULT_API_BIND.to_owned(),
            public_url: Some(format!("http://{}", config::DEFAULT_API_BIND)),
            max_request_bytes: STARTER_MAX_REQUEST_BYTES,
        },
        auth: AuthConfig {
            session_key_ref: STARTER_SESSION_KEY_REF.to_owned(),
            admin_key_ref: STARTER_ADMIN_KEY_REF.to_owned(),
        },
        security: SecurityConfig {
            http: SecurityHttpConfig {
                max_request_bytes: STARTER_MAX_REQUEST_BYTES,
                rate_limit_per_minute: STARTER_RATE_LIMIT_PER_MINUTE,
                burst: STARTER_RATE_LIMIT_BURST,
                auth_failures_per_minute: STARTER_AUTH_FAILURES_PER_MINUTE,
                auth_block_duration: STARTER_AUTH_BLOCK_DURATION.to_owned(),
                allowed_origins: Vec::new(),
                trust_proxy_headers: false,
                trusted_proxies: Vec::new(),
            },
        },
        edge: EdgeConfig::default(),
        workspace: WorkspaceConfig {
            root: workspace_root.clone(),
            uploads: workspace_uploads,
            default_shell: STARTER_DEFAULT_SHELL.to_owned(),
            runtime_user,
            max_file_bytes: STARTER_WORKSPACE_MAX_FILE_BYTES,
            code_sources: code_sources_from_args(args),
            data_sources: data_sources_from_args(args)?,
        },
        logging: LoggingConfig {
            level: STARTER_LOG_LEVEL.to_owned(),
            local_retention_days: STARTER_LOCAL_RETENTION_DAYS,
            supabase: Some(SupabaseLoggingConfig {
                enabled: false,
                url: STARTER_SUPABASE_URL.to_owned(),
                service_role_key_ref: STARTER_SUPABASE_SERVICE_ROLE_KEY_REF.to_owned(),
                schema: STARTER_SUPABASE_SCHEMA.to_owned(),
            }),
        },
        agent: AgentConfig {
            id: STARTER_AGENT_ID.to_owned(),
            name: STARTER_AGENT_NAME.to_owned(),
            command: STARTER_AGENT_COMMAND.to_owned(),
            args: Vec::new(),
            cwd: Some(workspace_root),
            env: Vec::new(),
            expected_sha256: None,
            restart: STARTER_AGENT_RESTART.to_owned(),
            mode: None,
            model: None,
            harness_version: None,
            adapter: None,
            provider: None,
            install: Some(AgentInstallConfig {
                install_type: STARTER_AGENT_INSTALL_TYPE.to_owned(),
                creates: STARTER_AGENT_INSTALL_CREATES.to_owned(),
                shell: Some(STARTER_AGENT_INSTALL_COMMAND.to_owned()),
            }),
        },
        permissions: Default::default(),
        commands: Default::default(),
        dependencies: Default::default(),
        mcp: Default::default(),
        acpctl: Default::default(),
    };

    let canonical = starter.to_canonical_toml()?;
    config::load_config_from_str(&canonical)?;
    Ok(canonical)
}

fn code_sources_from_args(args: &InitArgs) -> Vec<CodeSourceConfig> {
    args.code_from
        .iter()
        .map(|repo| CodeSourceConfig {
            source_type: "git".to_owned(),
            repo: Some(repo.clone()),
            branch: None,
            credential_ref: None,
            name: None,
        })
        .collect()
}

fn data_sources_from_args(args: &InitArgs) -> Result<Vec<DataSourceConfig>> {
    args.data_from
        .iter()
        .map(|value| classify_data_from(value))
        .collect()
}

fn classify_data_from(value: &str) -> Result<DataSourceConfig> {
    if value.strip_prefix("https://").is_some() {
        reject_unsupported_https_data_source(value)?;
        return Ok(DataSourceConfig {
            source_type: "https".to_owned(),
            name: None,
            path: None,
            url: Some(value.to_owned()),
            expected_sha256: None,
            max_download_bytes: None,
            max_extracted_bytes: None,
            bucket: None,
            prefix: None,
            region: None,
            access_key_ref: None,
            secret_key_ref: None,
        });
    }
    if value.starts_with("http://") {
        return Err(StackError::InvalidParam {
            field: "data-from",
            reason: format!("`{value}` must use https:// (http is not allowed)"),
        });
    }
    if !value.starts_with('/') {
        return Err(StackError::InvalidParam {
            field: "data-from",
            reason: format!("`{value}` must be an absolute path or an https:// URL"),
        });
    }
    Ok(DataSourceConfig {
        source_type: "local".to_owned(),
        name: None,
        path: Some(value.to_owned()),
        url: None,
        expected_sha256: None,
        max_download_bytes: None,
        max_extracted_bytes: None,
        bucket: None,
        prefix: None,
        region: None,
        access_key_ref: None,
        secret_key_ref: None,
    })
}

/// Reject HTTPS data sources that the materializer cannot satisfy headlessly.
/// Catches three known failure modes BEFORE init writes any state, so the
/// operator gets a clear error pointing at the actual URL rather than a vague
/// download/extract failure halfway through materialization.
///
/// Patterns rejected:
/// - `drive.google.com/file/d/.../view` (private file view link; needs the
///   `uc?export=download&id=` form to expose a usable HTTPS download)
/// - `drive.google.com/drive/folders/...` (folder, not an archive; the
///   materializer downloads single files)
/// - `dropbox.com/.../?dl=0` or no `dl` param (preview link; needs `?dl=1`)
fn reject_unsupported_https_data_source(value: &str) -> Result<()> {
    let lower = value.to_ascii_lowercase();
    if lower.contains("drive.google.com/file/d/")
        && !lower.contains("uc?export=download")
        && !lower.contains("uc?id=")
    {
        return Err(StackError::InvalidParam {
            field: "data-from",
            reason: format!(
                "`{value}` is a private Drive file viewer link; pass the `https://drive.google.com/uc?export=download&id=<ID>` form instead"
            ),
        });
    }
    if lower.contains("drive.google.com/drive/folders/") {
        return Err(StackError::InvalidParam {
            field: "data-from",
            reason: format!(
                "`{value}` is a Drive folder; init only supports single-archive downloads. Export the folder as an archive and link to the archive."
            ),
        });
    }
    if lower.contains("dropbox.com/") && !lower.contains("dl=1") && !lower.contains("raw=1") {
        return Err(StackError::InvalidParam {
            field: "data-from",
            reason: format!(
                "`{value}` is a Dropbox preview link; append `?dl=1` so the materializer receives the file bytes"
            ),
        });
    }
    Ok(())
}
