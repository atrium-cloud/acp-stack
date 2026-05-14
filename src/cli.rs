use crate::api::{self, AppState};
use crate::auth::generate_api_key;
use crate::config::{self, Config};
use crate::error::{Result, StackError};
use crate::fs_util::{
    atomic_write_owner_only, create_dir_owner_only, home_dir, parent_dir, pre_create_owner_only,
    set_owner_only_dir, set_owner_only_file, write_new_file_owner_only,
};
use crate::secrets::{SecretStore, age_key_path, secret_store_path};
use crate::state::{EventFilter, StateStore, default_state_path};
use crate::supervisor::ServerLifecycle;
use base64::Engine;
use clap::{Args, Parser, Subcommand};
use std::io::BufRead as _;
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
}

#[derive(Debug, Args)]
struct LogsQueryArgs {
    #[arg(long, default_value_t = 50)]
    limit: u32,
    #[arg(long)]
    level: Option<String>,
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
    let result = match cli.command {
        Command::Init => run_init(),
        Command::Status => run_status(),
        Command::Reset(args) => run_reset(args),
        Command::Serve(args) => run_serve(args),
        Command::Auth { command } => run_auth_command(command),
        Command::Secrets { command } => run_secrets_command(command),
        Command::Config { command } => run_config_command(command),
        Command::Logs { command } => run_logs_command(command),
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
        if current.auth.session_key_ref != config.auth.session_key_ref {
            return Err(StackError::ImportChangesAuthRef {
                field: "session_key_ref",
                current: current.auth.session_key_ref,
                incoming: config.auth.session_key_ref,
            });
        }
        if current.auth.admin_key_ref != config.auth.admin_key_ref {
            return Err(StackError::ImportChangesAuthRef {
                field: "admin_key_ref",
                current: current.auth.admin_key_ref,
                incoming: config.auth.admin_key_ref,
            });
        }
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

/// `acps secrets set/delete` must not touch the configured auth refs.
/// Direct manipulation would bypass the regenerate-session-key flow and the
/// "admin key never regenerable in place" invariant.
fn reject_auth_ref_mutation(name: &str, config: &Config) -> Result<()> {
    if name == config.auth.session_key_ref {
        return Err(StackError::SecretReservedForAuth {
            name: name.to_owned(),
            kind: "session",
        });
    }
    if name == config.auth.admin_key_ref {
        return Err(StackError::SecretReservedForAuth {
            name: name.to_owned(),
            kind: "admin",
        });
    }
    Ok(())
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
        store.append_event(
            "info",
            "auth.keys_generated",
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
    store.append_event("info", "init.completed", "initialized", "{}")?;

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
    store.append_event("info", "status.checked", "status checked", "{}")?;

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
        let lifecycle = ServerLifecycle::starting(&store, &local)?;
        let app_state =
            AppState::with_effective_bind(config, store, session_key, admin_key, local.clone());
        let state_handle = app_state.state.clone();
        lifecycle.started(&state_handle, &local).await?;
        eprintln!("acps serve: listening on {local}");
        let serve_result = api::serve(app_state, listener).await;
        let reason = match &serve_result {
            Ok(()) => "signal",
            Err(_) => "error",
        };
        // Always record stopped, even on error. Failures from the second
        // lifecycle write are logged but do not mask the original serve error.
        if let Err(err) = lifecycle.stopped(&state_handle, reason).await {
            tracing::error!(error = %err, "failed to record server.stopped");
        }
        eprintln!("acps serve: stopped ({reason})");
        serve_result
    })
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
            let events = store.query_events(EventFilter {
                limit: args.limit,
                level: args.level.as_deref(),
            })?;

            for event in events {
                println!(
                    "{} {} {} {}",
                    event.created_at, event.level, event.kind, event.message
                );
            }

            Ok(())
        }
    }
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
    if let Err(log_error) = store.append_event("error", "cli.error", "command failed", &payload) {
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
    use super::strip_ansi;

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
}
