//! TOML schema types for `acps-config.toml`.
//!
//! The nested config structs are grouped into the domain submodules below and
//! glob re-exported here, so both `crate::config::X` and
//! `crate::config::schema::X` resolve exactly as they did before the split.
//! The root `Config` aggregator and the `RawConfig` deserialization shim stay
//! in `src/config.rs` because they own load-time orchestration. Default impls
//! and small `default_*` helper functions used by `#[serde(default = "...")]`
//! annotations are co-located with the struct they belong to so each domain
//! module is self-contained.

mod agent;
mod deps;
mod edge;
mod logging;
mod mcp;
mod runtime;
mod sandbox;
mod skills;
mod sources;
mod updates;

use crate::error::StackError;
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub use self::agent::*;
pub use self::deps::*;
pub use self::edge::*;
pub use self::logging::*;
pub use self::mcp::*;
pub use self::runtime::*;
pub use self::sandbox::*;
pub use self::skills::*;
pub use self::sources::*;
pub use self::updates::*;

// CONSTANTS

/// Fallback custom-model limits used when an operator does not provide agent
/// config values. They match the documented defaults for the custom provider
/// setup flow and keep the literals centralized across CLI and init paths.
pub const DEFAULT_CUSTOM_MODEL_CONTEXT: u64 = 200_000;
pub const DEFAULT_CUSTOM_MODEL_OUTPUT_MAX_TOKENS: u64 = 65_536;

pub const DEFAULT_PERMISSION_REQUEST_TIMEOUT: &str = "5m";
pub const DEFAULT_PERMISSION_TIMEOUT_ACTION: PermissionTimeoutAction =
    PermissionTimeoutAction::Deny;
pub const DEFAULT_AGENT_AUTO_UPDATE_FREQUENCY: &str = "1d";
pub const DEFAULT_STACK_UPDATE_FREQUENCY: &str = "1d";
pub const DEFAULT_STACK_UPDATE_POLICY: StackUpdatePolicy = StackUpdatePolicy::SecurityCritical;
