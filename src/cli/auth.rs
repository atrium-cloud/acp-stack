use crate::config::Config;
use crate::error::{Result, StackError};
use clap::{Args, Subcommand};
use std::io::IsTerminal;

use super::core::{CliMethod, daemon_base_url, daemon_request, resolve_admin_key};

#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    /// Generate a new session key through the running daemon.
    /// The admin key is not regenerable; use `acps reset --yes` to rotate it.
    RegenerateSessionKey(AuthRegenerateSessionKeyArgs),
}

#[derive(Debug, Args)]
pub struct AuthRegenerateSessionKeyArgs {
    /// Admin API key. If omitted on a TTY, prompts without echo.
    #[arg(long = "admin-key")]
    admin_key: Option<String>,
}

pub(super) fn run_auth_command(command: AuthCommand) -> Result<()> {
    match command {
        AuthCommand::RegenerateSessionKey(args) => run_auth_regenerate_session_key(args),
    }
}

fn run_auth_regenerate_session_key(args: AuthRegenerateSessionKeyArgs) -> Result<()> {
    let config = Config::load_from_default_path()?;
    let admin_key = resolve_admin_key(args.admin_key, std::io::stdin().is_terminal())?;
    let base_url = daemon_base_url(config.api.public_url.as_deref(), &config.api.bind)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|source| StackError::ServeIo { source })?;
    let body = runtime.block_on(async {
        daemon_request(
            &base_url,
            CliMethod::Post,
            "/v1/auth/session-key/regenerate",
            &admin_key,
            None,
        )
        .await
    })?;
    let new_key =
        body["data"]["session_key"]
            .as_str()
            .ok_or_else(|| StackError::AgentInitializeFailed {
                reason: "daemon response missing session_key".to_owned(),
            })?;
    println!("session key rotated");
    println!("value: {new_key}");
    println!("update any clients with the new value; the previous key is now invalid");
    Ok(())
}
