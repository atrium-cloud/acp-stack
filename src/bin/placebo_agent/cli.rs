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
    /// Advertise `session/delete` but fail the call. Separates "the agent
    /// cannot delete sessions" from "the delete was attempted and failed",
    /// which are distinct cleanup verdicts.
    #[arg(long)]
    pub(crate) fail_delete_session: bool,
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
    /// Settle the turn as cancelled once a `session/cancel` issued after the turn
    /// started arrives, after this extra delay. The wait runs off the dispatch loop,
    /// so the notification is still processed while the turn is open.
    #[arg(long)]
    pub(crate) prompt_settle_cancel_after_ms: Option<u64>,
    /// Hold the turn open forever while still processing incoming messages:
    /// `session/cancel` is accepted and acknowledged, and the prompt response
    /// never comes.
    #[arg(long)]
    pub(crate) prompt_never_settle: bool,
    #[arg(long)]
    pub(crate) request_permission_then_cancel: bool,
    /// Hold the turn open on a `session/request_permission` the agent never
    /// cancels itself: the turn settles only once the client answers, mirroring
    /// an adapter parked on an operator decision. A `cancelled` outcome settles
    /// the turn as cancelled; a selected option ends it as a normal turn.
    #[arg(long)]
    pub(crate) prompt_await_permission: bool,
    /// How many permission requests the turn raises in sequence, each one only
    /// after the previous is answered. Rounds past the first land after a
    /// `session/cancel` has already gone out, exercising a client that keeps
    /// answering for as long as the turn is open.
    #[arg(long, default_value_t = 1)]
    pub(crate) prompt_await_permission_rounds: u32,
    #[arg(long)]
    pub(crate) session_list_paginated: bool,
    #[arg(long)]
    pub(crate) session_list_repeated_cursor: bool,
    #[arg(long)]
    pub(crate) model_config_option: Option<String>,
    #[arg(long, default_value = "model")]
    pub(crate) model_config_option_id: String,
    /// Extra select config option: `<id>[@<category>]=<current>:<v1>,<v2>,...`.
    /// Repeatable. An empty category segment advertises a category-less option.
    #[arg(long)]
    pub(crate) config_option_select: Vec<String>,
    /// Extra boolean config option: `<id>[@<category>]=<true|false>`. Repeatable.
    #[arg(long)]
    pub(crate) config_option_boolean: Vec<String>,
    /// Advertise native session modes on `session/new`: repeatable mode id. When
    /// any is given, the response carries a `modes` (SessionModeState) instead of
    /// a mode config option, exercising the native `session/set_mode` lane.
    #[arg(long)]
    pub(crate) session_mode: Vec<String>,
    /// Current native mode id (defaults to the first `--session-mode`).
    #[arg(long)]
    pub(crate) session_mode_current: Option<String>,
    /// Fail `session/prompt` unless this mode id was applied via `session/set_mode`
    /// first, proving the native mode lane fired (mirrors `--expect-model-config`).
    #[arg(long)]
    pub(crate) expect_mode: Option<String>,
    /// After each `session/set_config_option`, also emit a
    /// `config_option_update` session notification carrying the full list.
    #[arg(long)]
    pub(crate) emit_config_option_update: bool,
    /// Strict-agent mode: `session/set_config_option` responds with an empty
    /// list, so the notification (see `--emit-config-option-update`) is the
    /// only carrier of the refreshed state.
    #[arg(long)]
    pub(crate) set_config_option_responds_empty: bool,
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
    /// Fail `session/prompt` unless `<id>=<value>` was applied via
    /// `session/set_config_option` on this session first, proving the generic
    /// config lane fired (mirrors `--expect-model-config`). Repeatable.
    #[arg(long)]
    pub(crate) expect_config_option: Vec<String>,
    #[arg(long)]
    pub(crate) write_pid: Option<PathBuf>,
}
