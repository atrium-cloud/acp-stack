mod headless_snapshot;
mod install;
mod model_mode;
mod prompt;
mod provider;
mod registry_apply;
mod resume;
mod skills;
mod starter_config;
mod testflight;

use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};

use clap::{Args, ValueEnum};

use crate::config::{self, AgentSubagentConfig, Config, DataSourceConfig};
use crate::error::{Result, StackError};
use crate::fs_util::{
    atomic_write_owner_only, create_dir_owner_only, home_dir, parent_dir, pre_create_owner_only,
    set_owner_only_file, write_new_file_owner_only,
};
use crate::runtime::agent::agent_headless_config::OPENCODE_AGENT_ID;
use crate::runtime::dependencies::deps_apply::{
    DepApplyOutcome, apply_dependencies_with_progress, pending_candidates,
};
use crate::runtime::init_runner::{StepDisposition, StepOutcome, record_step, step_kind};
use crate::runtime::install::agent_installer::InstallerOutcome;
use crate::runtime::install::agent_registry::RegistryCatalog;
use crate::runtime::install::skill_installer::SkillInstallReport;
use crate::runtime::install::skill_registry::SkillCatalog;
use crate::secrets::{SecretStore, age_key_path};
use crate::state::{
    INIT_RUN_FAILED, INIT_RUN_SUCCEEDED, INIT_STEP_FAILED, INIT_STEP_PENDING, INIT_STEP_RUNNING,
    StateStore, default_state_path,
};

use self::headless_snapshot::{
    capture_dir_listings_for, capture_path_snapshots, headless_config_candidate_paths,
    headless_config_side_dirs, remove_new_files_in_dirs, restore_headless_snapshots,
};
use self::install::{
    MAX_INSTALL_ATTEMPTS, install_configured_agent, local_bin_dir, operator_registry_override,
    run_install_with_retry, should_install_agent,
};
use self::model_mode::{
    ModelModeAction, configure_model_and_mode_for_init, preflight_model_and_mode_for_init,
    verify_agent_acp_connection,
};
use self::provider::{
    configure_provider_for_init, configured_provider_refs_satisfied, preflight_provider_for_init,
};
use self::registry_apply::{
    AgentSelection, CustomAgentSpec, apply_custom_agent_to_config, apply_edge_profile_to_config,
    apply_registry_entry_to_config, is_custom_agent, reject_registry_id_for_custom_agent,
    resolve_custom_agent_spec, select_agent_for_init,
};
use self::resume::{
    FreshKeys, finalize_with_error, init_complete_event_already_recorded,
    installer_postcondition_holds, perform_auth_init, recorded_init_args, resolve_init_run,
    step_needs_resume, workspace_postcondition_holds,
};
use self::skills::{
    install_init_skills, prompt_init_skills_if_needed, resolve_skill_install_plan,
    skill_install_postcondition_holds,
};
use self::starter_config::{
    AgentEnvCollection, append_agent_env_refs, apply_agent_env_collection,
    collect_agent_env_refs_for_init, configure_stack_update_for_init,
    prompt_starter_config_selections_if_needed, push_args_deps_to_config,
    reject_agent_env_refs_for_existing_config, reject_deps_args_for_existing_config,
    reject_starter_only_mcp_args_for_existing_config, should_apply_deps_for_init, starter_config,
    validate_deployment_overrides_match_existing, validate_stack_update_args,
};
use self::testflight::{TestflightDecision, resolve_testflight_decision};
use super::config as cli_config;
use super::logging::{
    SUPABASE_API_KEY_REF_ENV, SUPABASE_DEFAULT_API_KEY_REF, SUPABASE_DEFAULT_SCHEMA,
    SUPABASE_ENABLED_ENV, SUPABASE_SCHEMA_ENV, SUPABASE_URL_ENV, apply_supabase_config,
    disabled_supabase_config, enabled_supabase_config, ensure_supabase_secret,
};

#[derive(Debug, Clone)]
pub(super) struct InitMcpStdioServer {
    pub(super) name: String,
    pub(super) command: String,
    pub(super) args: Vec<String>,
    pub(super) env: Vec<String>,
}

#[derive(Debug, Clone)]
pub(super) struct InitMcpHttpServer {
    pub(super) name: String,
    pub(super) url: String,
    pub(super) headers: Vec<InitMcpHttpHeader>,
}

#[derive(Debug, Clone)]
pub(super) struct InitMcpHttpHeader {
    pub(super) name: String,
    pub(super) value_ref: String,
}

#[derive(Debug, Args)]
pub struct InitArgs {
    /// Select the configured agent non-interactively from the registry.
    #[arg(long)]
    pub(super) agent: Option<String>,
    /// Define a custom (non-registry) agent by id. Requires
    /// `--custom-agent-command` and `--custom-agent-install`. The agent is
    /// modeled via `[agent.install]`; provider/model are configured through the
    /// agent's own env, not these init flags.
    #[arg(
        long = "custom-agent-id",
        value_name = "ID",
        conflicts_with_all = ["agent", "provider", "model", "custom_provider"]
    )]
    pub(super) custom_agent_id: Option<String>,
    /// Display name for the custom agent (defaults to the id).
    #[arg(
        long = "custom-agent-name",
        value_name = "NAME",
        requires = "custom_agent_id"
    )]
    pub(super) custom_agent_name: Option<String>,
    /// Launch command (binary on PATH) for the custom agent.
    #[arg(
        long = "custom-agent-command",
        value_name = "CMD",
        requires = "custom_agent_id"
    )]
    pub(super) custom_agent_command: Option<String>,
    /// Launch argument for the custom agent. Repeatable.
    #[arg(
        long = "custom-agent-arg",
        value_name = "ARG",
        requires = "custom_agent_id"
    )]
    pub(super) custom_agent_arg: Vec<String>,
    /// Shell snippet that installs the custom agent (and its adapter, if any).
    #[arg(
        long = "custom-agent-install",
        value_name = "SHELL",
        requires = "custom_agent_id"
    )]
    pub(super) custom_agent_install: Option<String>,
    /// Path that must resolve to an executable after install (defaults to the
    /// launch command).
    #[arg(
        long = "custom-agent-creates",
        value_name = "PATH",
        requires = "custom_agent_id"
    )]
    pub(super) custom_agent_creates: Option<String>,
    /// Reference an existing secret as an environment variable for the agent
    /// process. Repeatable. The secret must already be in the store. Interactive
    /// optional setup can collect masked values. Applies only when creating a
    /// new config.
    #[arg(long = "agent-env-ref", value_name = "NAME")]
    pub(super) agent_env_ref: Vec<String>,
    /// Declare a user-scope dependency install action as NAME=SHELL. Repeatable.
    /// New config only.
    #[arg(long = "dep", value_name = "NAME=SHELL")]
    pub(super) dep: Vec<String>,
    /// Declare a system-scope (privileged) dependency install action as
    /// NAME=SHELL. Repeatable. New config only.
    #[arg(long = "dep-system", value_name = "NAME=SHELL")]
    pub(super) dep_system: Vec<String>,
    /// Run declared dependency install actions during init (opt-in).
    #[arg(long = "deps-apply")]
    pub(super) deps_apply: bool,
    /// Skip the dependency-apply confirmation; required for non-interactive
    /// dependency apply.
    #[arg(long = "deps-apply-yes", requires = "deps_apply")]
    pub(super) deps_apply_yes: bool,
    /// acp-stack auto-update policy: on (all compatible), security (security
    /// updates only), or off (manual).
    #[arg(long = "stack-update", value_name = "on|security|off")]
    pub(super) stack_update: Option<String>,
    /// acp-stack auto-update frequency (day/week units, e.g. 1d, 3w; minimum 1
    /// day). Ignored when the policy is off.
    #[arg(
        long = "stack-update-frequency",
        value_name = "FREQ",
        requires = "stack_update"
    )]
    pub(super) stack_update_frequency: Option<String>,
    /// Confirm that init is running without prompts. Non-interactive first
    /// runs must also pass `--agent <id>`.
    #[arg(long)]
    pub(super) non_interactive: bool,
    /// Emit the platform automation handoff payload as the only stdout output.
    #[arg(long = "handoff-json")]
    pub(super) handoff_json: bool,
    /// Initialize from an existing acps-config.toml file.
    #[arg(
        long = "from-file",
        value_name = "PATH",
        conflicts_with_all = ["from_toml", "from_base64", "resume"]
    )]
    pub(super) from_file: Option<PathBuf>,
    /// Initialize from pasted acps-config.toml text.
    #[arg(
        long = "from-toml",
        value_name = "TOML",
        conflicts_with_all = ["from_file", "from_base64", "resume"]
    )]
    pub(super) from_toml: Option<String>,
    /// Initialize from base64-encoded acps-config.toml text.
    #[arg(
        long = "from-base64",
        value_name = "BASE64",
        conflicts_with_all = ["from_file", "from_toml", "resume"]
    )]
    pub(super) from_base64: Option<String>,
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
    /// API family for a custom provider: chat-completions, responses, or anthropic-messages.
    #[arg(long = "provider-api", requires = "custom_provider")]
    pub(super) provider_api: Option<String>,
    /// Initial model id. With `--custom-provider`, taken verbatim as the
    /// custom model id. Otherwise validated against the agent's
    /// ACP-advertised `model` values discovered via a provisional
    /// session.
    #[arg(long)]
    pub(super) model: Option<String>,
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
    /// Skip Agent Skills during init.
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
    #[arg(skip)]
    pub(super) prompt_agent_env_refs: bool,
    #[arg(skip)]
    pub(super) prompt_skills: bool,
    #[arg(skip)]
    pub(super) prompt_data_sources: Vec<DataSourceConfig>,
    #[arg(skip)]
    pub(super) prompt_mcp_stdio: Vec<InitMcpStdioServer>,
    #[arg(skip)]
    pub(super) prompt_mcp_http: Vec<InitMcpHttpServer>,
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

    pub(super) fn config_import_source_label(&self) -> Option<&'static str> {
        if self.from_file.is_some() {
            Some("file")
        } else if self.from_toml.is_some() {
            Some("toml")
        } else if self.from_base64.is_some() {
            Some("base64")
        } else {
            None
        }
    }
}

pub(super) const STARTER_MAX_REQUEST_BYTES: u64 = 104_857_600;
pub(super) const STARTER_RATE_LIMIT_PER_MINUTE: u64 = 120;
pub(super) const STARTER_RATE_LIMIT_BURST: u64 = 30;
pub(super) const STARTER_AUTH_FAILURES_PER_MINUTE: u64 = 5;
pub(super) const STARTER_AUTH_BLOCK_DURATION: &str = "15m";
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
    interactive: bool,
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
    let alias = entry.subagent_alias.as_deref().unwrap_or("subagent");
    // Default-yes: declining (or a non-interactive run) inherits the main
    // provider/model by leaving `subagent` unset under the "absent = inherit"
    // semantic; only an explicit "no" disables it.
    if prompt::confirm(
        interactive,
        &format!("inherit main provider/model for {alias}? declining disables it."),
        true,
    )? {
        return Ok(true);
    }
    config.agent.subagent = Some(AgentSubagentConfig {
        disabled: true,
        provider: None,
    });
    println!(
        "subagent model disabled; run `acps subagent set` to configure, or `acps subagent match` to inherit later"
    );
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

const KEY_HANDOVER_PRINTED_EVENT: &str = "auth.keys_handover_printed";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InitOutputMode {
    Text,
    HandoffJson,
}

impl InitOutputMode {
    fn is_text(self) -> bool {
        matches!(self, Self::Text)
    }

    fn is_handoff_json(self) -> bool {
        matches!(self, Self::HandoffJson)
    }
}

macro_rules! init_println {
    ($output:expr, $($arg:tt)*) => {
        if $output.is_text() {
            println!($($arg)*);
        }
    };
}

#[derive(Debug, Clone)]
struct InitHandoffContext {
    config_path: PathBuf,
    state_path: PathBuf,
    secret_store_path: PathBuf,
    age_key_path: PathBuf,
    agent_id: String,
    agent_name: String,
}

/// Drop guard that performs the session/admin key handover as the very last
/// thing the operator sees. Holding the plaintext across the whole run (instead
/// of printing at generation time) keeps the keys from scrolling off-screen
/// behind install/workspace/testflight output; rendering on Drop means a fresh
/// run that fails AFTER key generation still surfaces the otherwise
/// unrecoverable, non-regenerable admin key before init exits. In handoff JSON
/// mode, preserved keys are reported without reprinting plaintext material.
struct KeyHandover {
    keys: Option<FreshKeys>,
    output_mode: InitOutputMode,
    failure_context: Option<InitHandoffContext>,
    auth_ready: bool,
    emitted: bool,
}

impl Drop for KeyHandover {
    fn drop(&mut self) {
        if self.emitted {
            return;
        }
        match self.output_mode {
            InitOutputMode::Text => {
                self.print_text();
            }
            InitOutputMode::HandoffJson => {
                self.print_failed_json();
            }
        }
    }
}

impl KeyHandover {
    fn print_text(&mut self) -> Option<(String, String)> {
        let keys = self.keys.take()?;
        println!("---");
        println!("session key: {}", keys.session_value.as_str());
        println!("admin key: {}", keys.admin_value.as_str());
        println!(
            "save the admin key now; it is never regenerable. use `acps reset --yes` to rotate it."
        );
        println!("---");
        Some(("session".to_owned(), "admin".to_owned()))
    }

    fn print_and_record(&mut self, store: &StateStore, run_id: &str) -> Result<()> {
        if self.keys.is_some() {
            self.record(store, run_id)?;
            self.print_text();
        }
        Ok(())
    }

    fn record(&self, store: &StateStore, run_id: &str) -> Result<()> {
        if self.keys.is_none() {
            return Ok(());
        }
        store.append_event_with_source(
            "info",
            KEY_HANDOVER_PRINTED_EVENT,
            crate::state::EVENT_SOURCE_CLI,
            "session and admin API keys were shown to the operator",
            &serde_json::json!({
                "init_run_id": run_id,
                "key_kinds": ["session", "admin"],
            })
            .to_string(),
        )?;
        Ok(())
    }

    fn print_handoff_json(
        &mut self,
        status: &'static str,
        context: &InitHandoffContext,
    ) -> Result<()> {
        let payload = init_handoff_payload(status, context, self.keys.as_ref());
        let rendered =
            serde_json::to_string_pretty(&payload).map_err(|source| StackError::ServeIo {
                source: std::io::Error::other(format!("serialize init handoff JSON: {source}")),
            })?;
        println!("{rendered}");
        self.keys.take();
        self.emitted = true;
        Ok(())
    }

    fn print_failed_json(&mut self) {
        let Some(context) = self.failure_context.as_ref() else {
            return;
        };
        if !self.auth_ready {
            return;
        }
        let payload = init_handoff_payload("failed", context, self.keys.as_ref());
        match serde_json::to_string_pretty(&payload) {
            Ok(rendered) => println!("{rendered}"),
            Err(error) => eprintln!("failed to serialize init handoff JSON: {error}"),
        }
        self.keys.take();
        self.emitted = true;
    }
}

fn init_handoff_payload(
    status: &'static str,
    context: &InitHandoffContext,
    fresh_keys: Option<&FreshKeys>,
) -> serde_json::Value {
    let generated_keys = if fresh_keys.is_some() {
        serde_json::json!(["session", "admin"])
    } else {
        serde_json::json!([])
    };
    let preserved_keys = if fresh_keys.is_some() {
        serde_json::json!([])
    } else {
        serde_json::json!(["session", "admin"])
    };
    let mut payload = serde_json::json!({
        "status": status,
        "config_path": context.config_path.display().to_string(),
        "state_path": context.state_path.display().to_string(),
        "secret_store_path": context.secret_store_path.display().to_string(),
        "age_key_path": context.age_key_path.display().to_string(),
        "agent": {
            "id": context.agent_id,
            "name": context.agent_name,
        },
        "auth": {
            "generated_keys": generated_keys,
            "preserved_keys": preserved_keys,
        },
    });
    if let Some(keys) = fresh_keys {
        let object = payload
            .as_object_mut()
            .expect("init handoff payload is an object");
        object.insert(
            "session_key".to_owned(),
            serde_json::Value::String(keys.session_value.as_str().to_owned()),
        );
        object.insert(
            "admin_key".to_owned(),
            serde_json::Value::String(keys.admin_value.as_str().to_owned()),
        );
    }
    payload
}

/// Whether init should drive interactive prompts: a real TTY and no
/// prompt-suppressing automation flags. The single source of truth for the
/// gate, so every prompt site honors the same contract.
fn prompts_enabled_for(args: &InitArgs, stdin_is_terminal: bool) -> bool {
    stdin_is_terminal && !args.non_interactive && !args.handoff_json
}

pub(super) fn prompts_enabled(args: &InitArgs) -> bool {
    prompts_enabled_for(args, io::stdin().is_terminal())
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
        items.push((
            ConfigSourceChoice::Resume,
            "Resume interrupted init".to_owned(),
            String::new(),
        ));
        items.push((
            ConfigSourceChoice::ContinueExisting,
            "Continue with existing config".to_owned(),
            String::new(),
        ));
    } else {
        items.push((
            ConfigSourceChoice::ImportFile,
            "Import acps-config.toml path".to_owned(),
            String::new(),
        ));
        items.push((
            ConfigSourceChoice::PasteBase64,
            "Paste base64 acps-config.toml".to_owned(),
            String::new(),
        ));
        items.push((
            ConfigSourceChoice::StartFresh,
            "Start fresh".to_owned(),
            String::new(),
        ));
    }

    match prompt::select(interactive, "Config source", &items)? {
        Some(ConfigSourceChoice::Resume) => {
            args.resume = true;
        }
        Some(ConfigSourceChoice::ContinueExisting | ConfigSourceChoice::StartFresh) | None => {}
        Some(ConfigSourceChoice::ImportFile) => {
            let Some(path) = prompt::text(interactive, "acps-config.toml path", true)? else {
                return Ok(());
            };
            args.from_file = Some(PathBuf::from(path.trim()));
        }
        Some(ConfigSourceChoice::PasteBase64) => {
            let Some(encoded) = prompt::text(interactive, "base64 acps-config.toml", true)? else {
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

pub(super) fn run_init(mut args: InitArgs, mode: InitMode) -> Result<()> {
    let output_mode = if args.handoff_json {
        InitOutputMode::HandoffJson
    } else {
        InitOutputMode::Text
    };
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

    let home = home_dir()?;
    let config_path = config::default_config_path()?;
    let state_path = default_state_path(&home);
    let config_dir = parent_dir(&config_path)?;
    let state_dir = parent_dir(&state_path)?;

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
    if creating_config && !args.resume {
        prompt_starter_config_selections_if_needed(&mut args, &registry)?;
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

    let mut legacy_auth = None;
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
    }
    // On resume, re-collect the replayed `--agent-env-ref` names (flags only) so
    // they are re-verified against the now-open store rather than silently
    // dropped. Interactive values from the original run cannot be replayed.
    if resumed {
        agent_env_collection = collect_agent_env_refs_for_init(&args, false)?;
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
    let edge_requested = apply_edge_profile_to_config(&args, &mut config)?;
    let supabase_configured = apply_supabase_to_config_for_init(&args, &mut config)?;
    prompt_init_skills_if_needed(&mut args, &config, &registry)?;
    if agent_applied || edge_requested || supabase_configured {
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

    let skill_catalog = SkillCatalog::load_embedded()?;
    let skill_install_plan =
        resolve_skill_install_plan(&args, &home, &config, &registry, &skill_catalog)?;

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
    let handoff_context = InitHandoffContext {
        config_path: config_path.clone(),
        state_path: state_path.clone(),
        secret_store_path: secret_store.store_path().to_path_buf(),
        age_key_path: age_key_path(&home),
        agent_id: config.agent.id.clone(),
        agent_name: config.agent.name.clone(),
    };
    key_handover.failure_context = Some(handoff_context.clone());
    init_println!(output_mode, "progress: initializing auth");
    let step_result = record_step(
        &store,
        &init_run,
        1,
        step_kind::SECRETS_INIT,
        || store.auth_key_pair_present(),
        || {
            let outcome = perform_auth_init(&store, legacy_auth.as_ref(), &home)?;
            auth_status = outcome.status;
            let generated_keys = outcome.generated_keys;
            key_handover.keys = outcome.fresh_keys;
            key_handover.auth_ready = true;
            if generated_keys {
                store.append_event_with_source(
                    "info",
                    "auth.keys_generated",
                    crate::state::EVENT_SOURCE_CLI,
                    "generated session and admin API keys",
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
                let outcome = run_install_with_retry(
                    |attempt| {
                        let message =
                            format!("installing agent (attempt {attempt}/{MAX_INSTALL_ATTEMPTS})");
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
                        init_println!(output_mode, "retrying in {}s…", delay.as_secs());
                        std::thread::sleep(delay);
                    },
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
    // Step 3: agent_skills_install — install selected Agent Skills before
    // first launch/testflight. Agent harnesses auto-detect the files.
    // -----------------------------------------------------------------
    let mut skill_install_report: Option<SkillInstallReport> = None;
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
        let result = crate::runtime::init_runner::record_step_with_default_log_dir(
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
    }

    // -----------------------------------------------------------------
    // Step 10 (ordinal): deps_apply — run declared dependency install
    // actions before the agent is launched for provider/model discovery, so
    // deps the agent needs to run already exist. Opt-in: a TTY confirm, or
    // `--deps-apply --deps-apply-yes` non-interactively.
    // -----------------------------------------------------------------
    let deps_candidates = pending_candidates(&config, None);
    // Wrap in finalize_with_error so a confirmation error (e.g. `--deps-apply`
    // without `--deps-apply-yes`) marks the run terminal instead of leaving it
    // pending after the earlier steps already succeeded.
    let deps_apply_requested =
        match should_apply_deps_for_init(&args, &deps_candidates, prompts_enabled(&args)) {
            Ok(requested) => requested,
            Err(error) => return finalize_with_error(&store, &init_run, error),
        };
    if deps_apply_requested || step_needs_resume(&prior_init_steps, step_kind::DEPS_APPLY) {
        init_println!(output_mode, "progress: applying dependencies");
        let result = record_step(
            &store,
            &init_run,
            10,
            step_kind::DEPS_APPLY,
            || Ok(pending_candidates(&config, None).is_empty()),
            || {
                let report = apply_dependencies_with_progress(
                    &config,
                    None,
                    Some(&store),
                    &config.workspace.default_shell,
                    |current, total, name| {
                        init_println!(
                            output_mode,
                            "progress: applying dependency {current}/{total}: {name}"
                        );
                        Ok(())
                    },
                )?;
                let mut failures = Vec::new();
                for entry in &report.results {
                    match &entry.outcome {
                        DepApplyOutcome::Failed { exit_code, .. } => {
                            let code = exit_code
                                .map(|value| value.to_string())
                                .unwrap_or_else(|| "?".to_owned());
                            failures.push(format!("{} failed (exit={code})", entry.name));
                        }
                        DepApplyOutcome::PrivilegeRequired { uid } => {
                            failures
                                .push(format!("{} needs root privilege (uid={uid})", entry.name));
                        }
                        DepApplyOutcome::Installed | DepApplyOutcome::AlreadyPresent => {}
                    }
                }
                if !failures.is_empty() {
                    return Err(StackError::InvalidParam {
                        field: "deps",
                        reason: format!(
                            "dependency apply produced non-success outcomes: {}; inspect `acps installer history --agent deps_apply` (apply_run_id={})",
                            failures.join("; "),
                            report.apply_run_id,
                        ),
                    });
                }
                Ok(StepOutcome::with_payload(format!(
                    r#"{{"apply_run_id":"{}","applied":{}}}"#,
                    report.apply_run_id,
                    report.results.len(),
                )))
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
    let result = record_step(
        &store,
        &init_run,
        4,
        step_kind::PROVIDER_CONFIGURE,
        || {
            // Provider config is idempotent only when there's no explicit
            // change requested for any lane this step owns (provider, model).
            // We always re-run on resume so partial writes (e.g. missing secret
            // refs) get re-collected, and so a resumed `--model` still gets
            // validated and persisted rather than silently skipped because the
            // prior succeeded row passes the verifier.
            let secret_store = SecretStore::open(&provider_verify_home)?;
            Ok(args.provider.is_none()
                && args.model.is_none()
                && configured_provider_refs_satisfied(
                    &registry,
                    &provider_verify_config,
                    &secret_store,
                ))
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
            // Custom agents skip provider/model discovery, so they would
            // otherwise never spawn during init. Gate on an ACP session here so a
            // non-ACP or broken custom binary is caught now, not at first session.
            if is_custom_agent(&config, &registry) {
                verify_agent_acp_connection(&home, &config, output_mode.is_text())?;
            }
            let model_mode_changed =
                matches!(model_mode_outcome.model_action, ModelModeAction::Set);
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
                r#"{{"provider_configured":{provider_configured},"model_action":"{:?}","subagent_configured":{subagent_configured}}}"#,
                model_mode_outcome.model_action,
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

    // -----------------------------------------------------------------
    // Step 5: agent_headless_config — write the agent's local config
    // files so the harness can start without first-run prompts.
    // -----------------------------------------------------------------
    let mut provisioned_agent_configs = Vec::new();
    init_println!(output_mode, "progress: writing agent headless config");
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
        init_println!(output_mode, "progress: preparing Cloudflare edge artifacts");
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
    if let Some(report) = skill_install_report {
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
    if output_mode.is_handoff_json() {
        key_handover.record(&store, &init_run.id)?;
        crate::runtime::init_runner::finalize_run(&store, &init_run.id, INIT_RUN_SUCCEEDED)?;
        key_handover.print_handoff_json("initialized", &handoff_context)?;
    } else {
        key_handover.print_and_record(&store, &init_run.id)?;
        crate::runtime::init_runner::finalize_run(&store, &init_run.id, INIT_RUN_SUCCEEDED)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

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
    fn starter_config_writes_interactive_mcp_rows() {
        let mut args = parse_init_args(&[]);
        args.prompt_mcp_stdio.push(InitMcpStdioServer {
            name: "local-tool".to_owned(),
            command: "local-tool-mcp".to_owned(),
            args: vec!["serve".to_owned(), "--verbose".to_owned()],
            env: vec!["LOCAL_TOOL_API_KEY".to_owned()],
        });
        args.prompt_mcp_http.push(InitMcpHttpServer {
            name: "remote".to_owned(),
            url: "https://mcp.example.com".to_owned(),
            headers: vec![InitMcpHttpHeader {
                name: "Authorization".to_owned(),
                value_ref: "REMOTE_MCP_TOKEN".to_owned(),
            }],
        });

        let toml = starter_config::starter_config(&args).expect("starter config");
        let config = config::load_config_from_str(&toml).expect("config parses");
        assert_eq!(config.mcp.servers.len(), 2);
        match &config.mcp.servers[0] {
            config::McpServerConfig::Stdio(stdio) => {
                assert_eq!(stdio.name, "local-tool");
                assert_eq!(stdio.command, "local-tool-mcp");
                assert_eq!(stdio.args, ["serve", "--verbose"]);
                assert_eq!(stdio.env, ["LOCAL_TOOL_API_KEY"]);
            }
            other => panic!("expected stdio MCP, got {other:?}"),
        }
        match &config.mcp.servers[1] {
            config::McpServerConfig::Http(http) => {
                assert_eq!(http.name, "remote");
                assert_eq!(http.url, "https://mcp.example.com");
                assert_eq!(http.headers.len(), 1);
                assert_eq!(http.headers[0].name, "Authorization");
                assert_eq!(http.headers[0].value_ref, "REMOTE_MCP_TOKEN");
            }
            other => panic!("expected HTTP MCP, got {other:?}"),
        }
    }

    #[test]
    fn starter_config_writes_interactive_s3_data_source() {
        let mut args = parse_init_args(&[]);
        args.prompt_data_sources.push(DataSourceConfig {
            source_type: "s3".to_owned(),
            name: None,
            path: None,
            url: None,
            expected_sha256: None,
            max_download_bytes: None,
            max_extracted_bytes: None,
            bucket: Some("acps-fixtures".to_owned()),
            prefix: Some("datasets".to_owned()),
            region: Some("us-east-1".to_owned()),
            access_key_ref: Some("AWS_ACCESS_KEY_ID".to_owned()),
            secret_key_ref: Some("AWS_SECRET_ACCESS_KEY".to_owned()),
        });

        let toml = starter_config::starter_config(&args).expect("starter config");
        let config = config::load_config_from_str(&toml).expect("config parses");
        assert_eq!(config.workspace.data_sources.len(), 1);
        let source = &config.workspace.data_sources[0];
        assert_eq!(source.source_type, "s3");
        assert_eq!(source.bucket.as_deref(), Some("acps-fixtures"));
        assert_eq!(source.prefix.as_deref(), Some("datasets"));
        assert_eq!(source.region.as_deref(), Some("us-east-1"));
        assert_eq!(source.access_key_ref.as_deref(), Some("AWS_ACCESS_KEY_ID"));
        assert_eq!(
            source.secret_key_ref.as_deref(),
            Some("AWS_SECRET_ACCESS_KEY")
        );
    }
}
