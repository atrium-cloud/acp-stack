mod agent;
mod array;
mod auth;
mod config;
mod core;
mod deps;
mod deps_apply_worker;
mod extensions;
mod init;
mod installer;
mod logging;
mod logs;

// Final hop of the init schema re-export chain: exposes the def-producers at
// `crate::cli` for `schema_export`. Dev-tools only.
#[cfg(feature = "dev-tools")]
pub(crate) use self::init::{init_request_defs, init_response_defs};
mod metrics;
mod reset;
mod secrets;
mod security;
mod serve;
mod sessions;
mod skill;
mod status;
mod subagent;
#[cfg(feature = "stack-self-update")]
mod update;
mod workspace;
mod ws;

pub use core::{Cli, run};
