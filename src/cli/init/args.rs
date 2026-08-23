use super::*;

#[derive(Debug, Args)]
pub struct InitCommand {
    #[command(subcommand)]
    pub(super) command: Option<InitSubcommand>,
    #[command(flatten)]
    pub(super) args: InitArgs,
}

#[derive(Debug, Subcommand)]
pub(super) enum InitSubcommand {
    /// Run the hosted bootstrap init HTTP/WebSocket server.
    Serve(serve::InitServeArgs),
}

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

/// Mirrors `HttpHeaderRef`: exactly one of `value_ref` (whole-value secret
/// ref) or `value` (`${NAME}` template) is set; enforced where the record is
/// built (wire boundary or flag splitter).
#[derive(Debug, Clone)]
pub(super) struct InitMcpHttpHeader {
    pub(super) name: String,
    pub(super) value_ref: Option<String>,
    pub(super) value: Option<String>,
}

#[derive(Clone)]
pub(super) struct InitNativeConfigUpload {
    pub(super) filename: String,
    pub(super) content: Zeroizing<String>,
}

impl std::fmt::Debug for InitNativeConfigUpload {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InitNativeConfigUpload")
            .field("filename", &self.filename)
            .field("size_bytes", &self.content.len())
            .finish()
    }
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
        conflicts_with_all = ["agent", "provider", "model", "mode", "effort", "custom_provider"]
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
    /// Launch the selected registry agent through a designated ACP adapter
    /// binary instead of its curated entry command.
    #[arg(
        long = "adapter-override-command",
        value_name = "CMD",
        conflicts_with = "custom_agent_id"
    )]
    pub(super) adapter_override_command: Option<String>,
    /// Launch argument for the designated adapter. Repeatable; values may
    /// start with `-` (adapter flags are the common case).
    #[arg(
        long = "adapter-override-arg",
        value_name = "ARG",
        allow_hyphen_values = true,
        requires = "adapter_override_command"
    )]
    pub(super) adapter_override_arg: Vec<String>,
    /// GitHub repo of the designated adapter (`owner/repo` or URL).
    #[arg(
        long = "adapter-override-github",
        value_name = "REPO",
        requires = "adapter_override_command"
    )]
    pub(super) adapter_override_github: Option<String>,
    /// Install the designated adapter from this npm package (managed updates
    /// track the npm registry).
    #[arg(
        long = "adapter-override-install-npm",
        value_name = "PACKAGE",
        requires = "adapter_override_command",
        conflicts_with = "adapter_override_install_shell"
    )]
    pub(super) adapter_override_install_npm: Option<String>,
    /// Install the designated adapter with this shell snippet (managed
    /// updates skip shell installs).
    #[arg(
        long = "adapter-override-install-shell",
        value_name = "SHELL",
        requires = "adapter_override_command"
    )]
    pub(super) adapter_override_install_shell: Option<String>,
    /// Path that must resolve to an executable after the adapter install
    /// (defaults to the adapter command).
    #[arg(
        long = "adapter-override-install-creates",
        value_name = "PATH",
        requires = "adapter_override_command"
    )]
    pub(super) adapter_override_install_creates: Option<String>,
    /// Remove a stored `[agent.adapter_override]` block and return the agent
    /// to its curated registry launch command.
    #[arg(
        long = "adapter-override-clear",
        conflicts_with_all = [
            "adapter_override_command",
            "adapter_override_arg",
            "adapter_override_github",
            "adapter_override_install_npm",
            "adapter_override_install_shell",
            "adapter_override_install_creates",
            "custom_agent_id",
        ]
    )]
    pub(super) adapter_override_clear: bool,
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
    /// Run the confirmed dependency install in a detached background process
    /// and let init continue without waiting for it. Poll
    /// `GET /v1/deps/apply/runs/{apply_run_id}` for the result.
    #[arg(long = "deps-apply-async", requires = "deps_apply")]
    pub(super) deps_apply_async: bool,
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
    /// Managed agent auto-update: on (daemon updates the harness/adapter when
    /// stopped) or off (updates stay manual via `acps agent update`).
    #[arg(long = "agent-update", value_name = "on|off")]
    pub(super) agent_update: Option<String>,
    /// Managed agent auto-update frequency (hour/day/week units, e.g. 12h, 1d,
    /// 3w; minimum 1 hour). Ignored when auto-update is off.
    #[arg(
        long = "agent-update-frequency",
        value_name = "FREQ",
        requires = "agent_update"
    )]
    pub(super) agent_update_frequency: Option<String>,
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
    /// Initial session mode id, validated against the agent's ACP-advertised
    /// `mode` values discovered via the same provisional session as `--model`.
    #[arg(long)]
    pub(super) mode: Option<String>,
    /// Initial reasoning-effort value, validated against the agent's
    /// ACP-advertised effort values discovered via the same provisional
    /// session as `--model`.
    #[arg(long)]
    pub(super) effort: Option<String>,
    /// Display name for a custom model.
    #[arg(long = "model-name", requires = "custom_provider")]
    pub(super) model_name: Option<String>,
    /// Context window in tokens for a custom model.
    #[arg(long, requires = "custom_provider")]
    pub(super) context: Option<String>,
    /// Maximum output tokens for a custom model.
    #[arg(long = "output-max-tokens", requires = "custom_provider")]
    pub(super) output_max_tokens: Option<String>,
    /// Reviewed skill source alias, or github:<owner> for <owner>/skills.
    #[arg(
        long = "skills-source",
        requires = "skills",
        conflicts_with = "no_skills"
    )]
    pub(super) skills_source: Option<String>,
    /// Comma-separated Agent Skill selectors to install during init.
    #[arg(
        long = "skills",
        value_name = "NAME",
        value_delimiter = ',',
        requires = "skills_source",
        conflicts_with = "no_skills"
    )]
    pub(super) skills: Vec<String>,
    #[arg(skip)]
    pub(super) essential_skills: bool,
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
    /// Agent sandbox mode to write into a newly-created starter config:
    /// off (default), unshare, bwrap, or custom. Sets
    /// `[workspace.sandbox].mode`; only applied when the starter config is
    /// being created. `custom` additionally requires a wrapper, which must be
    /// supplied via an imported config.
    #[arg(long = "sandbox", value_name = "off|unshare|bwrap|custom")]
    pub(super) sandbox: Option<String>,
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
    /// Add an env entry to a custom stdio MCP server as `server=SECRET_REF`
    /// or `server=VAR=template` (template values interpolate `${SECRET_REF}`).
    #[arg(long = "mcp-stdio-env", value_name = "SERVER=ENTRY")]
    pub(super) mcp_stdio_env: Vec<String>,
    /// Add a custom HTTP MCP server as `name=https://...`.
    #[arg(long = "mcp-http", value_name = "NAME=URL")]
    pub(super) mcp_http: Vec<String>,
    /// Add a header to a custom HTTP MCP server as `server=Header:SECRET_REF`
    /// (whole-value ref) or `server=Header:=template` (template values
    /// interpolate `${SECRET_REF}`).
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
    pub(super) standard_agent_work_deps: bool,
    #[arg(skip)]
    pub(super) browser_use_profile: bool,
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
    #[arg(skip)]
    pub(super) native_config_upload: Option<InitNativeConfigUpload>,
    #[arg(skip)]
    pub(super) native_config_revision: Option<String>,
    /// The hosted caller declared it will push a custom provider's credential
    /// out-of-band (managed-state) after init. Only then is a missing custom
    /// provider ref soft-passed; a terminal run leaves this false and keeps the
    /// hard failure. Not a CLI flag: the declaration rides the start request.
    #[arg(skip)]
    pub(super) defer_provider_credentials: bool,
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
    /// Regenerate the session and admin API keys even when verifier rows
    /// already exist, and include the new plaintexts in the handover. A
    /// running daemon must be restarted before the new keys are accepted.
    #[arg(long)]
    pub(super) rotate_keys: bool,
}

/// Every flag absent, as if `acps init` were invoked with no arguments at all.
/// Hand-written rather than derived because three fields are not zero-valued:
/// `no_skills` starts set (skills are opt-in for a caller that declares
/// nothing), and the two edge enums carry the same defaults clap applies.
impl Default for InitArgs {
    fn default() -> Self {
        Self {
            agent: None,
            custom_agent_id: None,
            custom_agent_name: None,
            custom_agent_command: None,
            custom_agent_arg: Vec::new(),
            custom_agent_install: None,
            custom_agent_creates: None,
            adapter_override_command: None,
            adapter_override_arg: Vec::new(),
            adapter_override_github: None,
            adapter_override_install_npm: None,
            adapter_override_install_shell: None,
            adapter_override_install_creates: None,
            adapter_override_clear: false,
            agent_env_ref: Vec::new(),
            dep: Vec::new(),
            dep_system: Vec::new(),
            deps_apply: false,
            deps_apply_yes: false,
            deps_apply_async: false,
            stack_update: None,
            stack_update_frequency: None,
            agent_update: None,
            agent_update_frequency: None,
            non_interactive: false,
            handoff_json: false,
            from_file: None,
            from_toml: None,
            from_base64: None,
            provider: None,
            api_key_ref: None,
            custom_provider: false,
            provider_name: None,
            base_url: None,
            provider_api: None,
            model: None,
            mode: None,
            effort: None,
            model_name: None,
            context: None,
            output_max_tokens: None,
            skills_source: None,
            skills: Vec::new(),
            essential_skills: false,
            no_skills: true,
            edge: None,
            exposure: None,
            hostname: None,
            cloudflare_mode: CloudflareModeArg::Generated,
            cloudflare_api_token_ref: None,
            cloudflare_account_id_ref: None,
            cloudflared_deployment: CloudflaredDeploymentArg::Host,
            workspace_root: None,
            workspace_uploads: None,
            runtime_user: None,
            sandbox: None,
            code_from: Vec::new(),
            data_from: Vec::new(),
            mcp_preset: Vec::new(),
            mcp_stdio: Vec::new(),
            mcp_stdio_env: Vec::new(),
            mcp_http: Vec::new(),
            mcp_http_header: Vec::new(),
            supabase_url: None,
            supabase_schema: None,
            supabase_api_key_ref: None,
            no_supabase: false,
            #[cfg(feature = "dev-tools")]
            skip_workspace_init: false,
            testflight: false,
            native_config_upload: None,
            native_config_revision: None,
            defer_provider_credentials: false,
            skip_testflight: false,
            standard_agent_work_deps: false,
            browser_use_profile: false,
            prompt_agent_env_refs: false,
            prompt_skills: false,
            prompt_data_sources: Vec::new(),
            prompt_mcp_stdio: Vec::new(),
            prompt_mcp_http: Vec::new(),
            resume: false,
            fresh: false,
            run_id: None,
            rotate_keys: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::cli) enum InitMode {
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
