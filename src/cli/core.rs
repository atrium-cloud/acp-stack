use crate::auth::{AuthVerifierEnsureOutcome, KeyKind, ensure_auth_verifier_pair};
use crate::config::Config;
use crate::error::{Result, StackError};
use crate::fs_util::{home_dir, set_owner_only_file};
use crate::state::{StateStore, default_state_path};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::{Shell, generate};
use std::io::IsTerminal;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use super::agent::AgentCommand;
use super::array::ArrayCommand;
use super::auth::AuthCommand;
use super::config::ConfigCommand;
use super::deps::DepsCommand;
#[cfg(feature = "dev-tools")]
use super::init::InitArgs;
use super::init::{InitCommand, InitMode};
use super::installer::InstallerCommand;
use super::logging::LoggingCommand;
use super::logs::LogsCommand;
use super::metrics::MetricsCommand;
use super::reset::ResetArgs;
use super::secrets::SecretsCommand;
use super::security::SecurityCommand;
use super::serve::{ServeArgs, ServeMode};
use super::sessions::SessionsCommand;
use super::subagent::SubagentCommand;
#[cfg(feature = "stack-self-update")]
use super::update::UpdateCommand;
use super::ws::WsCommand;

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
}

#[cfg(feature = "dev-tools")]
#[derive(Debug, Subcommand)]
enum DevCommand {
    /// Initialize with development-only flags enabled.
    #[command(mut_arg("skip_workspace_init", |arg| arg.hide(false)))]
    Init(Box<InitArgs>),
    /// Run the daemon with development-only flags enabled.
    #[command(mut_arg("allow_root", |arg| arg.hide(false)))]
    Serve(ServeArgs),
}

pub fn run() -> Result<()> {
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
            super::init::run_init_command(*args, InitMode::Operator)
        }
        #[cfg(feature = "dev-tools")]
        Command::Dev { command } => {
            output.reject_json("dev")?;
            match command {
                DevCommand::Init(args) => super::init::run_init(*args, InitMode::Dev),
                DevCommand::Serve(args) => super::serve::run_serve(args, ServeMode::Dev),
            }
        }
        Command::Status => super::status::run_status(output.effective()),
        #[cfg(feature = "stack-self-update")]
        Command::Update { command } => {
            super::update::run_update_command(command, output.effective())
        }
        Command::Reset(args) => {
            output.reject_json("reset")?;
            super::reset::run_reset(args)
        }
        Command::Serve(args) => {
            output.reject_json("serve")?;
            super::serve::run_serve(args, ServeMode::Operator)
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
        Command::Logging { command } => {
            super::logging::run_logging_command(command, output.effective())
        }
        Command::Logs { command } => super::logs::run_logs_command(command, output),
        Command::Agent { command } => super::agent::run_agent_command(command, output),
        Command::Array { command } => super::array::run_array_command(command, output.effective()),
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

pub(super) const SESSION_KEY_ENV: &str = "ACP_STACK_SESSION_KEY";

#[derive(Debug, Clone, Copy)]
pub(super) enum CliMethod {
    Get,
    Post,
    Put,
    Delete,
}

/// Generalized daemon-RPC helper. Callers supply the explicit bearer key.
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
        CliMethod::Put => client.put(&url),
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

pub(super) async fn local_daemon_request(
    config: &Config,
    method: CliMethod,
    path: &str,
    body: Option<&serde_json::Value>,
) -> Result<serde_json::Value> {
    let (status, body) = local_daemon_json_response(config, method, path, body).await?;
    if !status.is_success() {
        return Err(StackError::AgentApiStatus {
            path: static_path_label(path),
            status,
            body: body.to_string(),
        });
    }
    Ok(body)
}

pub(super) async fn local_daemon_json_response(
    config: &Config,
    method: CliMethod,
    path: &str,
    body: Option<&serde_json::Value>,
) -> Result<(http::StatusCode, serde_json::Value)> {
    let socket_path = local_socket_path(config)?;
    let body_bytes = body.map(serde_json::to_vec).transpose().map_err(|source| {
        StackError::AgentInitializeFailed {
            reason: format!("serialize local daemon request body: {source}"),
        }
    })?;
    let response = local_http_request(&socket_path, method.as_str(), path, body_bytes).await?;
    let status = http::StatusCode::from_u16(response.status)
        .unwrap_or(http::StatusCode::INTERNAL_SERVER_ERROR);
    let body_text =
        String::from_utf8(response.body).map_err(|source| StackError::AgentInitializeFailed {
            reason: format!("local daemon response was not UTF-8: {source}"),
        })?;
    let body =
        serde_json::from_str(&body_text).map_err(|err| StackError::AgentInitializeFailed {
            reason: format!("local daemon response was not JSON: {err}"),
        })?;
    Ok((status, body))
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
    } else if bare == "/v1/auth/session-key/regenerate" {
        "/v1/auth/session-key/regenerate"
    } else if bare == "/v1/auth/local-session-access" {
        "/v1/auth/local-session-access"
    } else if bare == "/v1/config/validate" {
        "/v1/config/validate"
    } else if bare == "/v1/agent/capabilities" {
        "/v1/agent/capabilities"
    } else if bare == "/v1/array/status" {
        "/v1/array/status"
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
    } else if bare.starts_with("/v1/array/targets/") && bare.ends_with("/capabilities") {
        "/v1/array/targets/{target_id}/capabilities"
    } else if bare.starts_with("/v1/array/targets/") && bare.ends_with("/install") {
        "/v1/array/targets/{target_id}/install"
    } else if bare.starts_with("/v1/array/targets/") && bare.ends_with("/start") {
        "/v1/array/targets/{target_id}/start"
    } else if bare.starts_with("/v1/array/targets/") && bare.ends_with("/stop") {
        "/v1/array/targets/{target_id}/stop"
    } else if bare.starts_with("/v1/array/targets/") && bare.ends_with("/restart") {
        "/v1/array/targets/{target_id}/restart"
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
    } else if bare.starts_with("/v1/sessions/") && bare.ends_with("/fork") {
        "/v1/sessions/{id}/fork"
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

impl CliMethod {
    fn as_str(self) -> &'static str {
        match self {
            CliMethod::Get => "GET",
            CliMethod::Post => "POST",
            CliMethod::Put => "PUT",
            CliMethod::Delete => "DELETE",
        }
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

pub(super) fn resolve_session_key(value: Option<String>) -> Result<String> {
    if let Some(key) = value {
        return validate_key_input("--session-key", key);
    }
    if let Ok(key) = std::env::var(SESSION_KEY_ENV) {
        return validate_key_input(SESSION_KEY_ENV, key);
    }
    Err(StackError::MissingField {
        field: "--session-key or ACP_STACK_SESSION_KEY",
    })
}

pub(super) enum SessionAccess {
    Bearer(String),
    Local,
}

pub(super) fn resolve_session_access(
    config: &Config,
    value: Option<String>,
) -> Result<SessionAccess> {
    if let Some(key) = value {
        return validate_key_input("--session-key", key).map(SessionAccess::Bearer);
    }
    if let Ok(key) = std::env::var(SESSION_KEY_ENV) {
        return validate_key_input(SESSION_KEY_ENV, key).map(SessionAccess::Bearer);
    }
    if config.local.session_auth == crate::config::LocalSessionAuth::Keyless {
        return Ok(SessionAccess::Local);
    }
    Err(StackError::MissingField {
        field: "--session-key or ACP_STACK_SESSION_KEY (or enable local session access with `acps auth local-session-access enable`)",
    })
}

pub(super) fn resolve_admin_key(value: Option<String>, interactive: bool) -> Result<String> {
    if let Some(key) = value {
        return validate_key_input("--admin-key", key);
    }
    if interactive && std::io::stdin().is_terminal() {
        let key = rpassword::prompt_password("admin key: ")
            .map_err(|source| StackError::ServeIo { source })?;
        return validate_key_input("--admin-key", key);
    }
    Err(StackError::MissingField {
        field: "--admin-key",
    })
}

pub(super) fn validate_local_admin_key(key: &str) -> Result<()> {
    let home = home_dir()?;
    let loaded_config = Config::load_from_default_path_with_legacy()?;
    let state_path = default_state_path(&home);
    let store = StateStore::open(&state_path)?;
    store.migrate()?;
    match ensure_auth_verifier_pair(&store, loaded_config.legacy_auth.as_ref(), &home)? {
        AuthVerifierEnsureOutcome::Preserved
        | AuthVerifierEnsureOutcome::BackfilledLegacySecrets => {}
        AuthVerifierEnsureOutcome::Missing => {
            return Err(StackError::MissingField {
                field: "auth_keys.session and auth_keys.admin",
            });
        }
    }
    validate_admin_key_against_store(&store, key)
}

pub(super) fn validate_local_admin_key_from_state(key: &str) -> Result<()> {
    let home = home_dir()?;
    let state_path = default_state_path(&home);
    let store = StateStore::open(&state_path)?;
    store.migrate()?;
    validate_admin_key_against_store(&store, key)
}

fn validate_admin_key_against_store(store: &StateStore, key: &str) -> Result<()> {
    let verifiers = store.load_auth_verifier_pair()?;
    if verifiers.verify(key) == Some(KeyKind::Admin) {
        Ok(())
    } else {
        Err(StackError::InvalidParam {
            field: "--admin-key",
            reason: "admin key did not validate against local auth verifier".to_owned(),
        })
    }
}

fn validate_key_input(field: &'static str, value: String) -> Result<String> {
    if value.trim().is_empty() || value.trim().len() != value.len() {
        return Err(StackError::MissingField { field });
    }
    Ok(value)
}

pub(super) fn local_socket_path(config: &Config) -> Result<PathBuf> {
    if let Some(path) = config.local.socket_path.as_deref() {
        return Ok(PathBuf::from(path));
    }
    crate::local_listener::default_socket_path()
}

struct LocalHttpResponse {
    status: u16,
    body: Vec<u8>,
}

async fn local_http_request(
    socket: &Path,
    method: &str,
    path: &str,
    body: Option<Vec<u8>>,
) -> Result<LocalHttpResponse> {
    let mut stream = UnixStream::connect(socket)
        .await
        .map_err(|source| StackError::ServeIo { source })?;
    let body_bytes = body.unwrap_or_default();
    let mut request =
        format!("{method} {path} HTTP/1.1\r\nHost: acps.local\r\nConnection: close\r\n");
    if !body_bytes.is_empty() {
        request.push_str("Content-Type: application/json\r\n");
    }
    request.push_str(&format!("Content-Length: {}\r\n\r\n", body_bytes.len()));
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|source| StackError::ServeIo { source })?;
    if !body_bytes.is_empty() {
        stream
            .write_all(&body_bytes)
            .await
            .map_err(|source| StackError::ServeIo { source })?;
    }
    let mut raw = Vec::with_capacity(4096);
    stream
        .read_to_end(&mut raw)
        .await
        .map_err(|source| StackError::ServeIo { source })?;
    parse_local_http_response(&raw)
}

fn parse_local_http_response(raw: &[u8]) -> Result<LocalHttpResponse> {
    let header_end = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| StackError::AgentInitializeFailed {
            reason: "local daemon response missing header terminator".to_owned(),
        })?;
    let header_text = std::str::from_utf8(&raw[..header_end]).map_err(|source| {
        StackError::AgentInitializeFailed {
            reason: format!("local daemon response headers were not UTF-8: {source}"),
        }
    })?;
    let mut lines = header_text.split("\r\n");
    let status_line = lines
        .next()
        .ok_or_else(|| StackError::AgentInitializeFailed {
            reason: "local daemon response missing status line".to_owned(),
        })?;
    let status = status_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| StackError::AgentInitializeFailed {
            reason: "local daemon response status code missing".to_owned(),
        })?
        .parse::<u16>()
        .map_err(|source| StackError::AgentInitializeFailed {
            reason: format!("local daemon response status code was invalid: {source}"),
        })?;
    let mut content_length: Option<usize> = None;
    for line in lines {
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            content_length = Some(value.trim().parse::<usize>().map_err(|source| {
                StackError::AgentInitializeFailed {
                    reason: format!("local daemon response Content-Length was invalid: {source}"),
                }
            })?);
        }
    }
    let body_start = header_end + 4;
    let body = match content_length {
        Some(length) => {
            let end = body_start + length;
            if raw.len() < end {
                return Err(StackError::AgentInitializeFailed {
                    reason: format!(
                        "local daemon response truncated: Content-Length={length}, available={}",
                        raw.len().saturating_sub(body_start)
                    ),
                });
            }
            raw[body_start..end].to_vec()
        }
        None => raw[body_start..].to_vec(),
    };
    Ok(LocalHttpResponse { status, body })
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
            (
                "/v1/auth/local-session-access",
                "/v1/auth/local-session-access",
            ),
            ("/v1/agent/restart", "/v1/agent/restart"),
        ];
        for (input, expected) in cases {
            assert_eq!(static_path_label(input), expected);
        }
    }
}
