use crate::config::Config;
use crate::error::{Result, StackError};
use crate::state::{EventFilter, StateStore, default_state_path};
use base64::Engine;
use clap::{Args, Parser, Subcommand};
use std::env;
use std::fs::Permissions;
use std::io::Write as _;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

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
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    Logs {
        #[command(subcommand)]
        command: LogsCommand,
    },
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    Validate(ConfigValidateArgs),
    Export(ConfigExportArgs),
}

#[derive(Debug, Subcommand)]
enum LogsCommand {
    Query(LogsQueryArgs),
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
        Command::Config { command } => run_config_command(command),
        Command::Logs { command } => run_logs_command(command),
    };

    if let Err(error) = &result {
        record_cli_error_message(&strip_ansi(&error.to_string()));
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
    }
}

fn load_config(path: Option<PathBuf>) -> Result<Config> {
    match path {
        Some(path) => Config::load_from_path(path),
        None => Config::load_from_default_path(),
    }
}

fn run_init() -> Result<()> {
    let home = home_dir()?;
    let config_path = crate::config::default_config_path()?;
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
    store.append_event("info", "init.completed", "initialized", "{}")?;

    println!("initialized acp-stack");
    println!("{config_status}: {}", config_path.display());
    println!("state: {}", state_path.display());

    Ok(())
}

fn run_status() -> Result<()> {
    let home = home_dir()?;
    let config_path = crate::config::default_config_path()?;
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

fn home_dir() -> Result<PathBuf> {
    let home = env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .ok_or(StackError::HomeNotSet)?;
    Ok(PathBuf::from(home))
}

fn parent_dir(path: &Path) -> Result<&Path> {
    path.parent().ok_or_else(|| StackError::MissingParentDir {
        path: path.to_path_buf(),
    })
}

fn create_dir_owner_only(path: &Path) -> Result<()> {
    if path.exists() {
        return set_owner_only_dir(path);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| StackError::DirectoryCreate {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(path)
            .map_err(|source| StackError::DirectoryCreate {
                path: path.to_path_buf(),
                source,
            })
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(path).map_err(|source| StackError::DirectoryCreate {
            path: path.to_path_buf(),
            source,
        })
    }
}

fn write_new_file_owner_only(path: &Path, content: &[u8]) -> Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    let mut file = opts.open(path).map_err(|source| StackError::FileCreate {
        path: path.to_path_buf(),
        source,
    })?;
    file.write_all(content)
        .map_err(|source| StackError::FileCreate {
            path: path.to_path_buf(),
            source,
        })
}

fn pre_create_owner_only(path: &Path) -> Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    match opts.open(path) {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            // File survived from a previous run or an older binary. Repair the mode
            // before any caller opens it, so writes never land while the file is
            // still group/world-readable.
            set_owner_only_file(path)
        }
        Err(source) => Err(StackError::FileCreate {
            path: path.to_path_buf(),
            source,
        }),
    }
}

#[cfg(unix)]
fn set_owner_only_dir(path: &Path) -> Result<()> {
    set_permissions(path, 0o700)
}

#[cfg(not(unix))]
fn set_owner_only_dir(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_owner_only_file(path: &Path) -> Result<()> {
    set_permissions(path, 0o600)
}

#[cfg(not(unix))]
fn set_owner_only_file(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_permissions(path: &Path, mode: u32) -> Result<()> {
    std::fs::set_permissions(path, Permissions::from_mode(mode)).map_err(|source| {
        StackError::PermissionSet {
            path: path.to_path_buf(),
            source,
        }
    })
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
api_key_ref = "SUPABASE_SECRET_KEY"
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
