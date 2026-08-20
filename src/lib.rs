// `serde_json::json!` expands one recursion level per key, and the init resume
// record (`cli::init::resume`) enumerates every replayable flag in one literal;
// the default limit of 128 is not enough for it.
#![recursion_limit = "256"]

pub mod api;
pub mod auth;
pub mod cli;
pub mod config;
pub mod dev_gates;
pub mod edge;
pub mod envelope;
pub mod error;
pub mod events;
pub mod extensions;
pub mod fs_util;
pub mod http_hardening;
pub mod local_listener;
pub mod ownership;
pub mod runtime;
// Derives the published `/v1` JSON Schema contract from the wire DTOs. Exists
// only under `dev-tools`; the `generate-api-schema` bin is its only caller and
// nothing in the shipped binary references it.
#[cfg(feature = "dev-tools")]
pub mod schema_export;
pub mod secrets;
pub mod security;
pub mod state;
pub mod time_util;
pub mod tracing_init;
pub mod workspace;

pub use error::{Result, StackError};
