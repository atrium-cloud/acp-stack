//! Whether the binaries an agent install lays down are ones acp-stack put in place. A binary is
//! acp-stack's when it links into the managed bundles root, or when the same agent step recorded
//! it, by path and sha256, on a `ran` row. A match on a `kept` row marks a binary an operator
//! chose to keep instead of replacing.

use std::path::{Component, Path};

use crate::config::AgentConfig;
use crate::error::{Result, StackError};
use crate::runtime::agent::acp_bridge::resolve_command_path;
use crate::runtime::install::agent_installer::{
    InstalledArtifact, STEP_ADAPTER, STEP_HARNESS, STEP_INSTALL,
};
use crate::runtime::install::agent_registry::{
    InstallSet, RegistryEntry, RegistryKind, effective_registry_entry,
};
use crate::runtime::install::managed_bundles_dir;
use crate::state::{INSTALLER_STATUS_KEPT, INSTALLER_STATUS_RAN, StateStore};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComponentRole {
    /// The agent CLI itself.
    Harness,
    /// The ACP adapter an adapter-kind agent launches the CLI through.
    Adapter,
}

/// One binary an agent install lays down.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallComponent {
    pub role: ComponentRole,
    /// The `installer_runs.step` label the install records for this binary.
    pub step: &'static str,
    /// The name or path the command resolver looks up.
    pub command: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BinaryOwnership {
    /// Nothing resolves under the component's command.
    Absent,
    /// acp-stack installed the resolved binary.
    Installed(InstalledArtifact),
    /// An operator kept the resolved binary in place of an acp-stack install.
    Kept(InstalledArtifact),
    /// Something other than acp-stack put the resolved binary there.
    Foreign(InstalledArtifact),
}

/// The binaries installing `agent` lays down, in install order. A harness its adapter bundles has
/// no binary of its own.
pub fn install_components(
    agent: &AgentConfig,
    entry: &RegistryEntry,
) -> Result<Vec<InstallComponent>> {
    let entry = effective_registry_entry(entry, agent)?;
    let harness = entry
        .harness
        .as_ref()
        .ok_or_else(|| StackError::RegistryLoad {
            reason: format!("registry entry `{}` has no harness block", entry.id),
        })?;
    if entry.kind != RegistryKind::Adapter {
        return Ok(vec![InstallComponent {
            role: ComponentRole::Harness,
            step: STEP_INSTALL,
            command: component_command(&harness.install, &harness.id),
        }]);
    }
    let adapter = entry
        .adapter
        .as_ref()
        .ok_or_else(|| StackError::RegistryLoad {
            reason: format!("registry entry `{}` has no adapter block", entry.id),
        })?;
    let mut components = Vec::with_capacity(2);
    if !harness.install.is_provided_by_adapter() {
        components.push(InstallComponent {
            role: ComponentRole::Harness,
            step: STEP_HARNESS,
            command: component_command(&harness.install, &harness.id),
        });
    }
    components.push(InstallComponent {
        role: ComponentRole::Adapter,
        step: STEP_ADAPTER,
        command: component_command(&adapter.install, &adapter.id),
    });
    Ok(components)
}

fn component_command(install: &InstallSet, id: &str) -> String {
    install.created_binary_name().unwrap_or(id).to_owned()
}

/// Resolve `component` the way spawning does and trace the binary back to the step that put it
/// there. A relative command with a slash resolves against `workspace_root`, as the installer
/// resolves `creates`.
pub fn classify_component(
    store: &StateStore,
    agent_id: &str,
    component: &InstallComponent,
    workspace_root: &Path,
    home: &Path,
) -> Result<BinaryOwnership> {
    let Some(path) = resolve_command_path(&component.command, workspace_root, home) else {
        return Ok(BinaryOwnership::Absent);
    };
    let artifact = InstalledArtifact::of(&path)?;
    if links_into_managed_bundles(&path, home)? {
        return Ok(BinaryOwnership::Installed(artifact));
    }
    let recorded = store.latest_installer_run_for_artifact(
        agent_id,
        component.step,
        &path.display().to_string(),
        &artifact.sha256,
    )?;
    Ok(match recorded.as_ref().map(|row| row.status.as_str()) {
        Some(INSTALLER_STATUS_RAN) => BinaryOwnership::Installed(artifact),
        Some(INSTALLER_STATUS_KEPT) => BinaryOwnership::Kept(artifact),
        _ => BinaryOwnership::Foreign(artifact),
    })
}

/// A bundle install leaves `~/.local/bin/<name>` as a link into its versioned release, and
/// nothing else writes into the bundles root.
fn links_into_managed_bundles(path: &Path, home: &Path) -> Result<bool> {
    let inspect_failed = |source| StackError::AgentBinaryInspect {
        path: path.to_path_buf(),
        source,
    };
    let metadata = std::fs::symlink_metadata(path).map_err(inspect_failed)?;
    if !metadata.file_type().is_symlink() {
        return Ok(false);
    }
    let target = std::fs::read_link(path).map_err(inspect_failed)?;
    let target = match path.parent() {
        Some(parent) if target.is_relative() => parent.join(target),
        _ => target,
    };
    // `starts_with` is lexical, so `bundles/../elsewhere` would pass it. Bundle links are
    // absolute and never carry `..`.
    if target
        .components()
        .any(|component| component == Component::ParentDir)
    {
        return Ok(false);
    }
    Ok(target.starts_with(managed_bundles_dir(home)))
}

#[cfg(test)]
mod tests;
