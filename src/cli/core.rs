use crate::config::Config;
use crate::error::{Result, StackError};
use crate::fs_util::{home_dir, set_owner_only_file};
use crate::secrets::SecretStore;
use crate::state::{StateStore, default_state_path};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::{Shell, generate};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use super::agent::AgentCommand;
use super::auth::AuthCommand;
use super::config::ConfigCommand;
use super::deps::DepsCommand;
use super::init::InitArgs;
use super::installer::InstallerCommand;
use super::logs::LogsCommand;
use super::metrics::MetricsCommand;
use super::reset::ResetArgs;
use super::secrets::SecretsCommand;
use super::security::SecurityCommand;
use super::serve::ServeArgs;
use super::sessions::SessionsCommand;
use super::subagent::SubagentCommand;
use super::ws::WsCommand;

#[derive(Debug, Parser)]
#[command(
    name = "acps",
    version,
    about = env!("CARGO_PKG_DESCRIPTION"),
    color = clap::ColorChoice::Never,
    after_help = "Examples:
  acps init --agent opencode --provider openrouter --api-key-ref OPENROUTER_API_KEY
  acps status --format json
  acps sessions list --range week --format json
  acps logs query --since 1h --kind prompt. --format json
  acps deps check --format json
  acps security history --format json
  acps config export --output acp-stack.toml
  acps config import acp-stack.toml --dry-run
  acps completion zsh > _acps",
)]
pub struct Cli {
    /// Output format for commands that support structured output.
    #[arg(long = "format", global = true, value_enum)]
    format: Option<OutputFormat>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
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
    Init(Box<InitArgs>),
    /// Print daemon health and runtime status.
    Status,
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
  acps config export --output acp-stack.toml
  acps config export --format json
  acps config import acp-stack.toml --dry-run")]
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
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
}

pub fn run() -> Result<()> {
    // Test fixture, debug builds only: an internal argv sentinel makes this
    // binary behave as a minimal ACP agent for integration tests instead of
    // parsing CLI args. Production release builds compile this branch out.
    #[cfg(debug_assertions)]
    {
        let mut args = std::env::args_os();
        let _program = args.next();
        if args.next().as_deref() == Some(std::ffi::OsStr::new("__acps-test-fake-agent")) {
            let fake_args = args
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            return super::serve::run_fake_agent(fake_args);
        }
    }

    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            // `use_stderr()` is false for DisplayHelp / DisplayVersion — those are not failures.
            if error.use_stderr() {
                record_cli_error_message(&strip_ansi(&error.to_string()));
            }
            error.exit();
        }
    };
    run_cli(cli)
}

fn run_cli(cli: Cli) -> Result<()> {
    let output = OutputFormatChoice::new(cli.format);
    let result = match cli.command {
        Command::Completion { shell } => {
            output.reject_json("completion")?;
            let mut command = Cli::command();
            generate(shell, &mut command, "acps", &mut std::io::stdout());
            Ok(())
        }
        Command::Init(args) => {
            output.reject_json("init")?;
            super::init::run_init(*args)
        }
        Command::Status => super::status::run_status(output.effective()),
        Command::Reset(args) => {
            output.reject_json("reset")?;
            super::reset::run_reset(args)
        }
        Command::Serve(args) => {
            output.reject_json("serve")?;
            super::serve::run_serve(args)
        }
        Command::Auth { command } => {
            output.reject_json("auth")?;
            super::auth::run_auth_command(command)
        }
        Command::Secrets { command } => {
            super::secrets::run_secrets_command(command, output.effective())
        }
        Command::Config { command } => {
            super::config::run_config_command(command, output.effective())
        }
        Command::Logs { command } => super::logs::run_logs_command(command, output),
        Command::Agent { command } => super::agent::run_agent_command(command, output),
        Command::Subagent { command } => {
            output.reject_json("subagent")?;
            super::subagent::run_subagent_command(command)
        }
        Command::Installer { command } => {
            super::installer::run_installer_command(command, output.effective())
        }
        Command::Sessions { command } => {
            super::sessions::run_sessions_command(command, output.effective())
        }
        Command::Deps { command } => super::deps::run_deps_command(command, output.effective()),
        Command::Security { command } => super::security::run_security_command(command, output),
        Command::Metrics { command } => {
            super::metrics::run_metrics_command(command, output.effective())
        }
        Command::Ws { command } => super::ws::run_ws_command(command, output.effective()),
    };

    if let Err(error) = &result {
        // `acps reset` dry-run intentionally returns this error to signal the
        // operator must pass `--yes`. The dry-run contract is "exits without
        // touching the filesystem" — recording a `cli.error` row into
        // state.sqlite would violate that, so we skip the durable log for it.
        if !matches!(error, StackError::ResetNotConfirmed) {
            record_cli_error_message(&strip_ansi(&error.to_string()));
        }
    }

    result
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(super) enum OutputFormat {
    Text,
    Json,
}

impl OutputFormat {
    pub(super) fn is_json(self) -> bool {
        matches!(self, Self::Json)
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) struct OutputFormatChoice {
    format: OutputFormat,
    explicit: bool,
}

impl OutputFormatChoice {
    fn new(format: Option<OutputFormat>) -> Self {
        Self {
            format: format.unwrap_or(OutputFormat::Text),
            explicit: format.is_some(),
        }
    }

    pub(super) fn effective(self) -> OutputFormat {
        self.format
    }

    pub(super) fn reject_json(self, command: &'static str) -> Result<()> {
        if self.explicit && self.format == OutputFormat::Json {
            return Err(StackError::InvalidParam {
                field: "format",
                reason: format!("{command} does not support --format json"),
            });
        }
        Ok(())
    }

    pub(super) fn resolve_json_alias(self, json: bool, flag: &'static str) -> Result<OutputFormat> {
        if json && self.explicit && self.format == OutputFormat::Text {
            return Err(StackError::InvalidParam {
                field: flag,
                reason: "--json conflicts with --format text; use --format json or omit --format"
                    .to_owned(),
            });
        }
        if json {
            Ok(OutputFormat::Json)
        } else {
            Ok(self.format)
        }
    }
}

pub(super) fn print_json(data: &serde_json::Value) -> Result<()> {
    let rendered = serde_json::to_string_pretty(data).map_err(|source| StackError::ServeIo {
        source: std::io::Error::other(format!("serialize CLI JSON: {source}")),
    })?;
    println!("{rendered}");
    Ok(())
}

/// Tier of the API key used for a daemon-RPC call. Session-tier matches the
/// strict-tiering invariant: read/operate on session state with the session
/// key; never use the admin key for routine session operations. The single
/// variant is an enum so future admin-tier CLI helpers slot in without
/// reshaping the helper signatures.
#[derive(Debug, Clone, Copy)]
pub(super) enum CliKey {
    Session,
    Admin,
}

#[derive(Debug, Clone, Copy)]
pub(super) enum CliMethod {
    Get,
    Post,
    Delete,
}

/// Generalized daemon-RPC helper. Loads the configured key from the secret
/// store, builds the URL, dispatches, and parses the success envelope.
pub(super) async fn daemon_request(
    base_url: &str,
    method: CliMethod,
    path: &str,
    key: &str,
    body: Option<&serde_json::Value>,
) -> Result<serde_json::Value> {
    let url = format!("{}{}", base_url.trim_end_matches('/'), path);
    // Static `path` is the spec contract for error reporting; keep it pinned
    // to a small set of known prefixes so `AgentApiRequest::path` stays a
    // useful diagnostic and we don't accidentally leak path params to logs.
    let path_label: &'static str = static_path_label(path);
    let client = reqwest::Client::new();
    let request = match method {
        CliMethod::Get => client.get(&url),
        CliMethod::Post => client.post(&url),
        CliMethod::Delete => client.delete(&url),
    }
    .bearer_auth(key);
    let request = if let Some(body) = body {
        request.json(body)
    } else {
        request
    };
    let response = request
        .send()
        .await
        .map_err(|source| StackError::AgentApiRequest {
            path: path_label,
            source,
        })?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|source| StackError::AgentApiRequest {
            path: path_label,
            source,
        })?;
    if !status.is_success() {
        return Err(StackError::AgentApiStatus {
            path: path_label,
            status,
            body,
        });
    }
    serde_json::from_str(&body).map_err(|err| StackError::AgentInitializeFailed {
        reason: format!("daemon response was not JSON: {err}"),
    })
}

fn static_path_label(path: &str) -> &'static str {
    // Strip the query string before bucketing so callers passing `?limit=` etc.
    // still resolve to the canonical path label.
    let bare = path.split('?').next().unwrap_or(path);
    if bare == "/v1/status" {
        "/v1/status"
    } else if bare == "/v1/status/agent" {
        "/v1/status/agent"
    } else if bare == "/v1/agent/status" {
        "/v1/agent/status"
    } else if bare == "/v1/status/connections" {
        "/v1/status/connections"
    } else if bare == "/v1/health/live" {
        "/v1/health/live"
    } else if bare == "/v1/health/ready" {
        "/v1/health/ready"
    } else if bare == "/v1/config/export" {
        "/v1/config/export"
    } else if bare == "/v1/config/validate" {
        "/v1/config/validate"
    } else if bare == "/v1/agent/capabilities" {
        "/v1/agent/capabilities"
    } else if bare == "/v1/agent/install" {
        "/v1/agent/install"
    } else if bare == "/v1/agent/start" {
        "/v1/agent/start"
    } else if bare == "/v1/agent/stop" {
        "/v1/agent/stop"
    } else if bare == "/v1/agent/restart" {
        "/v1/agent/restart"
    } else if bare == "/v1/agent/switch" {
        "/v1/agent/switch"
    } else if bare == "/v1/logs/events" {
        "/v1/logs/events"
    } else if bare == "/v1/logs/commands" {
        "/v1/logs/commands"
    } else if bare == "/v1/logs/permissions" {
        "/v1/logs/permissions"
    } else if bare == "/v1/logs/security" {
        "/v1/logs/security"
    } else if bare == "/v1/logs/sessions" {
        "/v1/logs/sessions"
    } else if bare == "/v1/metrics/summary" {
        "/v1/metrics/summary"
    } else if bare == "/v1/workspace" {
        "/v1/workspace"
    } else if bare == "/v1/files" {
        "/v1/files"
    } else if bare == "/v1/files/content" {
        "/v1/files/content"
    } else if bare == "/v1/files/upload" {
        "/v1/files/upload"
    } else if bare == "/v1/files/download" {
        "/v1/files/download"
    } else if bare == "/v1/commands" {
        "/v1/commands"
    } else if bare.starts_with("/v1/commands/") && bare.ends_with("/output") {
        "/v1/commands/{id}/output"
    } else if bare.starts_with("/v1/commands/") && bare.ends_with("/cancel") {
        "/v1/commands/{id}/cancel"
    } else if bare.starts_with("/v1/commands/") {
        "/v1/commands/{id}"
    } else if bare == "/v1/deps" {
        "/v1/deps"
    } else if bare == "/v1/deps/check" {
        "/v1/deps/check"
    } else if bare == "/v1/providers" {
        "/v1/providers"
    } else if bare == "/v1/models" {
        "/v1/models"
    } else if bare == "/v1/permissions/pending" {
        "/v1/permissions/pending"
    } else if bare.starts_with("/v1/permissions/") && bare.ends_with("/approve") {
        "/v1/permissions/{id}/approve"
    } else if bare.starts_with("/v1/permissions/") && bare.ends_with("/deny") {
        "/v1/permissions/{id}/deny"
    } else if bare == "/v1/security/check" {
        "/v1/security/check"
    } else if bare == "/v1/security/history" {
        "/v1/security/history"
    } else if bare.starts_with("/v1/security/history/") {
        "/v1/security/history/{run_id}"
    } else if bare == "/v1/ws/connections" {
        "/v1/ws/connections"
    } else if bare == "/v1/ws/sessions" {
        "/v1/ws/sessions"
    } else if bare == "/v1/ws/connections/disconnect" {
        "/v1/ws/connections/disconnect"
    } else if bare == "/v1/ws/sessions/disconnect" {
        "/v1/ws/sessions/disconnect"
    } else if bare == "/v1/sessions" {
        "/v1/sessions"
    } else if bare == "/v1/sessions/-/status" {
        "/v1/sessions/-/status"
    } else if bare.starts_with("/v1/sessions/") && bare.ends_with("/prompt") {
        "/v1/sessions/{id}/prompt"
    } else if bare.starts_with("/v1/sessions/") && bare.ends_with("/cancel") {
        "/v1/sessions/{id}/cancel"
    } else if bare.starts_with("/v1/sessions/") && bare.ends_with("/load") {
        "/v1/sessions/{id}/load"
    } else if bare.starts_with("/v1/sessions/") && bare.ends_with("/resume") {
        "/v1/sessions/{id}/resume"
    } else if bare.starts_with("/v1/sessions/") && bare.contains("/prompts/") {
        "/v1/sessions/{id}/prompts/{prompt_id}"
    } else if bare.starts_with("/v1/sessions/") && bare.ends_with("/events") {
        "/v1/sessions/{id}/events"
    } else if bare.starts_with("/v1/sessions/") && bare.ends_with("/snapshot") {
        "/v1/sessions/{id}/snapshot"
    } else if bare.starts_with("/v1/sessions/") {
        "/v1/sessions/{id}"
    } else {
        // The remaining callers in this CLI pass static literals listed
        // explicitly in this match.
        "/v1/agent"
    }
}

/// Percent-encode a single URL path segment using the "unreserved" RFC 3986
/// allowlist. ACP session and prompt IDs are opaque strings — an agent that
/// returned `sess_a/b` (with a slash) would otherwise be routed as a
/// different resource entirely, which is both a correctness bug and a
/// path-injection vector for any client that forwards untrusted IDs.
pub(super) fn encode_path_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.as_bytes() {
        let b = *byte;
        let is_unreserved =
            b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.' || b == b'~';
        if is_unreserved {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub(super) fn open_cli_key(config: &Config, home: &std::path::Path, key: CliKey) -> Result<String> {
    let store = SecretStore::open(home)?;
    let name = match key {
        CliKey::Session => &config.auth.session_key_ref,
        CliKey::Admin => &config.auth.admin_key_ref,
    };
    Ok(store.get(name)?.to_owned())
}

pub(super) fn daemon_base_url(public_url: Option<&str>, bind: &str) -> Result<String> {
    if let Some(public_url) = public_url.filter(|value| !value.trim().is_empty()) {
        return Ok(public_url.trim_end_matches('/').to_owned());
    }
    let socket: SocketAddr = bind
        .parse()
        .map_err(|_| StackError::InvalidSocketAddress { field: "api.bind" })?;
    let host = match socket.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST).to_string(),
        IpAddr::V6(ip) if ip.is_unspecified() => format!("[{}]", Ipv6Addr::LOCALHOST),
        IpAddr::V4(ip) => ip.to_string(),
        IpAddr::V6(ip) => format!("[{ip}]"),
    };
    Ok(format!("http://{host}:{}", socket.port()))
}

fn record_cli_error_message(error_message: &str) {
    let Ok(home) = home_dir() else {
        return;
    };
    let state_path = default_state_path(&home);
    if !state_path.exists() {
        return;
    }
    // Repair the existing file's mode before opening, so the error row is not written
    // while the database is still readable by other local users.
    if set_owner_only_file(&state_path).is_err() {
        return;
    }
    let Ok(store) = StateStore::open(&state_path) else {
        return;
    };
    if store.migrate().is_err() {
        return;
    }
    let payload = serde_json::json!({ "error": error_message }).to_string();
    if let Err(log_error) = store.append_event_with_source(
        "error",
        "cli.error",
        crate::state::EVENT_SOURCE_CLI,
        "command failed",
        &payload,
    ) {
        eprintln!("failed to record CLI error: {log_error}");
    }
}

fn strip_ansi(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for ch in chars.by_ref() {
                let code = ch as u32;
                if (0x40..=0x7e).contains(&code) {
                    break;
                }
            }
        } else {
            output.push(c);
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::{daemon_base_url, static_path_label, strip_ansi};

    #[test]
    fn strip_ansi_removes_csi_sequences() {
        assert_eq!(strip_ansi("\x1b[31mred\x1b[0m"), "red");
        assert_eq!(
            strip_ansi("plain \x1b[1;33mhighlight\x1b[0m end"),
            "plain highlight end"
        );
    }

    #[test]
    fn strip_ansi_passes_plain_text_unchanged() {
        assert_eq!(strip_ansi("nothing to strip"), "nothing to strip");
    }

    #[test]
    fn strip_ansi_preserves_other_control_characters() {
        // Tabs, newlines, and other control chars survive: serde_json escapes them downstream.
        assert_eq!(strip_ansi("a\tb\nc"), "a\tb\nc");
    }

    #[test]
    fn daemon_base_url_prefers_public_url() {
        assert_eq!(
            daemon_base_url(Some("https://agent.example.com/root"), "0.0.0.0:7700").expect("url"),
            "https://agent.example.com/root"
        );
    }

    #[test]
    fn daemon_base_url_rewrites_wildcard_binds_to_loopback() {
        assert_eq!(
            daemon_base_url(None, "0.0.0.0:7700").expect("url"),
            "http://127.0.0.1:7700"
        );
        assert_eq!(
            daemon_base_url(None, "[::]:7700").expect("url"),
            "http://[::1]:7700"
        );
    }

    #[test]
    fn daemon_base_url_preserves_explicit_loopback_bind() {
        assert_eq!(
            daemon_base_url(None, "127.0.0.1:7700").expect("url"),
            "http://127.0.0.1:7700"
        );
    }

    #[test]
    fn static_path_label_covers_cli_daemon_routes() {
        let cases = [
            ("/v1/metrics/summary?range=day", "/v1/metrics/summary"),
            ("/v1/logs/events?limit=10", "/v1/logs/events"),
            ("/v1/files/content?path=README.md", "/v1/files/content"),
            ("/v1/commands/cmd_123/output", "/v1/commands/{id}/output"),
            (
                "/v1/permissions/pending?limit=10",
                "/v1/permissions/pending",
            ),
            (
                "/v1/sessions/sess_123/snapshot",
                "/v1/sessions/{id}/snapshot",
            ),
            ("/v1/agent/restart", "/v1/agent/restart"),
        ];
        for (input, expected) in cases {
            assert_eq!(static_path_label(input), expected);
        }
    }
}
