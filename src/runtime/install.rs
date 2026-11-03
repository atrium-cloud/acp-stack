pub mod agent_auto_update;
pub mod agent_installer;
pub mod agent_registry;
pub mod agent_updater;
pub mod agent_version_check;
pub mod github_release;
pub mod install_ownership;
pub mod npm_registry;
pub mod skill_installer;
pub mod skill_registry;
#[cfg(feature = "stack-self-update")]
pub mod stack_updater;

use std::path::{Path, PathBuf};

/// Canonical operator registry override path; when it exists it shadows the embedded catalog.
pub fn operator_registry_override(home: &Path) -> PathBuf {
    home.join(".config").join("acp-stack").join("agents.toml")
}

/// Canonical destination directory for managed agent binaries (`~/.local/bin`).
pub fn local_bin_dir(home: &Path) -> PathBuf {
    home.join(".local").join("bin")
}

/// Root of the unpacked directory-bundle releases that `~/.local/bin` links into. It must stay
/// outside `sandbox::sensitive_mask_paths`, or sandboxed agents would see an empty directory.
pub fn managed_bundles_dir(home: &Path) -> PathBuf {
    home.join(".local")
        .join("lib")
        .join("acp-stack")
        .join("bundles")
}
