//! `acpctl` — local agent CLI for `acp-stack`.
//!
//! Speaks HTTP/1.1 over a Unix-domain socket against the daemon's local
//! listener. Maps each subcommand to one of the allowlisted local routes.

mod app;
mod cli_defs;
mod client;
mod formatters;
mod helpers;

use std::process::ExitCode;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    app::run_cli().await
}
