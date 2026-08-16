use super::*;

#[derive(Debug, Parser)]
#[command(
    name = "placebo-agent",
    version,
    about = "Deterministic ACP test fixture agent.",
    color = clap::ColorChoice::Never
)]
pub(crate) struct Cli {
    #[arg(long, global = true)]
    pub(crate) print_logs: bool,
    #[arg(long, global = true, value_enum)]
    pub(crate) log_level: Option<LogLevel>,
    #[arg(long, global = true)]
    pub(crate) pure: bool,
    #[arg(long, global = true, default_value_t = 0)]
    pub(crate) port: u16,
    #[arg(long, global = true, default_value = "127.0.0.1")]
    pub(crate) hostname: String,
    #[arg(long, global = true, default_value_t = false)]
    pub(crate) mdns: bool,
    #[arg(long, global = true, default_value = "opencode.local")]
    pub(crate) mdns_domain: String,
    #[arg(long, global = true)]
    pub(crate) cors: Vec<String>,
    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Start ACP (Agent Client Protocol) server.
    Acp(AcpArgs),
}

#[derive(Debug, Args, Clone)]
pub(crate) struct AcpArgs {
    /// Working directory.
    #[arg(long, default_value_os_t = std::env::current_dir().unwrap_or_else(|_| PathBuf::from(DEFAULT_CWD)))]
    pub(crate) cwd: PathBuf,
    #[arg(long, default_value = DEFAULT_CWD)]
    pub(crate) listed_cwd: PathBuf,
    #[arg(long)]
    pub(crate) assert_env_absent: Vec<String>,
    #[arg(long)]
    pub(crate) assert_env_present: Vec<String>,
    #[arg(long, num_args = 2)]
    pub(crate) assert_env_not_equals: Vec<String>,
    #[arg(long)]
    pub(crate) no_cap_load_session: bool,
    #[arg(long)]
    pub(crate) no_cap_list_session: bool,
    #[arg(long)]
    pub(crate) no_cap_resume_session: bool,
    #[arg(long)]
    pub(crate) no_cap_close_session: bool,
    #[arg(long)]
    pub(crate) no_cap_delete_session: bool,
    #[arg(long)]
    pub(crate) no_cap_fork_session: bool,
    #[arg(long)]
    pub(crate) no_cap_fork_message_id: bool,
    /// Advertise `mcpCapabilities.http = true`. Without it the placebo
    /// advertises no MCP capability at all.
    #[arg(long)]
    pub(crate) cap_mcp_http: bool,
    #[arg(long)]
    pub(crate) expect_fork_message_id: Option<String>,
    #[arg(long)]
    pub(crate) prompt_silent: bool,
    #[arg(long)]
    pub(crate) initialize_error: bool,
    #[arg(long)]
    pub(crate) initialize_protocol_v0: bool,
    #[arg(long)]
    pub(crate) require_client_info: bool,
    #[arg(long)]
    pub(crate) session_new_error: bool,
    #[arg(long)]
    pub(crate) session_new_stall: bool,
    #[arg(long)]
    pub(crate) prompt_error: bool,
    #[arg(long)]
    pub(crate) prompt_inference_error: Option<String>,
    #[arg(long)]
    pub(crate) prompt_inference_error_after_update: Option<String>,
    #[arg(long)]
    pub(crate) prompt_response_delay_ms: Option<u64>,
    #[arg(long)]
    pub(crate) prompt_stall_after_update: bool,
    #[arg(long)]
    pub(crate) request_permission_then_cancel: bool,
    #[arg(long)]
    pub(crate) session_list_paginated: bool,
    #[arg(long)]
    pub(crate) session_list_repeated_cursor: bool,
    #[arg(long)]
    pub(crate) model_config_option: Option<String>,
    #[arg(long, default_value = "model")]
    pub(crate) model_config_option_id: String,
    /// Strict-agent mode: return session config options only when the client
    /// advertised `session.configOptions` support at initialize.
    #[arg(long)]
    pub(crate) require_client_config_options: bool,
    /// Strict-agent mode: drive `terminal/*` only when the client advertised
    /// `terminal: true` at initialize.
    #[arg(long)]
    pub(crate) require_terminal: bool,
    /// During prompt handling, run this program through a client terminal and
    /// report the round-trip as a `terminal-report:` message chunk.
    #[arg(long)]
    pub(crate) terminal_command: Option<String>,
    #[arg(long)]
    pub(crate) terminal_arg: Vec<String>,
    #[arg(long)]
    pub(crate) terminal_byte_limit: Option<u64>,
    #[arg(long)]
    pub(crate) terminal_cwd: Option<PathBuf>,
    /// Kill the terminal right after creation instead of waiting for natural
    /// exit.
    #[arg(long)]
    pub(crate) terminal_kill: bool,
    /// Cancel the first wait request, verify the terminal remains usable,
    /// then kill and complete the normal wait/release lifecycle.
    #[arg(long)]
    pub(crate) terminal_cancel_wait: bool,
    /// Create the terminal and leave it running: no wait, kill, or release.
    /// Exercises the client's shutdown kill-and-release path.
    #[arg(long)]
    pub(crate) terminal_orphan: bool,
    /// Call `terminal/release` with an unknown id and report the error code.
    #[arg(long)]
    pub(crate) terminal_release_unknown: bool,
    /// Strict-agent mode: drive `fs/*` only when the client advertised both
    /// `fs.readTextFile` and `fs.writeTextFile` at initialize.
    #[arg(long)]
    pub(crate) require_fs: bool,
    /// During prompt handling, write this file via `fs/write_text_file` and
    /// report the round-trip.
    #[arg(long)]
    pub(crate) fs_write_path: Option<PathBuf>,
    #[arg(long, default_value = "fs-probe-content")]
    pub(crate) fs_write_content: String,
    /// During prompt handling, read this file via `fs/read_text_file`.
    #[arg(long)]
    pub(crate) fs_read_path: Option<PathBuf>,
    #[arg(long)]
    pub(crate) fs_read_line: Option<u32>,
    #[arg(long)]
    pub(crate) fs_read_limit: Option<u32>,
    #[arg(long)]
    pub(crate) expect_model_config: Option<String>,
    #[arg(long)]
    pub(crate) write_pid: Option<PathBuf>,
}
