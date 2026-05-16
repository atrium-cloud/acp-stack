use crate::agent_installer::run_installer;
use crate::api::{self, AppState};
use crate::auth::generate_api_key;
use crate::config::{self, Config};
use crate::error::{Result, StackError};
use crate::fs_util::{
    atomic_write_owner_only, create_dir_owner_only, home_dir, parent_dir, pre_create_owner_only,
    set_owner_only_dir, set_owner_only_file, write_new_file_owner_only,
};
use crate::secrets::{SecretStore, age_key_path, reject_auth_ref_mutation, secret_store_path};
use crate::state::{EventFilter, StateStore, default_state_path};
use crate::supervisor::ServerLifecycle;
use base64::Engine;
use clap::{Args, Parser, Subcommand};
use std::collections::HashMap;
use std::io::BufRead as _;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "acps",
    version,
    about = env!("CARGO_PKG_DESCRIPTION"),
    color = clap::ColorChoice::Never,
)]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Init,
    Status,
    Reset(ResetArgs),
    /// Run the HTTP daemon in the foreground. Blocks until SIGTERM or SIGINT.
    Serve(ServeArgs),
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    Secrets {
        #[command(subcommand)]
        command: SecretsCommand,
    },
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    Logs {
        #[command(subcommand)]
        command: LogsCommand,
    },
    Agent {
        #[command(subcommand)]
        command: AgentCommand,
    },
    Sessions {
        #[command(subcommand)]
        command: SessionsCommand,
    },
    Deps {
        #[command(subcommand)]
        command: DepsCommand,
    },
    /// Inspect derived runtime metrics.
    Metrics {
        #[command(subcommand)]
        command: MetricsCommand,
    },
}

#[derive(Debug, Subcommand)]
enum MetricsCommand {
    /// Print the metrics summary (counts, durations, percentiles).
    Summary(MetricsSummaryArgs),
}

#[derive(Debug, Args)]
struct MetricsSummaryArgs {
    /// Window start. Accepts `1h`/`30m`/`2d` or an RFC3339 timestamp.
    /// Defaults to 24h ago.
    #[arg(long)]
    since: Option<String>,
    /// Window end. Same format as `--since`. Defaults to now.
    #[arg(long)]
    until: Option<String>,
}

#[derive(Debug, Subcommand)]
enum DepsCommand {
    /// Print declared dependency status.
    Check,
}

#[derive(Debug, Subcommand)]
enum SessionsCommand {
    /// List sessions newest-first.
    List(SessionsListArgs),
    /// Create a new session through the running daemon.
    New(SessionsNewArgs),
    /// Send a prompt to a session. Polls until completion unless `--no-wait`.
    Prompt(SessionsPromptArgs),
    /// Cancel any in-flight prompts and notify the agent.
    Cancel(SessionsTargetArgs),
    /// Close the session on the agent side and mark it closed locally.
    Close(SessionsTargetArgs),
}

#[derive(Debug, Args)]
struct SessionsListArgs {
    #[arg(long, default_value_t = 50)]
    limit: u32,
}

#[derive(Debug, Args)]
struct SessionsNewArgs {
    /// Optional working directory for the new session; defaults to
    /// `workspace.root` configured for the runtime.
    #[arg(long)]
    cwd: Option<String>,
}

#[derive(Debug, Args)]
struct SessionsPromptArgs {
    session_id: String,
    /// Prompt text. If omitted, the CLI reads stdin until EOF.
    text: Option<String>,
    /// Return immediately with the prompt id without polling completion.
    #[arg(long)]
    no_wait: bool,
    /// Maximum seconds to wait before giving up on the prompt (ignored when
    /// `--no-wait` is set). The daemon keeps the task running regardless.
    #[arg(long, default_value_t = 300)]
    timeout_secs: u64,
}

#[derive(Debug, Args)]
struct SessionsTargetArgs {
    session_id: String,
}

#[derive(Debug, Subcommand)]
enum AgentCommand {
    /// Install the configured ACP agent or adapter.
    Install,
    /// Ask the running daemon to start the configured agent.
    Start,
    /// Ask the running daemon to stop the configured agent.
    Stop,
    /// Print the latest persisted agent state from SQLite.
    Status,
}

#[derive(Debug, Args)]
struct ServeArgs {
    /// Override the `api.bind` address from config.
    #[arg(long)]
    bind: Option<String>,
}

#[derive(Debug, Args)]
struct ResetArgs {
    /// Confirm deletion of config, state, age key, and secret store.
    #[arg(long)]
    yes: bool,
}

#[derive(Debug, Subcommand)]
enum AuthCommand {
    /// Generate a new session key and store it in the encrypted secret store.
    /// The admin key is not regenerable; use `acps reset --yes` to rotate it.
    RegenerateSessionKey,
}

#[derive(Debug, Subcommand)]
enum SecretsCommand {
    /// List secret reference names. Values are never printed.
    List,
    /// Read a single line from stdin and store it as the named secret.
    Set(SecretsSetArgs),
    /// Remove the named secret from the store.
    Delete(SecretsDeleteArgs),
}

#[derive(Debug, Args)]
struct SecretsSetArgs {
    name: String,
}

#[derive(Debug, Args)]
struct SecretsDeleteArgs {
    name: String,
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    Validate(ConfigValidateArgs),
    Export(ConfigExportArgs),
    Import(ConfigImportArgs),
}

#[derive(Debug, Args)]
struct ConfigValidateArgs {
    path: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct ConfigExportArgs {
    #[arg(long)]
    output: Option<PathBuf>,
    #[arg(long)]
    base64: bool,
}

#[derive(Debug, Args)]
struct ConfigImportArgs {
    /// Path to a TOML config file. Mutually exclusive with --base64.
    path: Option<PathBuf>,
    /// Base64-encoded canonical TOML. Mutually exclusive with `path`.
    #[arg(long, conflicts_with = "path")]
    base64: Option<String>,
    /// Replace the existing config; without --force, import refuses to clobber.
    #[arg(long)]
    force: bool,
}

#[derive(Debug, Subcommand)]
enum LogsCommand {
    Query(LogsQueryArgs),
    Tail(LogsTailArgs),
}

#[derive(Debug, Args)]
struct LogsQueryArgs {
    #[arg(long, default_value_t = 50)]
    limit: u32,
    #[arg(long)]
    level: Option<String>,
    /// Lower time bound. Accepts a duration suffix (`1h`/`30m`/`2d`) or an
    /// RFC3339 timestamp. When a suffix is supplied it's interpreted as
    /// "this much time ago".
    #[arg(long)]
    since: Option<String>,
    /// Upper time bound. Same format as `--since`. Defaults to "now".
    #[arg(long)]
    until: Option<String>,
    /// Exact event kind, or a dotted prefix when the value ends with `.`
    /// (e.g. `command.` matches `command.started`, `command.exited`, ...).
    #[arg(long)]
    kind: Option<String>,
    /// Filter by writer source (`api`, `acp`, `command`, `permission`, `cli`,
    /// `system`).
    #[arg(long)]
    source: Option<String>,
    /// Show events scoped to a single session id.
    #[arg(long)]
    session: Option<String>,
    /// Show events whose payload carries this command id.
    #[arg(long)]
    command: Option<String>,
    /// Show events whose payload carries this permission id.
    #[arg(long)]
    permission: Option<String>,
    /// Continuation cursor from a previous page (the last returned event id).
    #[arg(long)]
    after: Option<String>,
}

#[derive(Debug, Args)]
struct LogsTailArgs {
    /// WebSocket topic to subscribe to. May be passed multiple times. Defaults
    /// to `logs`. Valid: `logs`, `workspace`, `agent`, `status`,
    /// `sessions.{id}`, `commands.{id}`.
    #[arg(long = "topic")]
    topics: Vec<String>,
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
            return run_fake_agent(fake_args);
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
    let result = match cli.command {
        Command::Init => run_init(),
        Command::Status => run_status(),
        Command::Reset(args) => run_reset(args),
        Command::Serve(args) => run_serve(args),
        Command::Auth { command } => run_auth_command(command),
        Command::Secrets { command } => run_secrets_command(command),
        Command::Config { command } => run_config_command(command),
        Command::Logs { command } => run_logs_command(command),
        Command::Agent { command } => run_agent_command(command),
        Command::Sessions { command } => run_sessions_command(command),
        Command::Deps { command } => run_deps_command(command),
        Command::Metrics { command } => run_metrics_command(command),
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

fn run_config_command(command: ConfigCommand) -> Result<()> {
    match command {
        ConfigCommand::Validate(args) => {
            load_config(args.path)?;
            println!("config is valid");
            Ok(())
        }
        ConfigCommand::Export(args) => {
            let config = Config::load_from_default_path()?;
            let canonical = config.to_canonical_toml()?;
            let output = if args.base64 {
                base64::engine::general_purpose::STANDARD.encode(canonical)
            } else {
                canonical
            };

            if let Some(path) = args.output {
                std::fs::write(&path, output)
                    .map_err(|source| StackError::ConfigWrite { path, source })?;
            } else {
                println!("{output}");
            }

            Ok(())
        }
        ConfigCommand::Import(args) => run_config_import(args),
    }
}

fn run_config_import(args: ConfigImportArgs) -> Result<()> {
    let raw_toml = match (args.path.as_deref(), args.base64.as_deref()) {
        (None, None) => {
            return Err(StackError::MissingField {
                field: "config import requires either <path> or --base64",
            });
        }
        (Some(_), Some(_)) => {
            return Err(StackError::MissingField {
                field: "config import accepts only one of <path> or --base64",
            });
        }
        (Some(path), None) => {
            std::fs::read_to_string(path).map_err(|source| StackError::ConfigRead {
                path: path.to_path_buf(),
                source,
            })?
        }
        (None, Some(encoded)) => {
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_err(|source| StackError::ImportBase64Decode { source })?;
            String::from_utf8(decoded).map_err(|source| StackError::ImportUtf8 { source })?
        }
    };

    let config = config::load_config_from_str(&raw_toml)?;
    let canonical = config.to_canonical_toml()?;
    let target = config::default_config_path()?;

    let target_dir = parent_dir(&target)?;
    create_dir_owner_only(target_dir)?;

    if target.exists() {
        if !args.force {
            return Err(StackError::ConfigExists {
                path: target.clone(),
            });
        }
        // Refuse to change the auth-ref names through import. Allowing it
        // would let an operator point `admin_key_ref` at a secret of their
        // own choosing, effectively replacing the original admin key without
        // going through `acps reset --yes` — bypassing the documented
        // reset-only rotation path for the admin key.
        let current = Config::load_from_path(&target)?;
        config::compare_auth_refs(&current.auth, &config.auth)?;
        // Atomic replace via temp file + rename, with owner-only mode on both
        // the temp and the final file. Avoids leaving a truncated config on
        // crash mid-write, which would otherwise brick the next `acps` run.
        atomic_write_owner_only(&target, canonical.as_bytes())?;
        println!("imported config (replaced): {}", target.display());
    } else {
        write_new_file_owner_only(&target, canonical.as_bytes())?;
        println!("imported config: {}", target.display());
    }

    Ok(())
}

fn run_auth_command(command: AuthCommand) -> Result<()> {
    match command {
        AuthCommand::RegenerateSessionKey => run_auth_regenerate_session_key(),
    }
}

fn run_auth_regenerate_session_key() -> Result<()> {
    let home = home_dir()?;
    let config = Config::load_from_default_path()?;
    let mut store = SecretStore::open(&home)?;
    let session_ref = config.auth.session_key_ref.clone();
    let admin_ref = config.auth.admin_key_ref.clone();
    // Mirror the init invariant: any operation on the auth secret pair must
    // see both refs in the store. Otherwise rotation could silently create a
    // new session secret in a half-initialized store where the admin key was
    // separately deleted, papering over an anomaly that should require reset.
    if !store.contains(&admin_ref) {
        return Err(StackError::MissingAdminKey { name: admin_ref });
    }
    if !store.contains(&session_ref) {
        return Err(StackError::MissingSessionKey { name: session_ref });
    }
    let new_key = generate_api_key();
    store.set(&session_ref, &new_key)?;
    println!("session key rotated");
    println!("reference: {session_ref}");
    println!("value: {new_key}");
    println!("update any clients with the new value; the previous key is now invalid");
    Ok(())
}

fn run_secrets_command(command: SecretsCommand) -> Result<()> {
    let home = home_dir()?;
    match command {
        SecretsCommand::List => {
            let store = SecretStore::open(&home)?;
            for name in store.list_names() {
                println!("{name}");
            }
            Ok(())
        }
        SecretsCommand::Set(args) => {
            let config = Config::load_from_default_path()?;
            reject_auth_ref_mutation(&args.name, &config)?;
            // Read a single line from stdin; trailing CR/LF stripped. Values
            // are single-line text by spec — multi-line input would silently
            // store the rest of stdin, which is surprising.
            let mut buffer = String::new();
            std::io::stdin()
                .lock()
                .read_line(&mut buffer)
                .map_err(|source| StackError::StdinRead { source })?;
            let value = buffer.trim_end_matches(['\n', '\r']);
            let mut store = SecretStore::open(&home)?;
            store.set(&args.name, value)?;
            println!("set secret: {}", args.name);
            Ok(())
        }
        SecretsCommand::Delete(args) => {
            let config = Config::load_from_default_path()?;
            reject_auth_ref_mutation(&args.name, &config)?;
            let mut store = SecretStore::open(&home)?;
            store.delete(&args.name)?;
            println!("deleted secret: {}", args.name);
            Ok(())
        }
    }
}

fn run_reset(args: ResetArgs) -> Result<()> {
    let home = home_dir()?;
    let config_path = config::default_config_path()?;
    let state_path = default_state_path(&home);
    let age_key = age_key_path(&home);
    let store_path = secret_store_path(&home);

    let targets = [&config_path, &state_path, &age_key, &store_path];

    if !args.yes {
        println!("acps reset would delete:");
        for target in targets {
            println!("  {}", target.display());
        }
        println!("re-run with --yes to confirm");
        return Err(StackError::ResetNotConfirmed);
    }

    for target in targets {
        if !target.exists() {
            continue;
        }
        std::fs::remove_file(target).map_err(|source| StackError::FileRemove {
            path: target.to_path_buf(),
            source,
        })?;
    }

    println!("reset acp-stack: removed config, state, age key, and secret store");
    Ok(())
}

fn load_config(path: Option<PathBuf>) -> Result<Config> {
    match path {
        Some(path) => Config::load_from_path(path),
        None => Config::load_from_default_path(),
    }
}

fn run_init() -> Result<()> {
    let home = home_dir()?;
    let config_path = config::default_config_path()?;
    let state_path = default_state_path(&home);
    let config_dir = parent_dir(&config_path)?;
    let state_dir = parent_dir(&state_path)?;

    create_dir_owner_only(config_dir)?;
    create_dir_owner_only(state_dir)?;

    let config_status = if config_path.exists() {
        // Repair perms before validation so a failure to parse the file does not
        // leave a permissive config on disk; matches the behavior of `acps status`.
        set_owner_only_file(&config_path)?;
        Config::load_from_path(&config_path)?;
        "validated existing config"
    } else {
        write_new_file_owner_only(&config_path, starter_config().as_bytes())?;
        Config::load_from_path(&config_path)?;
        "created starter config"
    };

    pre_create_owner_only(&state_path)?;
    let store = StateStore::open(&state_path)?;
    store.migrate()?;
    set_owner_only_file(&state_path)?;

    let config = Config::load_from_path(&config_path)?;
    let session_ref = config.auth.session_key_ref.clone();
    let admin_ref = config.auth.admin_key_ref.clone();
    let store_existed = secret_store_path(&home).exists();
    let mut secret_store = SecretStore::open_or_create(&home)?;
    let session_present = secret_store.contains(&session_ref);
    let admin_present = secret_store.contains(&admin_ref);
    let auth_status = if store_existed {
        // Pre-existing store: both refs must be present. Half-initialized state
        // (e.g. one ref deleted, or unrelated secrets but no auth refs) is an
        // anomaly. Refuse to proceed — admin key is not regenerable in place;
        // the documented recovery path is `acps reset --yes`.
        if !admin_present {
            return Err(StackError::MissingAdminKey { name: admin_ref });
        }
        if !session_present {
            return Err(StackError::MissingSessionKey { name: session_ref });
        }
        "preserved existing API keys"
    } else {
        // Fresh store: generate both keys. Print the values BEFORE the durable
        // event write, so a downstream failure in `append_event` cannot leave
        // the persisted-but-never-revealed admin key unrecoverable.
        let session_value = generate_api_key();
        let admin_value = generate_api_key();
        println!("---");
        println!("session key ({session_ref}): {session_value}");
        println!("admin key ({admin_ref}): {admin_value}");
        println!(
            "save the admin key now; it is never regenerable. use `acps reset --yes` to rotate it."
        );
        println!("---");
        // Write both refs in one atomic persist so a mid-init failure cannot
        // leave the store with one key set and the other missing, which the
        // fail-fast logic would then treat as a corrupted state requiring
        // reset.
        secret_store.set_many([
            (session_ref.as_str(), session_value.as_str()),
            (admin_ref.as_str(), admin_value.as_str()),
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
        "generated session and admin API keys"
    };

    // Record init.completed AFTER secret-store setup so a half-finished init
    // (e.g. failed key generation) does not leave a misleading
    // "initialized" event in the durable log.
    store.append_event_with_source(
        "info",
        "init.completed",
        crate::state::EVENT_SOURCE_CLI,
        "initialized",
        "{}",
    )?;

    println!("initialized acp-stack");
    println!("{config_status}: {}", config_path.display());
    println!("state: {}", state_path.display());
    println!("secrets: {}", secret_store.store_path().display());
    println!("age key: {}", age_key_path(&home).display());
    println!("auth: {auth_status}");

    Ok(())
}

fn run_status() -> Result<()> {
    let home = home_dir()?;
    let config_path = config::default_config_path()?;
    let config_dir = parent_dir(&config_path)?;
    if config_dir.exists() {
        set_owner_only_dir(config_dir)?;
    }
    if config_path.exists() {
        set_owner_only_file(&config_path)?;
    }
    Config::load_from_path(&config_path)?;

    let state_path = default_state_path(&home);
    let state_dir = parent_dir(&state_path)?;
    create_dir_owner_only(state_dir)?;
    pre_create_owner_only(&state_path)?;
    let store = StateStore::open(&state_path)?;
    store.migrate()?;
    set_owner_only_file(&state_path)?;
    store.append_event_with_source(
        "info",
        "status.checked",
        crate::state::EVENT_SOURCE_CLI,
        "status checked",
        "{}",
    )?;

    let schema_version = store.schema_version()?;
    let latest_event = store
        .latest_event_timestamp()?
        .unwrap_or_else(|| "none".to_owned());

    println!("config: ok ({})", config_path.display());
    println!("state: ok ({})", state_path.display());
    println!("schema_version: {schema_version}");
    println!("latest_event: {latest_event}");

    Ok(())
}

fn run_serve(args: ServeArgs) -> Result<()> {
    let home = home_dir()?;
    let config_path = config::default_config_path()?;
    let config_dir = parent_dir(&config_path)?;
    if config_dir.exists() {
        set_owner_only_dir(config_dir)?;
    }
    if config_path.exists() {
        set_owner_only_file(&config_path)?;
    }
    let config = Config::load_from_path(&config_path)?;

    let state_path = default_state_path(&home);
    let state_dir = parent_dir(&state_path)?;
    create_dir_owner_only(state_dir)?;
    pre_create_owner_only(&state_path)?;
    let store = StateStore::open(&state_path)?;
    store.migrate()?;
    set_owner_only_file(&state_path)?;

    // Reconcile any prompts left in-flight by a previous crash/restart.
    // The in-memory task registry is empty at startup, so a `pending` or
    // `running` row from before would never get a terminal status without
    // this sweep, leaving CLI/HTTP clients polling forever.
    let reconciled = store.reconcile_orphaned_prompts("daemon restart")?;
    if reconciled > 0 {
        tracing::info!(
            reconciled,
            "marked orphaned in-flight prompts as errored on startup"
        );
    }

    // Same sweep for the command gateway: a daemon restart kills any
    // mediated subprocesses via `kill_on_drop`, but their `commands` rows
    // are not finalized along the way. Mark them `failed` so polling
    // clients see them settle.
    let reconciled_commands = store.reconcile_orphaned_commands("daemon restart")?;
    if reconciled_commands > 0 {
        tracing::info!(
            reconciled = reconciled_commands,
            "marked orphaned in-flight commands as failed on startup"
        );
    }

    // Reconcile pending permission rows. ACP-source rows are canceled (the
    // request channel is gone after restart); command-source rows are
    // expired so the spec's "clients never see them stay pending forever"
    // promise holds across restart.
    let (perm_canceled, perm_expired) = store.reconcile_orphaned_permissions()?;
    if perm_canceled > 0 || perm_expired > 0 {
        tracing::info!(
            canceled = perm_canceled,
            expired = perm_expired,
            "settled orphaned permission requests on startup"
        );
    }

    let secret_store = SecretStore::open(&home)?;
    let session_ref = config.auth.session_key_ref.clone();
    let admin_ref = config.auth.admin_key_ref.clone();
    if !secret_store.contains(&session_ref) {
        return Err(StackError::MissingSessionKey { name: session_ref });
    }
    if !secret_store.contains(&admin_ref) {
        return Err(StackError::MissingAdminKey { name: admin_ref });
    }
    let session_key = secret_store.get(&session_ref)?.to_owned();
    let admin_key = secret_store.get(&admin_ref)?.to_owned();

    let bind = args.bind.unwrap_or_else(|| config.api.bind.clone());

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|source| StackError::ServeIo { source })?;

    runtime.block_on(async move {
        // Bind first, then record `server.starting`. Recording before bind
        // would leave a dangling start row whenever the address is already
        // in use; pairing the lifecycle write with a successful bind keeps
        // the durable trail truthful.
        let listener = tokio::net::TcpListener::bind(&bind)
            .await
            .map_err(|source| StackError::ServeBind {
                bind: bind.clone(),
                source,
            })?;
        let local = listener
            .local_addr()
            .map(|a| a.to_string())
            .unwrap_or_else(|_| bind.clone());
        let (socket_path, parent_policy) = match config.acpctl.socket_path.as_deref() {
            Some(path) => (
                std::path::PathBuf::from(path),
                crate::local_listener::ParentPolicy::ValidateOwnerOnly,
            ),
            None => (
                crate::local_listener::default_socket_path()?,
                crate::local_listener::ParentPolicy::RepairOwnerOnly,
            ),
        };
        // Bind the acpctl UDS *and* record `server.starting` only after every
        // listener is ready. A bind failure here is a startup-time error, not
        // a post-start regression — emitting `server.starting` first would
        // leave a dangling lifecycle row whenever the UDS bind fails.
        let bound_local = crate::local_listener::bind_local(&socket_path, parent_policy).await?;
        let lifecycle = ServerLifecycle::starting(&store, &local)?;
        let app_state =
            AppState::with_effective_bind(config, store, session_key, admin_key, local.clone());
        let state_handle = app_state.state.clone();
        let event_hub = app_state.event_hub.clone();
        lifecycle.started(&state_handle, &event_hub, &local).await?;
        eprintln!("acps serve: listening on {local}");
        eprintln!("acps serve: acpctl socket at {}", socket_path.display());
        let agent_supervisor = app_state.agent_supervisor.clone();
        // The acpctl UDS server runs alongside the TCP server. Both subscribe
        // to the same SIGTERM/SIGINT handler via `axum::serve.with_graceful_shutdown`,
        // so a single signal stops both. If the TCP serve exits first, the
        // local task is aborted; its `SocketGuard::drop` unlinks the socket.
        let local_handle = tokio::spawn(crate::local_listener::serve_local(
            app_state.clone(),
            bound_local,
        ));
        let serve_result = api::serve(app_state, listener).await;
        local_handle.abort();
        match local_handle.await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => tracing::warn!(error = %err, "acpctl local listener exited with error"),
            Err(join_err) if join_err.is_cancelled() => {}
            Err(join_err) => {
                tracing::warn!(error = %join_err, "acpctl local listener task panicked")
            }
        }
        // Tear down the agent BEFORE recording server.stopped so the
        // durable trail shows the agent went down first. A leaked agent
        // process outliving the daemon is a real bug, not a theoretical
        // one: see superflous-restart guarantees in security.md.
        agent_supervisor
            .shutdown_on_serve_exit(&state_handle, &event_hub)
            .await;
        let reason = match &serve_result {
            Ok(()) => "signal",
            Err(_) => "error",
        };
        // Always record stopped, even on error. Failures from the second
        // lifecycle write are logged but do not mask the original serve error.
        if let Err(err) = lifecycle.stopped(&state_handle, &event_hub, reason).await {
            tracing::error!(error = %err, "failed to record server.stopped");
        }
        eprintln!("acps serve: stopped ({reason})");
        serve_result
    })
}

/// Minimal ACP agent fixture used only by integration tests. Reads NDJSON
/// JSON-RPC from stdin and writes hardcoded responses to stdout. Stays alive
/// until stdin closes, at which point it exits cleanly (mirroring how a real
/// ACP agent terminates when the client drops the connection).
///
/// Recognizes:
/// - `initialize` -> protocolVersion = 1, agentCapabilities with
///   `loadSession` and `sessionCapabilities.{resume,close}` set unless the
///   matching `--no-cap-*` flag is supplied, agentInfo set.
/// - `session/new` -> deterministic `sess_fake_{counter}` id.
/// - `session/load`, `session/resume`, `session/close` -> empty ok.
/// - `session/prompt` -> emits two `session/update` notifications with agent
///   message chunks, then returns `{ stopReason: "end_turn" }`. With
///   `--prompt-cancel` it returns `{ stopReason: "cancelled" }` instead.
/// - any other request -> JSON-RPC method-not-found.
///
/// `session/cancel` notification flips the in-process flag so the next
/// `session/prompt` response uses `stopReason: cancelled`.
///
/// Compiled only into debug builds — release binaries do not expose this code
/// path.
#[cfg(debug_assertions)]
fn run_fake_agent(args: Vec<String>) -> Result<()> {
    use std::io::{BufRead, Write};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    let mut title = "acps fake agent".to_owned();
    let mut load_session_cap = true;
    let mut resume_session_cap = true;
    let mut close_session_cap = true;
    let mut prompt_emits_updates = true;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--assert-env-absent" => {
                if let Some(name) = iter.next() {
                    title = if std::env::var_os(name).is_some() {
                        format!("env leaked: {name}")
                    } else {
                        "env absent".to_owned()
                    };
                }
            }
            "--no-cap-load-session" => {
                load_session_cap = false;
            }
            "--no-cap-resume-session" => {
                resume_session_cap = false;
            }
            "--no-cap-close-session" => {
                close_session_cap = false;
            }
            "--prompt-silent" => {
                prompt_emits_updates = false;
            }
            _ => {}
        }
    }
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut stdin = stdin.lock();
    let stdout_handle = std::sync::Mutex::new(stdout.lock());
    let session_counter = AtomicU64::new(0);
    let cancel_requested = AtomicBool::new(false);
    let mut buf = String::new();
    loop {
        buf.clear();
        match stdin.read_line(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(source) => return Err(StackError::StdinRead { source }),
        }
        let trimmed = buf.trim_end_matches(['\n', '\r']);
        if trimmed.is_empty() {
            continue;
        }
        let value: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let id = value.get("id");
        let method = value.get("method").and_then(serde_json::Value::as_str);
        // Notifications have no id; act on the ones we recognize and ignore
        // anything else. `session/cancel` toggles the flag so the next
        // `session/prompt` response returns `cancelled`.
        let Some(id) = id else {
            if method == Some("session/cancel") {
                cancel_requested.store(true, Ordering::SeqCst);
            }
            continue;
        };
        let response = match method {
            Some("initialize") => {
                let mut agent_caps = serde_json::Map::new();
                agent_caps.insert(
                    "loadSession".to_owned(),
                    serde_json::Value::Bool(load_session_cap),
                );
                let mut session_caps = serde_json::Map::new();
                if resume_session_cap {
                    session_caps.insert("resume".to_owned(), serde_json::json!({}));
                }
                if close_session_cap {
                    session_caps.insert("close".to_owned(), serde_json::json!({}));
                }
                agent_caps.insert(
                    "sessionCapabilities".to_owned(),
                    serde_json::Value::Object(session_caps),
                );
                agent_caps.insert("promptCapabilities".to_owned(), serde_json::json!({}));
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": 1,
                        "agentCapabilities": agent_caps,
                        "agentInfo": {
                            "name": "acps-fake-agent",
                            "title": title,
                            "version": "0.0.1"
                        },
                        "authMethods": []
                    }
                })
            }
            Some("session/new") => {
                let counter = session_counter.fetch_add(1, Ordering::SeqCst);
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": { "sessionId": format!("sess_fake_{counter}") }
                })
            }
            Some("session/load") | Some("session/resume") | Some("session/close") => {
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {}
                })
            }
            Some("session/prompt") => {
                let session_id = value
                    .get("params")
                    .and_then(|p| p.get("sessionId"))
                    .and_then(|s| s.as_str())
                    .unwrap_or("sess_fake_unknown")
                    .to_owned();
                if prompt_emits_updates {
                    // Push two notifications before the response. The bridge's
                    // SessionEventSink persists them keyed by session_id; tests
                    // assert on the `events.session_id` column.
                    for text in ["chunk-1", "chunk-2"] {
                        let note = serde_json::json!({
                            "jsonrpc": "2.0",
                            "method": "session/update",
                            "params": {
                                "sessionId": session_id,
                                "update": {
                                    "sessionUpdate": "agent_message_chunk",
                                    "content": { "type": "text", "text": text }
                                }
                            }
                        });
                        let mut guard = stdout_handle
                            .lock()
                            .expect("fake-agent stdout mutex poisoned");
                        writeln!(*guard, "{note}")
                            .map_err(|source| StackError::ServeIo { source })?;
                        guard
                            .flush()
                            .map_err(|source| StackError::ServeIo { source })?;
                    }
                }
                let stop = if cancel_requested.swap(false, Ordering::SeqCst) {
                    "cancelled"
                } else {
                    "end_turn"
                };
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": { "stopReason": stop }
                })
            }
            _ => serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": -32601,
                    "message": "method not found"
                }
            }),
        };
        let mut guard = stdout_handle
            .lock()
            .expect("fake-agent stdout mutex poisoned");
        writeln!(*guard, "{response}").map_err(|source| StackError::ServeIo { source })?;
        guard
            .flush()
            .map_err(|source| StackError::ServeIo { source })?;
    }
}

fn run_agent_command(command: AgentCommand) -> Result<()> {
    match command {
        AgentCommand::Install => run_agent_install(),
        AgentCommand::Start => run_agent_daemon_post("/v1/agent/start", "start"),
        AgentCommand::Stop => run_agent_daemon_post("/v1/agent/stop", "stop"),
        AgentCommand::Status => run_agent_status(),
    }
}

fn run_agent_daemon_post(path: &'static str, label: &'static str) -> Result<()> {
    let home = home_dir()?;
    let config = Config::load_from_default_path()?;
    let store = SecretStore::open(&home)?;
    let admin_key = store.get(&config.auth.admin_key_ref)?.to_owned();
    let base_url = daemon_base_url(config.api.public_url.as_deref(), &config.api.bind)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|source| StackError::ServeIo { source })?;
    let body = runtime.block_on(post_agent_daemon(&base_url, path, &admin_key))?;
    if label == "start" {
        let pid = body["data"]["pid"]
            .as_u64()
            .map(|pid| pid.to_string())
            .unwrap_or_else(|| "unknown".to_owned());
        println!("agent start: running");
        println!("pid: {pid}");
    } else {
        let exit_status = body["data"]["exit_status"]
            .as_i64()
            .map(|status| status.to_string())
            .unwrap_or_else(|| "unknown".to_owned());
        println!("agent stop: stopped");
        println!("exit_status: {exit_status}");
    }
    Ok(())
}

async fn post_agent_daemon(
    base_url: &str,
    path: &'static str,
    admin_key: &str,
) -> Result<serde_json::Value> {
    let url = format!("{}{}", base_url.trim_end_matches('/'), path);
    let response = reqwest::Client::new()
        .post(url)
        .bearer_auth(admin_key)
        .send()
        .await
        .map_err(|source| StackError::AgentApiRequest { path, source })?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|source| StackError::AgentApiRequest { path, source })?;
    if !status.is_success() {
        return Err(StackError::AgentApiStatus { path, status, body });
    }
    serde_json::from_str(&body).map_err(|err| StackError::AgentInitializeFailed {
        reason: format!("agent API response was not JSON: {err}"),
    })
}

/// Tier of the API key used for a daemon-RPC call. Session-tier matches the
/// strict-tiering invariant: read/operate on session state with the session
/// key; never use the admin key for routine session operations. The single
/// variant is an enum so future admin-tier CLI helpers slot in without
/// reshaping the helper signatures.
#[derive(Debug, Clone, Copy)]
enum CliKey {
    Session,
}

#[derive(Debug, Clone, Copy)]
enum CliMethod {
    Get,
    Post,
    Delete,
}

/// Generalized daemon-RPC helper. Loads the configured key from the secret
/// store, builds the URL, dispatches, and parses the success envelope.
async fn daemon_request(
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
    if bare == "/v1/sessions" {
        "/v1/sessions"
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
fn encode_path_segment(segment: &str) -> String {
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

fn open_cli_key(config: &Config, home: &std::path::Path, key: CliKey) -> Result<String> {
    let store = SecretStore::open(home)?;
    let name = match key {
        CliKey::Session => &config.auth.session_key_ref,
    };
    Ok(store.get(name)?.to_owned())
}

fn daemon_base_url(public_url: Option<&str>, bind: &str) -> Result<String> {
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

fn run_agent_install() -> Result<()> {
    let home = home_dir()?;
    let config = Config::load_from_default_path()?;
    let install = config
        .agent
        .install
        .clone()
        .ok_or(StackError::AgentNotConfigured)?;
    let expected_sha256 = config.agent.expected_sha256.clone();

    let state_path = default_state_path(&home);
    let state_dir = parent_dir(&state_path)?;
    create_dir_owner_only(state_dir)?;
    pre_create_owner_only(&state_path)?;
    let store = StateStore::open(&state_path)?;
    store.migrate()?;
    set_owner_only_file(&state_path)?;

    let env = resolve_agent_env_for_cli(&home, &config)?;
    let workspace_root = std::path::PathBuf::from(config.workspace.root.clone());
    let outcome = run_installer(
        &install,
        expected_sha256.as_deref(),
        env,
        &workspace_root,
        &store,
    )?;

    println!("agent install: {}", outcome.label());
    println!("path: {}", outcome.path().display());
    println!("sha256: {}", outcome.sha256());
    Ok(())
}

fn resolve_agent_env_for_cli(
    home: &std::path::Path,
    config: &Config,
) -> Result<HashMap<String, String>> {
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

fn run_agent_status() -> Result<()> {
    let home = home_dir()?;
    let config = Config::load_from_default_path()?;
    let state_path = default_state_path(&home);
    let state_dir = parent_dir(&state_path)?;
    create_dir_owner_only(state_dir)?;
    pre_create_owner_only(&state_path)?;
    let store = StateStore::open(&state_path)?;
    store.migrate()?;
    set_owner_only_file(&state_path)?;

    println!("agent: {} ({})", config.agent.name, config.agent.id);
    println!("command: {}", config.agent.command);

    match store.latest_agent_capabilities(&config.agent.id)? {
        Some(record) => {
            println!("latest capabilities captured: {}", record.captured_at);
            println!("capabilities_json: {}", record.capabilities_json);
        }
        None => println!("latest capabilities: none recorded yet"),
    }

    let lifecycle = store.query_agent_lifecycle(10)?;
    if lifecycle.is_empty() {
        println!("recent lifecycle: (no rows)");
    } else {
        println!("recent lifecycle:");
        for event in lifecycle {
            println!(
                "  {} {} {}",
                event.created_at, event.event_kind, event.message
            );
        }
    }
    Ok(())
}

fn run_deps_command(command: DepsCommand) -> Result<()> {
    match command {
        DepsCommand::Check => {
            let config = Config::load_from_default_path()?;
            let report = crate::deps::check_dependencies(&config);
            if report.dependencies.is_empty() {
                println!("no dependencies declared in [dependencies]");
                return Ok(());
            }
            for entry in &report.dependencies {
                let status = if entry.available {
                    if let Some(path) = &entry.path {
                        format!("OK  {path}")
                    } else {
                        "OK".to_owned()
                    }
                } else {
                    let reason = entry.reason.as_deref().unwrap_or("unavailable");
                    format!("MISS {reason}")
                };
                let required = if entry.required { "*" } else { " " };
                println!(
                    "{required}{kind:<8} {name:<24} {status}",
                    kind = format!("{:?}", entry.kind).to_lowercase(),
                    name = entry.name,
                );
            }
            Ok(())
        }
    }
}

/// Minimal percent-encoder for query-string values. Encodes the small set of
/// characters that can appear in our metrics bounds (`:` and `+` from RFC3339,
/// `&` defensively). Anything outside the safe set turns into `%XX`.
fn encode_query_value(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        let safe = matches!(byte,
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~'
        );
        if safe {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn run_metrics_command(command: MetricsCommand) -> Result<()> {
    let home = home_dir()?;
    let config = Config::load_from_default_path()?;
    let session_key = open_cli_key(&config, &home, CliKey::Session)?;
    let base_url = daemon_base_url(config.api.public_url.as_deref(), &config.api.bind)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|source| StackError::ServeIo { source })?;
    runtime.block_on(async move {
        match command {
            MetricsCommand::Summary(args) => {
                let mut query = String::new();
                if let Some(since) = &args.since {
                    query.push_str(&format!("since={}", encode_query_value(since)));
                }
                if let Some(until) = &args.until {
                    if !query.is_empty() {
                        query.push('&');
                    }
                    query.push_str(&format!("until={}", encode_query_value(until)));
                }
                let path = if query.is_empty() {
                    "/v1/metrics/summary".to_owned()
                } else {
                    format!("/v1/metrics/summary?{query}")
                };
                let body =
                    daemon_request(&base_url, CliMethod::Get, &path, &session_key, None).await?;
                // Pretty-print: full JSON is sufficient for the operator; the
                // shape is documented and stable.
                if let Some(data) = body.get("data") {
                    let rendered =
                        serde_json::to_string_pretty(data).unwrap_or_else(|_| data.to_string());
                    println!("{rendered}");
                } else {
                    println!("{body}");
                }
                Ok(())
            }
        }
    })
}

fn run_sessions_command(command: SessionsCommand) -> Result<()> {
    let home = home_dir()?;
    let config = Config::load_from_default_path()?;
    let session_key = open_cli_key(&config, &home, CliKey::Session)?;
    let base_url = daemon_base_url(config.api.public_url.as_deref(), &config.api.bind)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|source| StackError::ServeIo { source })?;
    runtime.block_on(async move {
        match command {
            SessionsCommand::List(args) => {
                let path = format!("/v1/sessions?limit={}", args.limit);
                let body =
                    daemon_request(&base_url, CliMethod::Get, &path, &session_key, None).await?;
                let sessions = body["data"]["sessions"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                if sessions.is_empty() {
                    println!("(no sessions)");
                } else {
                    for session in sessions {
                        let id = session["id"].as_str().unwrap_or("?");
                        let status = session["status"].as_str().unwrap_or("?");
                        let cwd = session["cwd"].as_str().unwrap_or("");
                        let updated = session["updated_at"].as_str().unwrap_or("?");
                        println!("{updated} {status} {id} {cwd}");
                    }
                }
                Ok(())
            }
            SessionsCommand::New(args) => {
                let body = serde_json::json!({
                    "cwd": args.cwd,
                    "mcp_servers": [],
                });
                let response = daemon_request(
                    &base_url,
                    CliMethod::Post,
                    "/v1/sessions",
                    &session_key,
                    Some(&body),
                )
                .await?;
                let id = response["data"]["id"].as_str().unwrap_or("?");
                let cwd = response["data"]["cwd"].as_str().unwrap_or("");
                println!("session: {id}");
                if !cwd.is_empty() {
                    println!("cwd: {cwd}");
                }
                Ok(())
            }
            SessionsCommand::Prompt(args) => {
                run_sessions_prompt(&base_url, &session_key, args).await
            }
            SessionsCommand::Cancel(args) => {
                let encoded = encode_path_segment(&args.session_id);
                let path = format!("/v1/sessions/{encoded}/cancel");
                daemon_request(&base_url, CliMethod::Post, &path, &session_key, None).await?;
                println!("session cancel: requested");
                println!("session: {}", args.session_id);
                Ok(())
            }
            SessionsCommand::Close(args) => {
                let encoded = encode_path_segment(&args.session_id);
                let path = format!("/v1/sessions/{encoded}");
                let response =
                    daemon_request(&base_url, CliMethod::Delete, &path, &session_key, None).await?;
                let status = response["data"]["status"].as_str().unwrap_or("closed");
                println!("session close: {status}");
                println!("session: {}", args.session_id);
                Ok(())
            }
        }
    })
}

async fn run_sessions_prompt(
    base_url: &str,
    session_key: &str,
    args: SessionsPromptArgs,
) -> Result<()> {
    let prompt_text = match args.text {
        Some(text) => text,
        None => {
            let mut buffer = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin().lock(), &mut buffer)
                .map_err(|source| StackError::StdinRead { source })?;
            buffer
        }
    };
    let body = serde_json::json!({ "prompt": prompt_text });
    let encoded_session = encode_path_segment(&args.session_id);
    let path = format!("/v1/sessions/{encoded_session}/prompt");
    let response =
        daemon_request(base_url, CliMethod::Post, &path, session_key, Some(&body)).await?;
    let prompt_id = response["data"]["prompt_id"]
        .as_str()
        .ok_or_else(|| StackError::AgentInitializeFailed {
            reason: "daemon prompt response missing prompt_id".to_owned(),
        })?
        .to_owned();
    if args.no_wait {
        println!("prompt: pending");
        println!("prompt_id: {prompt_id}");
        return Ok(());
    }

    let encoded_prompt = encode_path_segment(&prompt_id);
    let status_path = format!("/v1/sessions/{encoded_session}/prompts/{encoded_prompt}");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(args.timeout_secs);
    let mut delay_ms: u64 = 250;
    loop {
        if std::time::Instant::now() > deadline {
            return Err(StackError::AgentInitializeFailed {
                reason: format!(
                    "prompt did not settle within {}s (prompt_id={})",
                    args.timeout_secs, prompt_id
                ),
            });
        }
        let poll =
            daemon_request(base_url, CliMethod::Get, &status_path, session_key, None).await?;
        let status = poll["data"]["status"].as_str().unwrap_or("");
        match status {
            "completed" => {
                let stop = poll["data"]["stop_reason"].as_str().unwrap_or("end_turn");
                println!("prompt: completed");
                println!("prompt_id: {prompt_id}");
                println!("stop_reason: {stop}");
                return Ok(());
            }
            "errored" => {
                let code = poll["data"]["error_code"].as_str().unwrap_or("agent.error");
                let message = poll["data"]["error_message"].as_str().unwrap_or("");
                return Err(StackError::AgentRequestFailed {
                    method: "session/prompt",
                    message: format!("{code}: {message}"),
                });
            }
            "cancelled" => {
                println!("prompt: cancelled");
                println!("prompt_id: {prompt_id}");
                return Ok(());
            }
            _ => {
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                // Linear back-off capped at 2s so a long-running prompt does
                // not spam the daemon every 250ms for minutes on end.
                delay_ms = (delay_ms + 250).min(2000);
            }
        }
    }
}

fn run_logs_command(command: LogsCommand) -> Result<()> {
    match command {
        LogsCommand::Query(args) => {
            let home = home_dir()?;
            let state_path = default_state_path(&home);
            let state_dir = parent_dir(&state_path)?;
            create_dir_owner_only(state_dir)?;
            pre_create_owner_only(&state_path)?;
            let store = StateStore::open(&state_path)?;
            store.migrate()?;
            set_owner_only_file(&state_path)?;

            let now = chrono::Utc::now();
            let since = resolve_time_bound(args.since.as_deref(), "since", now)?;
            let until = resolve_time_bound(args.until.as_deref(), "until", now)?;
            let (kind_exact, kind_prefix) = match args.kind.as_deref() {
                Some(k) if k.ends_with('.') => (None, Some(k)),
                Some(k) => (Some(k), None),
                None => (None, None),
            };
            let events = store.query_events(EventFilter {
                limit: args.limit,
                level: args.level.as_deref(),
                kind: kind_exact,
                kind_prefix,
                source: args.source.as_deref(),
                session_id: args.session.as_deref(),
                command_id: args.command.as_deref(),
                permission_id: args.permission.as_deref(),
                since: since.as_deref(),
                until: until.as_deref(),
                after_id: args.after.as_deref(),
            })?;

            for event in &events {
                println!(
                    "{} {} {} {} {}",
                    event.created_at, event.level, event.source, event.kind, event.message
                );
            }
            if (events.len() as u32) == args.limit {
                if let Some(last) = events.last() {
                    eprintln!(
                        "-- more rows available; pass --after {} to continue",
                        last.id
                    );
                }
            }

            Ok(())
        }
        LogsCommand::Tail(args) => run_logs_tail(args),
    }
}

/// Accept either a duration suffix (`30m`, `1h`, `2d`) or an RFC3339
/// timestamp. The suffix form resolves relative to `now`; the RFC3339 form is
/// returned verbatim after a parse round-trip to confirm it's well-formed.
fn resolve_time_bound(
    raw: Option<&str>,
    field: &'static str,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Option<String>> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(raw) {
        return Ok(Some(
            dt.with_timezone(&chrono::Utc)
                .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
        ));
    }
    let duration =
        crate::time_util::parse_duration_suffix(raw).ok_or_else(|| StackError::InvalidParam {
            field,
            reason: format!("not a valid RFC3339 timestamp or duration: {raw}"),
        })?;
    let resolved = now - duration;
    Ok(Some(
        resolved.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
    ))
}

fn run_logs_tail(args: LogsTailArgs) -> Result<()> {
    let home = home_dir()?;
    let config = Config::load_from_default_path()?;
    let session_key = open_cli_key(&config, &home, CliKey::Session)?;
    let base_url = daemon_base_url(config.api.public_url.as_deref(), &config.api.bind)?;
    let topics = if args.topics.is_empty() {
        vec!["logs".to_owned()]
    } else {
        args.topics
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|source| StackError::ServeIo { source })?;
    runtime.block_on(tail_ws_loop(&base_url, &session_key, topics))
}

async fn tail_ws_loop(base_url: &str, session_key: &str, topics: Vec<String>) -> Result<()> {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::protocol::Message;

    let ws_url = http_to_ws_url(base_url)?;
    let url = format!("{ws_url}/v1/ws");
    let mut request = url
        .as_str()
        .into_client_request()
        .map_err(|source| StackError::ServeIo {
            source: std::io::Error::other(format!("invalid websocket url: {source}")),
        })?;
    request.headers_mut().insert(
        http::header::AUTHORIZATION,
        format!("Bearer {session_key}")
            .parse()
            .map_err(|_| StackError::ServeIo {
                source: std::io::Error::other("session key produced invalid header value"),
            })?,
    );

    let (stream, _response) =
        tokio_tungstenite::connect_async(request)
            .await
            .map_err(|source| StackError::ServeIo {
                source: std::io::Error::other(format!("websocket connect failed: {source}")),
            })?;
    let (mut writer, mut reader) = stream.split();

    let subscribe = serde_json::json!({"type": "subscribe", "topics": topics});
    writer
        .send(Message::Text(subscribe.to_string().into()))
        .await
        .map_err(|source| StackError::ServeIo {
            source: std::io::Error::other(format!("subscribe failed: {source}")),
        })?;

    eprintln!(
        "acps logs tail: subscribed to {} at {url}",
        topics.join(", ")
    );

    let mut ctrl_c = std::pin::pin!(tokio::signal::ctrl_c());
    loop {
        tokio::select! {
            biased;
            _ = &mut ctrl_c => {
                let _ = writer.send(Message::Close(None)).await;
                return Ok(());
            }
            frame = reader.next() => {
                let Some(frame) = frame else { return Ok(()); };
                let message = frame.map_err(|source| StackError::ServeIo {
                    source: std::io::Error::other(format!("websocket read failed: {source}")),
                })?;
                match message {
                    Message::Text(text) => println!("{text}"),
                    Message::Binary(_) | Message::Ping(_) | Message::Pong(_) => {}
                    Message::Close(_) => return Ok(()),
                    Message::Frame(_) => {}
                }
            }
        }
    }
}

fn http_to_ws_url(base: &str) -> Result<String> {
    let trimmed = base.trim_end_matches('/');
    if let Some(rest) = trimmed.strip_prefix("https://") {
        return Ok(format!("wss://{rest}"));
    }
    if let Some(rest) = trimmed.strip_prefix("http://") {
        return Ok(format!("ws://{rest}"));
    }
    Err(StackError::ServeIo {
        source: std::io::Error::other("daemon base url must start with http:// or https://"),
    })
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

fn starter_config() -> &'static str {
    r#"[api]
bind = "127.0.0.1:7700"
public_url = "http://127.0.0.1:7700"
max_request_bytes = 104857600

[auth]
session_key_ref = "ACP_STACK_SESSION_KEY"
admin_key_ref = "ACP_STACK_ADMIN_KEY"

[security.http]
max_request_bytes = 104857600
rate_limit_per_minute = 120
burst = 30
auth_failures_per_minute = 5
auth_block_duration = "15m"
allowed_origins = []
trust_proxy_headers = false

[workspace]
root = "/workspace"
uploads = "/workspace/uploads"
default_shell = "/bin/bash"
runtime_user = "acp"
max_file_bytes = 8388608

[workspace.source]
type = "none"

[logging]
level = "info"
local_retention_days = 30

[logging.supabase]
enabled = false
url = "https://example.supabase.co"
service_role_key_ref = "SUPABASE_SERVICE_ROLE_KEY"
schema = "acp_stack"

[agent]
id = "placeholder"
name = "Placeholder Agent"
command = "acp-agent"
args = []
cwd = "/workspace"
env = []
restart = "never"

[agent.install]
type = "shell"
shell = "true"
creates = "acp-agent"
"#
}

#[cfg(test)]
mod tests {
    use super::{daemon_base_url, strip_ansi};

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
}
