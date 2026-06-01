mod headless_snapshot;
mod install;
mod model_mode;
mod provider;
mod registry_apply;
mod resume;
mod skills;
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
use crate::runtime::install::skill_installer::SkillInstallReport;
use crate::runtime::install::skill_registry::SkillCatalog;
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
use self::skills::{
    install_init_skills, prompt_init_skills_if_needed, resolve_skill_install_plan,
    skill_install_postcondition_holds,
};
use self::starter_config::{
    prompt_starter_config_selections_if_needed, reject_starter_only_mcp_args_for_existing_config,
    starter_config, validate_deployment_overrides_match_existing,
};
use self::testflight::{TestflightDecision, resolve_testflight_decision};
use super::logging::{
    SUPABASE_API_KEY_REF_ENV, SUPABASE_DEFAULT_API_KEY_REF, SUPABASE_DEFAULT_SCHEMA,
    SUPABASE_ENABLED_ENV, SUPABASE_SCHEMA_ENV, SUPABASE_URL_ENV, apply_supabase_config,
    disabled_supabase_config, enabled_supabase_config, ensure_supabase_secret,
};

#[derive(Debug, Args)]
pub struct InitArgs {
    /// Select the configured agent non-interactively from the registry.
    #[arg(long)]
    pub(super) agent: Option<String>,
    /// Confirm that init is running without prompts. Non-interactive first
    /// runs must also pass `--agent <id>`.
    #[arg(long)]
    pub(super) non_interactive: bool,
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
    /// Skills marketplace/source: openai, anthropic, or github:<owner>.
    #[arg(
        long = "skills-source",
        requires = "skills",
        conflicts_with = "no_skills"
    )]
    pub(super) skills_source: Option<String>,
    /// Comma-separated dash-case Agent Skills to install during init.
    #[arg(
        long = "skills",
        value_name = "NAME",
        value_delimiter = ',',
        requires = "skills_source",
        conflicts_with = "no_skills"
    )]
    pub(super) skills: Vec<String>,
    /// Skip the Agent Skills prompt in interactive runs.
    #[arg(long, conflicts_with_all = ["skills_source", "skills"])]
    pub(super) no_skills: bool,
    /// Configure a public edge profile during init.
    #[arg(long, value_enum)]
    pub(super) edge: Option<EdgeProviderArg>,
    /// Public exposure model for the selected edge provider.
    #[arg(long, value_enum, requires = "edge")]
    pub(super) exposure: Option<EdgeExposureArg>,
    /// Public hostname for the edge profile, for example agent.example.com.
    #[arg(long, requires = "edge")]
    pub(super) hostname: Option<String>,
    /// Cloudflare setup mode: generated artifacts only or managed API provisioning.
    #[arg(
        long = "cloudflare-mode",
        value_enum,
        requires = "edge",
        default_value_t = CloudflareModeArg::Generated
    )]
    pub(super) cloudflare_mode: CloudflareModeArg,
    /// Secret ref containing a Cloudflare API token for managed provisioning.
    #[arg(long = "cloudflare-api-token-ref", requires = "edge")]
    pub(super) cloudflare_api_token_ref: Option<String>,
    /// Secret ref containing the Cloudflare account id for managed provisioning.
    #[arg(long = "cloudflare-account-id-ref", requires = "edge")]
    pub(super) cloudflare_account_id_ref: Option<String>,
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
    /// Add an MCP preset during init. Currently supports `linear`.
    #[arg(long = "mcp-preset", value_name = "NAME", value_delimiter = ',')]
    pub(super) mcp_preset: Vec<String>,
    /// Add a custom stdio MCP server as `name=command`.
    #[arg(long = "mcp-stdio", value_name = "NAME=COMMAND")]
    pub(super) mcp_stdio: Vec<String>,
    /// Add a secret ref to a custom stdio MCP server as `server=SECRET_REF`.
    #[arg(long = "mcp-stdio-env", value_name = "SERVER=SECRET_REF")]
    pub(super) mcp_stdio_env: Vec<String>,
    /// Add a custom HTTP MCP server as `name=https://...`.
    #[arg(long = "mcp-http", value_name = "NAME=URL")]
    pub(super) mcp_http: Vec<String>,
    /// Add a header secret ref to a custom HTTP MCP server as `server=Header:SECRET_REF`.
    #[arg(long = "mcp-http-header", value_name = "SERVER=HEADER:SECRET_REF")]
    pub(super) mcp_http_header: Vec<String>,
    /// Enable Supabase external logging during init.
    #[arg(long = "supabase-url", conflicts_with = "no_supabase")]
    pub(super) supabase_url: Option<String>,
    /// Supabase schema exposed through the Data API.
    #[arg(long = "supabase-schema", conflicts_with = "no_supabase")]
    pub(super) supabase_schema: Option<String>,
    /// Secret ref containing the Supabase secret API key.
    #[arg(long = "supabase-api-key-ref", conflicts_with = "no_supabase")]
    pub(super) supabase_api_key_ref: Option<String>,
    /// Leave Supabase external logging disabled during init.
    #[arg(long = "no-supabase")]
    pub(super) no_supabase: bool,
    /// Skip the workspace materializer; useful for tests and dev loops that
    /// do not need actual content fetched/cloned.
    #[cfg(feature = "dev-tools")]
    #[arg(long, hide = true)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum InitMode {
    Operator,
    Dev,
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub(super) enum EdgeProviderArg {
    Cloudflare,
}

impl EdgeProviderArg {
    pub(super) fn as_config_value(self) -> &'static str {
        match self {
            Self::Cloudflare => "cloudflare",
        }
    }

    pub(super) fn from_config_value(value: &str) -> Option<Self> {
        match value {
            "cloudflare" => Some(Self::Cloudflare),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub(super) enum EdgeExposureArg {
    Tunnel,
}

impl EdgeExposureArg {
    pub(super) fn as_config_value(self) -> &'static str {
        match self {
            Self::Tunnel => "tunnel",
        }
    }

    pub(super) fn from_config_value(value: &str) -> Option<Self> {
        match value {
            "tunnel" => Some(Self::Tunnel),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub(super) enum CloudflareModeArg {
    Generated,
    Managed,
}

impl CloudflareModeArg {
    pub(super) fn as_config_value(self) -> &'static str {
        match self {
            Self::Generated => "generated",
            Self::Managed => "managed",
        }
    }

    pub(super) fn from_config_value(value: &str) -> Option<Self> {
        match value {
            "generated" => Some(Self::Generated),
            "managed" => Some(Self::Managed),
            _ => None,
        }
    }
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

    pub(super) fn from_config_value(value: &str) -> Option<Self> {
        match value {
            "host" => Some(Self::Host),
            "docker" => Some(Self::Docker),
            "external" => Some(Self::External),
            _ => None,
        }
    }
}

impl InitArgs {
    pub(super) fn skip_workspace_init(&self) -> bool {
        #[cfg(feature = "dev-tools")]
        {
            self.skip_workspace_init
        }
        #[cfg(not(feature = "dev-tools"))]
        {
            false
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
            "inherit main provider/model for {}? declining disables it. [Y/n]: ",
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
            config.agent.subagent = Some(AgentSubagentConfig {
                disabled: true,
                provider: None,
            });
            println!(
                "subagent model disabled; run `acps subagent set` to configure, or `acps subagent match` to inherit later"
            );
            return Ok(true);
        }
    }
    // Accept path leaves `subagent` unset on purpose: under the new
    // "absent = inherit" semantic, no mirror of the main provider is needed.
    Ok(true)
}

fn apply_supabase_env_defaults(args: &mut InitArgs) -> Result<()> {
    let explicit_supabase_args = args.supabase_url.is_some()
        || args.supabase_schema.is_some()
        || args.supabase_api_key_ref.is_some();

    if args.no_supabase {
        return Ok(());
    }

    let enabled = match env_value(SUPABASE_ENABLED_ENV) {
        Some(value) => Some(parse_supabase_enabled_env(&value)?),
        None => None,
    };

    if enabled == Some(false) && !explicit_supabase_args {
        args.no_supabase = true;
        return Ok(());
    }

    if args.supabase_url.is_none() {
        args.supabase_url = env_value(SUPABASE_URL_ENV);
    }
    if args.supabase_schema.is_none() {
        args.supabase_schema = env_value(SUPABASE_SCHEMA_ENV);
    }
    if args.supabase_api_key_ref.is_none() {
        args.supabase_api_key_ref = env_value(SUPABASE_API_KEY_REF_ENV);
    }

    if enabled == Some(true) && args.supabase_url.is_none() {
        return Err(StackError::MissingField {
            field: SUPABASE_URL_ENV,
        });
    }

    if args.supabase_url.is_none()
        && (args.supabase_schema.is_some() || args.supabase_api_key_ref.is_some())
    {
        return Err(StackError::InvalidParam {
            field: "--supabase-url",
            reason: "required when setting Supabase schema or API-key ref during init".to_owned(),
        });
    }

    Ok(())
}

fn env_value(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn parse_supabase_enabled_env(value: &str) -> Result<bool> {
    match value {
        "1" | "true" | "TRUE" | "yes" | "YES" => Ok(true),
        "0" | "false" | "FALSE" | "no" | "NO" => Ok(false),
        _ => Err(StackError::InvalidParam {
            field: SUPABASE_ENABLED_ENV,
            reason: "must be 0, 1, true, false, yes, or no".to_owned(),
        }),
    }
}

fn apply_supabase_to_config_for_init(args: &InitArgs, config: &mut Config) -> Result<bool> {
    if args.no_supabase {
        let mut supabase = config
            .logging
            .supabase
            .clone()
            .unwrap_or_else(disabled_supabase_config);
        supabase.enabled = false;
        return apply_supabase_config(config, supabase);
    }

    let Some(url) = args.supabase_url.clone() else {
        return Ok(false);
    };
    apply_supabase_config(
        config,
        enabled_supabase_config(
            url,
            Some(
                args.supabase_schema
                    .clone()
                    .unwrap_or_else(|| SUPABASE_DEFAULT_SCHEMA.to_owned()),
            ),
            Some(
                args.supabase_api_key_ref
                    .clone()
                    .unwrap_or_else(|| SUPABASE_DEFAULT_API_KEY_REF.to_owned()),
            ),
        ),
    )
}

fn reject_supabase_init_args_for_existing_config(args: &InitArgs) -> Result<()> {
    if args.supabase_url.is_some()
        || args.supabase_schema.is_some()
        || args.supabase_api_key_ref.is_some()
        || args.no_supabase
    {
        return Err(StackError::InvalidParam {
            field: "--supabase-url",
            reason: "Supabase init setup applies only when creating a starter config; use `acps logging supabase` for initialized instances".to_owned(),
        });
    }
    Ok(())
}

pub(super) fn run_init(mut args: InitArgs, mode: InitMode) -> Result<()> {
    if args.skip_workspace_init() && mode != InitMode::Dev {
        return Err(StackError::InvalidParam {
            field: "--skip-workspace-init",
            reason: "development-only flag; use `acps dev init --skip-workspace-init`".to_owned(),
        });
    }

    let home = home_dir()?;
    let config_path = config::default_config_path()?;
    let state_path = default_state_path(&home);
    let config_dir = parent_dir(&config_path)?;
    let state_dir = parent_dir(&state_path)?;

    create_dir_owner_only(config_dir)?;
    create_dir_owner_only(state_dir)?;
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
    }
    if creating_config && !args.resume && args.agent.is_none() {
        if !io::stdin().is_terminal() {
            return Err(StackError::InvalidParam {
                field: "--agent",
                reason: "non-interactive init requires selecting a real agent; run `acps init` in a TTY or pass `--non-interactive --agent <id>`".to_owned(),
            });
        }
        let selected =
            select_agent_for_init(&args, &registry)?.ok_or_else(|| StackError::InvalidParam {
                field: "--agent",
                reason: "initializing a new config requires selecting a real agent".to_owned(),
            })?;
        args.agent = Some(selected.id.clone());
    }
    if creating_config && !args.resume {
        prompt_starter_config_selections_if_needed(&mut args)?;
    }

    let config_status = if config_path.exists() {
        // Repair perms before validation so a failure to parse the file does not
        // leave a permissive config on disk; matches the behavior of `acps status`.
        set_owner_only_file(&config_path)?;
        let existing_config = Config::load_from_path(&config_path)?;
        validate_deployment_overrides_match_existing(&args, &existing_config)?;
        reject_starter_only_mcp_args_for_existing_config(&args)?;
        "validated existing config"
    } else {
        let starter_config = starter_config(&args)?;
        let mut new_config = config::load_config_from_str(&starter_config)?;
        if let Some(agent_id) = args.agent.as_deref() {
            let entry = registry.lookup_required(agent_id)?;
            entry.ensure_supported()?;
            apply_registry_entry_to_config(&mut new_config, entry);
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
        println!("resuming init run {}", init_run.id);
    } else {
        println!("init run {}", init_run.id);
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
    let supabase_configured = apply_supabase_to_config_for_init(&args, &mut config)?;
    prompt_init_skills_if_needed(&mut args, &config, &registry)?;
    if selected_agent.is_some() || edge_requested || supabase_configured {
        let canonical = config.to_canonical_toml()?;
        config = config::load_config_from_str(&canonical)?;
        atomic_write_owner_only(&config_path, canonical.as_bytes())?;
    }

    if resumed
        && !args.no_skills
        && args.skills_source.is_none()
        && args.skills.is_empty()
        && let Some(recorded) = recorded_args.as_ref()
    {
        args.skills_source = recorded.skills_source.clone();
        args.skills = recorded.skills.clone();
        args.no_skills = recorded.no_skills;
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
    if step_needs_resume(&prior_init_steps, step_kind::PROVIDER_CONFIGURE)
        && args.provider.is_none()
    {
        args.provider = config
            .agent
            .provider
            .as_ref()
            .map(|provider| provider.id.clone());
        // A failed provider_configure step that owned ONLY model/mode (no
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
    let skill_catalog = SkillCatalog::load_embedded()?;
    let skill_install_plan =
        resolve_skill_install_plan(&args, &home, &config, &registry, &skill_catalog)?;

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
    println!("progress: initializing secrets");
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
    if let Some(supabase) = config.logging.supabase.as_ref()
        && supabase.enabled
    {
        let stored = match ensure_supabase_secret(
            &mut secret_store,
            &supabase.api_key_ref,
            io::stdin().is_terminal() && !args.non_interactive,
        ) {
            Ok(stored) => stored,
            Err(error) => return finalize_with_error(&store, &init_run, error),
        };
        if stored {
            println!("supabase secret: set ({})", supabase.api_key_ref);
        } else {
            println!("supabase secret: preserved ({})", supabase.api_key_ref);
        }
    }

    // -----------------------------------------------------------------
    // Step 2: agent_install — install the configured agent if requested.
    // -----------------------------------------------------------------
    let install_requested = should_install_agent(&config, &registry)?;
    let mut install_outcome: Option<InstallerOutcome> = None;
    let install_step_needs_resume = step_needs_resume(&prior_init_steps, step_kind::AGENT_INSTALL);
    if install_requested || install_step_needs_resume {
        println!("progress: installing agent");
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
    // Step 3: agent_skills_install — install selected Agent Skills before
    // first launch/testflight. Agent harnesses auto-detect the files.
    // -----------------------------------------------------------------
    let mut skill_install_report: Option<SkillInstallReport> = None;
    let skill_step_needs_resume =
        step_needs_resume(&prior_init_steps, step_kind::AGENT_SKILLS_INSTALL);
    if skill_install_plan.is_some() || skill_step_needs_resume {
        println!("progress: installing agent skills");
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
        let result = record_step(
            &store,
            &init_run,
            9,
            step_kind::AGENT_SKILLS_INSTALL,
            || Ok(skill_install_postcondition_holds(&verify_plan)),
            || {
                let report = install_init_skills(&plan)?;
                let payload = serde_json::to_string(&report).map_err(|source| {
                    StackError::SkillInstallFailed {
                        reason: format!("serialize skill install report: {source}"),
                    }
                })?;
                skill_install_report = Some(report);
                Ok(StepOutcome::with_payload(payload))
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
    println!("progress: configuring provider and model");
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
    if !args.skip_workspace_init()
        || step_needs_resume(&prior_init_steps, step_kind::WORKSPACE_MATERIALIZE)
    {
        println!("progress: materializing workspace sources");
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
    } else {
        println!("workspace: skipped (--skip-workspace-init)");
    }

    // -----------------------------------------------------------------
    // Step 5: agent_headless_config — write the agent's local config
    // files so the harness can start without first-run prompts.
    // -----------------------------------------------------------------
    let mut provisioned_agent_configs = Vec::new();
    println!("progress: writing agent headless config");
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
        println!("progress: preparing Cloudflare edge artifacts");
        let result = record_step(
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
    if let Some(report) = skill_install_report {
        for entry in report.installed {
            println!(
                "skill installed: {} -> {}",
                entry.name,
                entry.path.display()
            );
        }
        for entry in report.skipped {
            println!("skill already installed: {}", entry.name);
        }
    }
    for provisioned in provisioned_agent_configs {
        println!("{}: {}", provisioned.label, provisioned.path.display());
    }
    for artifact in provisioned_edge_artifacts {
        println!("{}: {}", artifact.label, artifact.path.display());
    }
    if let Some(materialize) = &materialize_report {
        println!("workspace root: {}", materialize.root.display());
        println!("workspace uploads: {}", materialize.uploads.display());
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
    // Step 8: testflight — optional real-prompt test. Decision uses
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
    crate::runtime::init_runner::finalize_run(&store, &init_run.id, INIT_RUN_SUCCEEDED)?;
    Ok(())
}
