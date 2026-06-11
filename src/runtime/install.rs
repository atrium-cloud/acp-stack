pub mod agent_auto_update;
pub mod agent_installer;
pub mod agent_registry;
pub mod agent_updater;
pub mod github_release;
pub mod npm_registry;
pub mod skill_installer;
pub mod skill_registry;

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
