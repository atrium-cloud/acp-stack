pub mod agent_auto_update;
pub mod agent_installer;
pub mod agent_registry;
pub mod agent_updater;
pub mod agent_version_check;
pub mod github_release;
pub mod npm_registry;
pub mod skill_installer;
pub mod skill_registry;
#[cfg(feature = "stack-self-update")]
pub mod stack_updater;

use std::path::{Path, PathBuf};

/// Canonical operator registry override path (`~/.config/acp-stack/agents.toml`).
/// When the file exists it shadows the embedded catalog. Centralized here so the
/// CLI, the API, and the daemon auto-updater all resolve the same location.
pub fn operator_registry_override(home: &Path) -> PathBuf {
    home.join(".config").join("acp-stack").join("agents.toml")
}

/// Canonical destination directory for managed agent binaries (`~/.local/bin`).
pub fn local_bin_dir(home: &Path) -> PathBuf {
    home.join(".local").join("bin")
}

/// Whether `agent_id`'s registry entry declares a per-provider endpoint field
/// acp-stack can write. Resolves the catalog the same way every other runtime
/// caller does — embedded plus the operator override — so an operator entry
/// that adds the capability is honored.
pub fn agent_supports_provider_base_url(agent_id: &str) -> crate::error::Result<bool> {
    let home = crate::fs_util::home_dir()?;
    Ok(
        agent_registry::RegistryCatalog::load_with_override(&operator_registry_override(&home))?
            .supports_provider_base_url(agent_id),
    )
}
