use super::*;

#[derive(Debug, Parser)]
#[command(
    name = "acps",
    version,
    about = env!("CARGO_PKG_DESCRIPTION"),
    color = clap::ColorChoice::Never,
after_help = "Examples:
  acps init --agent opencode --provider openrouter --api-key-ref OPENROUTER_API_KEY
  acps init --from-base64 <base64-acps-config-toml>
  acps status --format json
  acps array status
  acps sessions list --range week --format json
  acps logging supabase status --format json
  acps logs query --since 1h --kind prompt. --format json
  acps deps check --format json
  acps security history --format json
  acps config export --output acps-config.toml
  acps config import acps-config.toml --dry-run
  acps completion zsh > _acps",
)]
pub struct Cli {
    /// Output format for commands that support structured output.
    #[arg(long = "format", global = true, value_enum)]
    pub(crate) format: Option<OutputFormat>,
    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Generate shell completion scripts.
    #[command(after_help = "Examples:
  acps completion bash > acps.bash
  acps completion zsh > _acps
  acps completion fish > acps.fish")]
    Completion {
        #[arg(value_enum)]
        shell: Shell,
    },
    /// Initialize local config, secrets, workspace, and agent files.
    Init(Box<InitCommand>),
    /// Run development-only workflows.
    #[cfg(feature = "dev-tools")]
    #[command(after_help = "Examples:
  acps dev init --skip-workspace-init --agent opencode --skip-testflight")]
    Dev {
        #[command(subcommand)]
        command: DevCommand,
    },
    /// Print daemon health and runtime status.
    Status,
    /// Check, install, or configure acp-stack self-updates.
    #[cfg(feature = "stack-self-update")]
    #[command(after_help = "Examples:
  acps update check
  acps update install --latest
  acps update set --policy security-critical --frequency 1d")]
    Update {
        #[command(subcommand)]
        command: UpdateCommand,
    },
    /// Remove local acp-stack config, state, and secrets after confirmation.
    Reset(ResetArgs),
    /// Run the HTTP daemon in the foreground. Blocks until SIGTERM or SIGINT.
    Serve(ServeArgs),
    /// Rotate or inspect configured API key references.
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    /// Manage encrypted local secret values.
    Secrets {
        #[command(subcommand)]
        command: SecretsCommand,
    },
    /// Validate, export, or import runtime config.
    #[command(after_help = "Examples:
  acps config validate
  acps config export --output acps-config.toml
  acps config export --format json
  acps config import acps-config.toml --dry-run")]
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Configure external logging sinks.
    #[command(after_help = "Examples:
	  acps logging supabase status
	  acps logging supabase setup --url https://example.supabase.co
	  acps logging supabase check
	  acps logging supabase enable --url https://example.supabase.co
	  acps logging supabase set-secret
	  acps logging supabase set-db-url")]
    Logging {
        #[command(subcommand)]
        command: LoggingCommand,
    },
    /// Query durable runtime logs.
    #[command(after_help = "Examples:
  acps logs query --since 1h --kind prompt.
  acps logs query --follow --format json
  acps logs tail")]
    Logs {
        #[command(subcommand)]
        command: LogsCommand,
    },
    /// Install, control, test, or configure the agent.
    #[command(after_help = "Examples:
  acps agent status --format json
  acps agent check
  acps agent start
  acps agent restart")]
    Agent {
        #[command(subcommand)]
        command: AgentCommand,
    },
    /// Restart the supervised agent process.
    #[command(after_help = "Examples:
  acps restart
  acps restart auto")]
    Restart(AgentRestartArgs),
    /// Inspect or change workspace sources and sandbox settings.
    #[command(after_help = "Examples:
  acps workspace status
  acps workspace sync
  acps workspace code-source list
  acps workspace sandbox set --mode bwrap")]
    Workspace {
        #[command(subcommand)]
        command: WorkspaceCommand,
    },
    /// Inspect declared extension instances.
    #[command(after_help = "Examples:
  acps extensions status
  acps extensions status --format json")]
    Extensions {
        #[command(subcommand)]
        command: ExtensionsCommand,
    },
    /// Manage multi-agent Array targets.
    #[command(after_help = "Examples:
  acps array status
  acps array on
  acps array add codex
  acps array start --target codex")]
    Array {
        #[command(subcommand)]
        command: ArrayCommand,
    },
    /// Manage Agent Skills for the active agent: list, catalog, add, remove, source.
    #[command(after_help = "Examples:
  acps skills list
  acps skills catalog
  acps skills add anthropic docx pptx xlsx pdf
  acps skills remove docx")]
    Skills {
        #[command(subcommand)]
        command: SkillCommand,
    },
    /// Configure OpenCode small-model behavior.
    Subagent {
        #[command(subcommand)]
        command: SubagentCommand,
    },
    /// Inspect persisted installer step history.
    Installer {
        #[command(subcommand)]
        command: InstallerCommand,
    },
    /// List, create, prompt, or close sessions.
    #[command(after_help = "Examples:
  acps sessions list --range week
  acps sessions new --format json
  acps sessions prompt <session-id> --text \"hello\"")]
    Sessions {
        #[command(subcommand)]
        command: SessionsCommand,
    },
    /// Check and apply declared runtime dependencies.
    #[command(after_help = "Examples:
  acps deps check
  acps deps check --format json
  acps deps apply --yes")]
    Deps {
        #[command(subcommand)]
        command: DepsCommand,
    },
    /// Run runtime security self-checks.
    #[command(after_help = "Examples:
  acps security check
  acps security history --format json
  acps security show <run-id> --format json")]
    Security {
        #[command(subcommand)]
        command: SecurityCommand,
    },
    /// Inspect derived runtime metrics.
    Metrics {
        #[command(subcommand)]
        command: MetricsCommand,
    },
    /// Inspect and manage live WebSocket clients.
    Ws {
        #[command(subcommand)]
        command: WsCommand,
    },
    /// Internal: in-namespace masking step for the sandbox. Not for direct use.
    #[command(name = "__sandbox-exec", hide = true)]
    SandboxExec {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
        args: Vec<String>,
    },
    /// Internal: network-namespace lifecycle supervisor for isolated sandbox
    /// spawns. Not for direct use.
    #[command(name = "__sandbox-supervise", hide = true)]
    SandboxSupervise {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
        args: Vec<String>,
    },
    /// Internal: provider process-group monitor for network-isolated sandbox
    /// spawns. Not for direct use.
    #[command(name = "__sandbox-provider-supervise", hide = true)]
    SandboxProviderSupervise {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
        args: Vec<String>,
    },
    /// Internal: detached dependency-apply worker spawned by
    /// `acps init --deps-apply-async`. Not for direct use.
    #[command(name = "__deps-apply-run", hide = true)]
    DepsApplyRun {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
        args: Vec<String>,
    },
}

#[cfg(feature = "dev-tools")]
#[derive(Debug, Subcommand)]
pub(crate) enum DevCommand {
    /// Initialize with development-only flags enabled.
    #[command(mut_arg("skip_workspace_init", |arg| arg.hide(false)))]
    Init(Box<InitArgs>),
    /// Run the daemon with development-only flags enabled.
    #[command(mut_arg("allow_root", |arg| arg.hide(false)))]
    Serve(ServeArgs),
}
