use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "acpctl",
    version,
    about = "Local agent control CLI for the acp-stack runtime."
)]
pub(crate) struct Cli {
    /// Override the Unix-domain socket path. Defaults to
    /// `~/.local/share/acp-stack/acpctl.sock`.
    #[arg(long, global = true)]
    pub(crate) socket: Option<PathBuf>,
    /// Emit the raw JSON response envelope rather than human-readable text.
    #[arg(long, global = true)]
    pub(crate) json: bool,
    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Subcommand, Debug)]
#[allow(clippy::enum_variant_names)]
pub(crate) enum Command {
    /// Print runtime status (schema version, latest event).
    Status,
    /// Run the daemon's security self-check.
    Security {
        #[command(subcommand)]
        action: SecurityCommand,
    },
    /// Inspect or refresh dependency status.
    Deps {
        #[command(subcommand)]
        action: DepsCommand,
    },
    /// Query recent log events.
    Logs {
        #[command(subcommand)]
        action: LogsCommand,
    },
    /// Workspace file operations.
    Workspace {
        #[command(subcommand)]
        action: WorkspaceCommand,
    },
    /// Run a mediated shell command through the command gateway.
    Command {
        #[command(subcommand)]
        action: CommandCommand,
    },
    /// Config-related actions.
    Config {
        #[command(subcommand)]
        action: ConfigCommand,
    },
    /// Permission queue introspection.
    Permissions {
        #[command(subcommand)]
        action: PermissionsCommand,
    },
    /// Read-only WebSocket connection introspection.
    Ws {
        #[command(subcommand)]
        action: WsCommand,
    },
    /// Local MCP introspection server.
    Mcp {
        #[command(subcommand)]
        action: McpCommand,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum SecurityCommand {
    /// Print findings from the runtime security self-check.
    Check,
}

#[derive(Subcommand, Debug)]
pub(crate) enum DepsCommand {
    /// Run the dependency check and print the latest report.
    Check,
}

#[derive(Subcommand, Debug)]
pub(crate) enum LogsCommand {
    /// Query events between optional time bounds.
    Query(LogsQueryArgs),
}

#[derive(Args, Debug)]
pub(crate) struct LogsQueryArgs {
    /// Restrict to events on or after this time. Accepts duration suffixes
    /// (`30m`, `1h`, `2d`) or RFC3339 timestamps.
    #[arg(long)]
    pub(crate) since: Option<String>,
    /// Restrict to events strictly before this time.
    #[arg(long)]
    pub(crate) until: Option<String>,
    /// Filter by event kind. A trailing `.` matches as a prefix.
    #[arg(long)]
    pub(crate) kind: Option<String>,
    /// Filter by log level.
    #[arg(long)]
    pub(crate) level: Option<String>,
    /// Filter by session ID.
    #[arg(long)]
    pub(crate) session: Option<String>,
    /// Maximum number of rows to return.
    #[arg(long, default_value_t = 200)]
    pub(crate) limit: u32,
    /// Cursor for pagination; pass the last seen event id.
    #[arg(long)]
    pub(crate) after: Option<String>,
}

#[derive(Subcommand, Debug)]
pub(crate) enum WorkspaceCommand {
    /// List a directory inside the workspace root.
    List { path: String },
    /// Print the contents of a workspace file to stdout.
    Read { path: String },
    /// Write stdin to the workspace file at the given path (atomic).
    Write { path: String },
}

#[derive(Subcommand, Debug)]
pub(crate) enum CommandCommand {
    /// List recent command gateway records.
    List {
        /// Maximum number of command rows to return.
        #[arg(long, default_value_t = 200)]
        limit: u32,
    },
    /// Show one command gateway record.
    Get { id: String },
    /// Print captured command output chunks.
    Output {
        id: String,
        /// Maximum number of output chunks to return.
        #[arg(long, default_value_t = 200)]
        limit: u32,
        /// Cursor returned by a previous output call.
        #[arg(long)]
        after: Option<String>,
        /// Output order: asc or desc.
        #[arg(long, default_value = "asc")]
        order: String,
    },
    /// Request cancellation for a running command.
    Cancel { id: String },
    /// Submit a shell command to the command gateway.
    Run {
        command: String,
        /// Optional working directory; must remain inside the workspace root.
        #[arg(long)]
        cwd: Option<String>,
        /// Optional timeout, e.g. `30s`, `5m`.
        #[arg(long)]
        timeout: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum ConfigCommand {
    /// Print the canonical TOML config with secret references only.
    Export,
}

#[derive(Subcommand, Debug)]
pub(crate) enum PermissionsCommand {
    /// List pending permission requests.
    Pending {
        #[arg(long, default_value_t = 200)]
        limit: u32,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum WsCommand {
    /// List live WebSocket connections.
    Connections,
    /// List unique subscribed session IDs.
    Sessions,
}

#[derive(Subcommand, Debug)]
pub(crate) enum McpCommand {
    /// Serve the local introspection interface as an MCP server.
    Serve(McpServeArgs),
}

#[derive(Args, Debug)]
pub(crate) struct McpServeArgs {
    /// Transport for the MCP server.
    #[arg(long, value_enum, default_value_t = McpTransport::Stdio)]
    pub(crate) transport: McpTransport,
    /// Bind path for the `http-uds` transport. Defaults to
    /// `~/.local/share/acp-stack/acpctl-mcp.sock`. Ignored for `stdio`.
    #[arg(long)]
    pub(crate) bind: Option<PathBuf>,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
pub(crate) enum McpTransport {
    /// Speak MCP JSON-RPC over stdin/stdout (default; meant to be spawned by an
    /// agent as a child process).
    Stdio,
    /// Serve MCP streamable HTTP over a Unix-domain socket so agents that
    /// dial UDS URLs can connect without a child process.
    HttpUds,
}
