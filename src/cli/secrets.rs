use crate::config::Config;
use crate::error::{Result, StackError};
use crate::fs_util::home_dir;
use crate::secrets::{SecretStore, reject_auth_ref_mutation};
use clap::{Args, Subcommand};
use std::io::BufRead as _;

use super::core::{OutputFormat, print_json};

#[derive(Debug, Subcommand)]
pub enum SecretsCommand {
    /// List secret reference names. Values are never printed.
    List,
    /// Read a single line from stdin and store it as the named secret.
    Set(SecretsSetArgs),
    /// Remove the named secret from the store.
    Delete(SecretsDeleteArgs),
}

#[derive(Debug, Args)]
pub struct SecretsSetArgs {
    name: String,
}

#[derive(Debug, Args)]
pub struct SecretsDeleteArgs {
    name: String,
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
            if output.is_json() {
                print_json(&serde_json::json!({ "action": "set", "name": args.name }))?;
            } else {
                println!("set secret: {}", args.name);
            }
            Ok(())
        }
        SecretsCommand::Delete(args) => {
            let config = Config::load_from_default_path()?;
            reject_auth_ref_mutation(&args.name, &config)?;
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
