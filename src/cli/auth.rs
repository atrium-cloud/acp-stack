use crate::auth::generate_api_key;
use crate::config::Config;
use crate::error::{Result, StackError};
use crate::fs_util::home_dir;
use crate::secrets::SecretStore;
use clap::Subcommand;

#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    /// Generate a new session key and store it in the encrypted secret store.
    /// The admin key is not regenerable; use `acps reset --yes` to rotate it.
    RegenerateSessionKey,
}

pub(super) fn run_auth_command(command: AuthCommand) -> Result<()> {
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
