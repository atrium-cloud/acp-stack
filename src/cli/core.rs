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

use super::agent::{AgentCommand, AgentRestartArgs};
use super::array::ArrayCommand;
use super::auth::AuthCommand;
use super::config::ConfigCommand;
use super::deps::DepsCommand;
use super::extensions::ExtensionsCommand;
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
use super::skill::SkillCommand;
use super::subagent::SubagentCommand;
#[cfg(feature = "stack-self-update")]
use super::update::UpdateCommand;
use super::workspace::WorkspaceCommand;
use super::ws::WsCommand;

mod auth;
mod command;
mod dispatch;
mod http;
mod output;
mod request;

pub use self::command::Cli;
pub use self::dispatch::run;

pub(super) use self::auth::*;
pub(super) use self::command::*;
pub(super) use self::http::*;
pub(super) use self::output::*;
pub(super) use self::request::*;

#[cfg(test)]
mod tests;
