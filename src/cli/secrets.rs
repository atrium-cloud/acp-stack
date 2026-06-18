use crate::error::{Result, StackError};
use crate::fs_util::home_dir;
use crate::secrets::{SecretStore, reject_auth_ref_mutation};
use clap::{Args, Subcommand};
use std::io::BufRead as _;
use std::io::IsTerminal;

use super::core::{OutputFormat, print_json, resolve_admin_key, validate_local_admin_key};

#[derive(Debug, Subcommand)]
pub enum SecretsCommand {
    /// List secret reference names. Values are never printed.
    List,
    /// Store a named secret value.
    Set(SecretsSetArgs),
    /// Remove the named secret from the store.
    Delete(SecretsDeleteArgs),
}

#[derive(Debug, Args)]
pub struct SecretsSetArgs {
    /// Secret reference name. Can also be supplied with --name.
    name: Option<String>,
    /// Secret reference name.
    #[arg(long = "name", value_name = "NAME")]
    name_flag: Option<String>,
    /// Secret value. Prefer the interactive prompt or stdin when avoiding
    /// shell history and process-argument exposure matters.
    #[arg(long = "value", value_name = "VALUE")]
    value: Option<String>,
    /// Admin API key. If omitted on a TTY, prompts without echo.
    #[arg(long = "admin-key")]
    admin_key: Option<String>,
}

#[derive(Debug, Args)]
pub struct SecretsDeleteArgs {
    name: String,
    /// Admin API key. If omitted on a TTY, prompts without echo.
    #[arg(long = "admin-key")]
    admin_key: Option<String>,
}

pub(super) fn run_secrets_command(command: SecretsCommand, output: OutputFormat) -> Result<()> {
    let home = home_dir()?;
    match command {
        SecretsCommand::List => {
            let store = SecretStore::open(&home)?;
            let names = store.list_names();
            if output.is_json() {
                print_json(&serde_json::json!({ "secrets": names }))?;
            } else {
                for name in names {
                    println!("{name}");
                }
            }
            Ok(())
        }
        SecretsCommand::Set(args) => {
            let name = resolve_secret_name(args.name, args.name_flag)?;
            let stdin_is_terminal = std::io::stdin().is_terminal();
            let admin_key = resolve_admin_key(args.admin_key, stdin_is_terminal)?;
            validate_local_admin_key(&admin_key)?;
            reject_auth_ref_mutation(&name)?;
            let value = resolve_secret_value(args.value, &name, stdin_is_terminal)?;
            let mut store = SecretStore::open(&home)?;
            store.set(&name, &value)?;
            if output.is_json() {
                print_json(&serde_json::json!({ "action": "set", "name": name }))?;
            } else {
                println!("set secret: {name}");
            }
            Ok(())
        }
        SecretsCommand::Delete(args) => {
            let admin_key = resolve_admin_key(args.admin_key, std::io::stdin().is_terminal())?;
            validate_local_admin_key(&admin_key)?;
            reject_auth_ref_mutation(&args.name)?;
            let mut store = SecretStore::open(&home)?;
            store.delete(&args.name)?;
            if output.is_json() {
                print_json(&serde_json::json!({ "action": "delete", "name": args.name }))?;
            } else {
                println!("deleted secret: {}", args.name);
            }
            Ok(())
        }
    }
}

fn resolve_secret_name(positional: Option<String>, flag: Option<String>) -> Result<String> {
    match (positional, flag) {
        (Some(name), None) | (None, Some(name)) => Ok(name),
        (Some(_), Some(_)) => Err(StackError::InvalidParam {
            field: "--name",
            reason: "pass the secret name either positionally or with --name, not both".to_owned(),
        }),
        (None, None) => Err(StackError::MissingField {
            field: "<name> or --name",
        }),
    }
}

fn resolve_secret_value(
    value: Option<String>,
    name: &str,
    stdin_is_terminal: bool,
) -> Result<String> {
    match value {
        Some(value) => Ok(value),
        None => read_secret_value(name, stdin_is_terminal),
    }
}

fn read_secret_value(name: &str, stdin_is_terminal: bool) -> Result<String> {
    if stdin_is_terminal {
        return rpassword::prompt_password(format!("secret value for {name}: "))
            .map_err(|source| StackError::ServeIo { source });
    }

    // Read a single line from stdin; trailing CR/LF stripped. Values are
    // single-line text by spec, and script callers keep the existing pipe API.
    let mut buffer = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut buffer)
        .map_err(|source| StackError::StdinRead { source })?;
    Ok(buffer.trim_end_matches(['\n', '\r']).to_owned())
}
