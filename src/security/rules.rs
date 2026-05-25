//! Sub-modules for individual security rules. Each rule exposes a
//! `pub(super) fn check_<name>(inputs, findings)` entry point; the
//! orchestrator in `crate::security::check` calls them in the same order
//! the original linear implementation emitted findings, preserving the
//! finding sequence the test suite asserts.

mod bind;
mod cloudflare;
mod cors;
mod keys;
mod paths;
mod proxy;
mod runtime_user;

pub(super) use bind::check_bind;
pub(super) use cloudflare::check_cloudflare;
pub(super) use cors::check_cors;
pub(super) use keys::check_keys;
pub(super) use paths::check_paths;
pub(super) use proxy::check_proxy;
pub(super) use runtime_user::check_runtime_user;
