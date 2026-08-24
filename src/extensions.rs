//! Typed, data-declared extension seams: resolves the `[extensions]` config table into
//! the runtime representations each seam consumes. There is no dynamic route
//! registration and no in-process plugin loading.

pub mod managed_state;
pub mod network_provider;

use crate::config::Config;

pub use self::network_provider::{NetworkProviderExtension, apply_workload_env};

/// Resolve the declared network-provider instance, if any. Config validation guarantees
/// at most one and that the sandbox backend is `unshare`.
pub fn resolve_network_provider(config: &Config) -> Option<NetworkProviderExtension> {
    config
        .extensions
        .iter()
        .find(|(_, extension)| {
            extension.extension_type == crate::config::ExtensionType::NetworkProvider
        })
        .map(|(name, extension)| NetworkProviderExtension::from_config(name, extension))
}

/// Resolve `name` to a declared managed-state instance. Unknown names and type
/// mismatches are indistinguishable to the caller by design.
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
