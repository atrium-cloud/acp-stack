use crate::config::Config;
use crate::error::{Result, StackError};
use crate::fs_util::home_dir;
use crate::secrets::{SecretStore, reject_auth_ref_mutation};
use clap::{Args, Subcommand};
use std::io::BufRead as _;

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

pub(super) fn run_secrets_command(command: SecretsCommand) -> Result<()> {
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
