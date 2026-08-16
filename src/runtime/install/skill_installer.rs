//! Agent Skills installer used by `acps init`.

mod discover;
mod fs;
mod install;
mod link;
mod port;
mod remove;
mod source;
mod types;
mod validate;

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::{DEFAULT_SKILL_SOURCE_BRANCH, UserSkillSource};
use crate::error::{Result, StackError};
use crate::fs_util::{create_dir_owner_only, set_owner_only_dir, set_owner_only_file};
use crate::runtime::install::agent_registry::{RegistryCatalog, RegistryEntry};
use crate::runtime::install::skill_registry::{
    CatalogSkill, SkillCatalog, SkillDirectory, SkillSource,
};
use crate::runtime::workspace_sources::safe_download::{DownloadOpts, download_to_file};
use crate::runtime::workspace_sources::safe_extract::{ExtractOpts, extract_archive};

pub use self::discover::*;
use self::fs::*;
pub use self::install::*;
pub use self::link::*;
pub use self::port::*;
pub use self::remove::*;
pub use self::source::*;
pub use self::types::*;
pub(crate) use self::validate::validate_install_target_name;
use self::validate::{
    validate_github_owner, validate_registry_relative_path, validate_skill_name,
    validate_skill_selector,
};

pub const SOURCE_CUSTOM_GITHUB_PREFIX: &str = "github:";
const CUSTOM_SKILLS_REPO: &str = "skills";
const CUSTOM_SKILLS_DIRECTORY: &str = "skills";
const SKILL_DESCRIPTOR: &str = "SKILL.md";
/// Marker file inside each acp-stack-installed skill dir proving the runtime
/// manages it. `remove` and switch-port overwrite refuse unmarked dirs, so
/// skills a user placed in the install root by hand are never deleted. The
/// content is the id of the source the skill was installed from.
pub(crate) const MANAGED_SKILL_MARKER: &str = ".acp-stack-managed";
const GITHUB_ARCHIVE_MAX_BYTES: u64 = 200 * 1024 * 1024;

/// The installer's dominant failure shape: an operation, the path it was
/// attempted on, and the underlying error. Kept as one helper so every
/// filesystem failure in this module reads identically in logs and API
/// responses.
fn skill_io_err(verb: &str, path: &Path, source: impl std::fmt::Display) -> StackError {
    StackError::SkillInstallFailed {
        reason: format!("{verb} `{}`: {source}", path.display()),
    }
}

pub fn expand_agent_skills_install_dir(home: &Path, value: &str) -> Result<PathBuf> {
    if value == "~" {
        return Ok(home.to_path_buf());
    }
    if let Some(rest) = value.strip_prefix("~/") {
        return Ok(home.join(rest));
    }
    let path = Path::new(value);
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    Err(StackError::SkillInstallFailed {
        reason: format!("agent skill install dir `{value}` must be absolute or start with `~/`"),
    })
}

fn agent_skill_root(home: &Path, entry: &RegistryEntry) -> Result<Option<PathBuf>> {
    if !entry.supports_agent_skills {
        return Ok(None);
    }
    let Some(install_dir) = entry.agent_skills_install_dir.as_deref() else {
        return Ok(None);
    };
    expand_agent_skills_install_dir(home, install_dir).map(Some)
}

#[cfg(test)]
mod tests;
