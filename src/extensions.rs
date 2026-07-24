//! Typed, data-declared extension seams.
//!
//! An extension is declared by the operator in the `[extensions]` config table
//! and selected from the small set of types acp-stack defines
//! ([`crate::config::ExtensionType`]). Each type has a generic contract that
//! acp-stack supervises or serves without ever learning the extension's
//! semantics: there is no dynamic route registration and no in-process plugin
//! loading. The extension itself is whatever external software fulfills the
//! contract — an executable for `network-provider`, an API client for
//! `managed-state`.
//!
//! This module resolves declared config into the runtime representations the
//! seams consume; the per-type contracts live in the child modules.

pub mod managed_state;
pub mod network_provider;

use crate::config::Config;

pub use self::network_provider::NetworkProviderExtension;

/// Resolve the declared network-provider instance, if any. Config validation
/// guarantees at most one and that the sandbox backend is `unshare`.
pub fn resolve_network_provider(config: &Config) -> Option<NetworkProviderExtension> {
    config
        .extensions
        .iter()
        .find(|(_, extension)| {
            extension.extension_type == crate::config::ExtensionType::NetworkProvider
        })
        .map(|(name, extension)| NetworkProviderExtension::from_config(name, extension))
}

/// Resolve `name` to a declared managed-state instance. Unknown names and
/// type mismatches are indistinguishable to the caller by design: the fixed
/// route namespace only exists for declared managed-state extensions.
pub fn require_managed_state(config: &Config, name: &str) -> crate::error::Result<()> {
    let declared = config.extensions.get(name).is_some_and(|extension| {
        extension.extension_type == crate::config::ExtensionType::ManagedState
    });
    if declared {
        Ok(())
    } else {
        Err(crate::error::StackError::ExtensionNamespaceUnknown {
            name: name.to_owned(),
        })
    }
}
