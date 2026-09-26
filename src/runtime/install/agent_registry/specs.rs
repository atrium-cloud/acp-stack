//! Install and update specs mirroring the `[agents.harness]` /
//! `[agents.adapter]` blocks of `data/agents.toml`, validated at parse time so a
//! malformed catalog is rejected before any install runs.

use super::*;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessSpec {
    pub id: String,
    /// Arguments appended to the harness command to enter ACP stdio mode.
    #[serde(default = "default_acp_args")]
    pub acp_args: Vec<String>,
    pub install: InstallSet,
    #[serde(default)]
    pub update: UpdateSet,
}

/// The conventional ACP entry point: an `acp` subcommand.
pub fn default_acp_args() -> Vec<String> {
    vec!["acp".to_owned()]
}

impl HarnessSpec {
    pub(super) fn validate(&self, agent_id: &str, github: Option<&str>) -> Result<()> {
        validate_nonempty(agent_id, "harness.id", &self.id)?;
        if self.acp_args.is_empty() {
            return Err(StackError::RegistryLoad {
                reason: format!("agent `{agent_id}` harness.acp_args must not be empty"),
            });
        }
        for arg in &self.acp_args {
            validate_nonempty(agent_id, "harness.acp_args", arg)?;
        }
        self.install.validate(agent_id, "harness.install", github)?;
        if self.install.is_provided_by_adapter() && !self.update.is_empty() {
            return Err(StackError::RegistryLoad {
                reason: format!(
                    "agent `{agent_id}` harness.update cannot be set when harness.install is provided by adapter"
                ),
            });
        }
        if self.update.shell_rerun && self.install.shell.is_none() {
            return Err(StackError::RegistryLoad {
                reason: format!(
                    "agent `{agent_id}` harness.update.shell_rerun requires harness.install.shell"
                ),
            });
        }
        self.update.validate(agent_id, "harness.update")
    }
}

/// `_meta` dialect an adapter reads the breakpoint fork point from on
/// `session/fork`. Declared per adapter because no advertised ACP capability
/// names the JetBrains AIR extension, so the catalog is the only source.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default, schemars::JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum ForkPointDialect {
    /// `_meta.acpStack.messageId`, carrying acp-stack's own prompt message id.
    #[default]
    AcpStack,
    /// `_meta.jetbrains.air.fork`, carrying an id the adapter itself emitted as
    /// the `messageId` of a `session/update` chunk.
    JetbrainsAir,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdapterSpec {
    pub id: String,
    #[serde(default)]
    pub sync_id: Option<String>,
    #[serde(default)]
    pub github: Option<String>,
    /// Fork-point dialect this adapter honors. Adapters that implement the
    /// JetBrains AIR fork extension declare `jetbrains-air`.
    #[serde(default)]
    pub fork_point: ForkPointDialect,
    pub install: InstallSet,
    #[serde(default)]
    pub update: UpdateSet,
}

impl AdapterSpec {
    pub(super) fn validate(&self, agent_id: &str) -> Result<()> {
        validate_nonempty(agent_id, "adapter.id", &self.id)?;
        if let Some(sync_id) = &self.sync_id {
            validate_nonempty(agent_id, "adapter.sync_id", sync_id)?;
        }
        if let Some(github) = &self.github {
            github_url_from_value(agent_id, "adapter.github", github)?;
        }
        self.install
            .validate(agent_id, "adapter.install", self.github.as_deref())?;
        if self.update.shell_rerun && self.install.shell.is_none() {
            return Err(StackError::RegistryLoad {
                reason: format!(
                    "agent `{agent_id}` adapter.update.shell_rerun requires adapter.install.shell"
                ),
            });
        }
        self.update.validate(agent_id, "adapter.update")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallSet {
    #[serde(default)]
    pub provided_by: Option<InstallProvidedBy>,
    #[serde(default)]
    pub shell: Option<ShellInstall>,
    #[serde(default)]
    pub npm: Option<NpmInstall>,
    #[serde(default)]
    pub github: Option<GithubInstall>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InstallProvidedBy {
    Adapter,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateSet {
    #[serde(default)]
    pub apt: Option<AptUpdate>,
    /// Update by re-running the shell install recipe, for harnesses with no
    /// npm/github path and no native update subcommand.
    #[serde(default)]
    pub shell_rerun: bool,
}

impl UpdateSet {
    pub fn is_empty(&self) -> bool {
        self.apt.is_none() && !self.shell_rerun
    }

    fn validate(&self, agent_id: &str, field: &str) -> Result<()> {
        if let Some(apt) = &self.apt {
            apt.validate(agent_id, &format!("{field}.apt"))?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AptUpdate {
    pub package: String,
}

impl AptUpdate {
    fn validate(&self, agent_id: &str, field: &str) -> Result<()> {
        validate_nonempty(agent_id, &format!("{field}.package"), &self.package)
    }
}

impl InstallSet {
    pub fn is_empty(&self) -> bool {
        self.provided_by.is_none() && !self.has_install_paths()
    }

    pub fn is_provided_by_adapter(&self) -> bool {
        self.provided_by == Some(InstallProvidedBy::Adapter)
    }

    /// The binary name the install lanes leave on PATH. The lanes of one set
    /// agree in practice, so any of them names it; npm and github come first
    /// because a shell recipe's `creates` may be a path rather than a name.
    pub fn created_binary_name(&self) -> Option<&str> {
        self.npm
            .as_ref()
            .map(|npm| npm.creates.as_str())
            .or_else(|| {
                self.github
                    .as_ref()
                    .map(|github| github.binary_name.as_str())
            })
            .or_else(|| self.shell.as_ref().map(|shell| shell.creates.as_str()))
    }

    fn has_install_paths(&self) -> bool {
        self.shell.is_some() || self.npm.is_some() || self.github.is_some()
    }

    fn validate(&self, agent_id: &str, field: &str, github_url: Option<&str>) -> Result<()> {
        if let Some(provided_by) = self.provided_by {
            if self.has_install_paths() {
                return Err(StackError::RegistryLoad {
                    reason: format!(
                        "agent `{agent_id}` {field}.provided_by cannot be combined with shell, npm, or github install paths"
                    ),
                });
            }
            match provided_by {
                InstallProvidedBy::Adapter => {
                    if field != "harness.install" {
                        return Err(StackError::RegistryLoad {
                            reason: format!(
                                "agent `{agent_id}` {field}.provided_by = \"adapter\" is only valid for harness.install"
                            ),
                        });
                    }
                    return Ok(());
                }
            }
        }
        if !self.has_install_paths() {
            return Err(StackError::RegistryLoad {
                reason: format!(
                    "agent `{agent_id}` has no [{field}.shell|npm|github] path or {field}.provided_by"
                ),
            });
        }
        if let Some(shell) = &self.shell {
            shell.validate(agent_id, &format!("{field}.shell"))?;
        }
        if let Some(npm) = &self.npm {
            npm.validate(agent_id, &format!("{field}.npm"))?;
        }
        if let Some(github) = &self.github {
            if github_url.is_none_or(|value| value.trim().is_empty()) {
                return Err(StackError::RegistryLoad {
                    reason: format!("agent `{agent_id}` {field}.github requires github URL"),
                });
            }
            github.validate(agent_id, &format!("{field}.github"))?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShellInstall {
    pub script: String,
    pub creates: String,
    #[serde(default)]
    pub required_tools: Vec<String>,
    /// Whole-run budget for this recipe, overriding the installer default so a
    /// slow recipe does not force every hung install to wait as long.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

impl ShellInstall {
    fn validate(&self, agent_id: &str, field: &str) -> Result<()> {
        validate_nonempty(agent_id, &format!("{field}.script"), &self.script)?;
        validate_nonempty(agent_id, &format!("{field}.creates"), &self.creates)?;
        for tool in &self.required_tools {
            validate_required_tool(agent_id, &format!("{field}.required_tools"), tool)?;
        }
        // A zero budget would kill the recipe the instant it spawns, reading as
        // an install failure rather than a bad catalog.
        if self.timeout_secs == Some(0) {
            return Err(StackError::RegistryLoad {
                reason: format!(
                    "agent `{agent_id}` {field}.timeout_secs = 0; omit the field for the default budget, or set a positive value"
                ),
            });
        }
        // The cap keeps the `Instant::now() + timeout` deadline arithmetic in
        // `run_captured` from overflowing on a near-u64::MAX value.
        if let Some(timeout_secs) = self.timeout_secs
            && timeout_secs > crate::runtime::process_runner::MAX_INSTALL_TIMEOUT_SECS
        {
            return Err(StackError::RegistryLoad {
                reason: format!(
                    "agent `{agent_id}` {field}.timeout_secs = {timeout_secs} exceeds the {}-second (24 hour) cap",
                    crate::runtime::process_runner::MAX_INSTALL_TIMEOUT_SECS
                ),
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NpmInstall {
    pub package: String,
    pub creates: String,
}

impl NpmInstall {
    fn validate(&self, agent_id: &str, field: &str) -> Result<()> {
        validate_nonempty(agent_id, &format!("{field}.package"), &self.package)?;
        validate_nonempty(agent_id, &format!("{field}.creates"), &self.creates)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GithubInstall {
    pub asset_pattern: String,
    pub archive: ArchiveKind,
    #[serde(default)]
    pub archive_binary_name: Option<String>,
    /// Executable's path inside a directory-bundle archive. When set, the whole archive unpacks
    /// into a versioned release directory and `binary_name` in the bin directory links to it,
    /// because the executable resolves its bundled files relative to itself.
    #[serde(default)]
    pub bundle_binary_path: Option<String>,
    pub binary_name: String,
    #[serde(default)]
    pub checksums_asset: Option<String>,
    #[serde(default)]
    pub arch: ArchMap,
}

impl GithubInstall {
    fn validate(&self, agent_id: &str, field: &str) -> Result<()> {
        validate_nonempty(
            agent_id,
            &format!("{field}.asset_pattern"),
            &self.asset_pattern,
        )?;
        if let Some(archive_binary_name) = &self.archive_binary_name {
            validate_nonempty(
                agent_id,
                &format!("{field}.archive_binary_name"),
                archive_binary_name,
            )?;
        }
        validate_nonempty(agent_id, &format!("{field}.binary_name"), &self.binary_name)?;
        if let Some(bundle_binary_path) = &self.bundle_binary_path {
            self.validate_bundle(agent_id, field, bundle_binary_path)?;
        }
        if self.asset_pattern.contains("{arch}")
            || self
                .archive_binary_name
                .as_deref()
                .is_some_and(|name| name.contains("{arch}"))
        {
            self.arch.validate(agent_id, field)?;
        }
        Ok(())
    }

    fn validate_bundle(&self, agent_id: &str, field: &str, bundle_binary_path: &str) -> Result<()> {
        let invalid = |detail: &str| StackError::RegistryLoad {
            reason: format!("agent `{agent_id}` {field}.bundle_binary_path {detail}"),
        };
        if self.archive != ArchiveKind::TarGz {
            return Err(invalid("requires archive = \"tar.gz\""));
        }
        if self.archive_binary_name.is_some() {
            return Err(invalid("cannot be combined with archive_binary_name"));
        }
        if !is_relative_normal_path(bundle_binary_path) {
            return Err(invalid(
                "must be a non-empty relative path without `.` or `..` components",
            ));
        }
        // `binary_name` names the bundle's directory under the managed bundles root.
        if !is_relative_normal_path(&self.binary_name)
            || std::path::Path::new(&self.binary_name).components().count() != 1
        {
            return Err(StackError::RegistryLoad {
                reason: format!(
                    "agent `{agent_id}` {field}.binary_name must be a single path component in bundle mode"
                ),
            });
        }
        Ok(())
    }
}

/// Non-empty relative path made only of normal components, so joining it under a directory can
/// never escape that directory.
fn is_relative_normal_path(value: &str) -> bool {
    let path = std::path::Path::new(value);
    !value.trim().is_empty()
        && path
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchMap {
    #[serde(default)]
    pub x86_64: Option<String>,
    #[serde(default)]
    pub aarch64: Option<String>,
}

impl ArchMap {
    pub fn token_for_host(&self) -> Option<&str> {
        match std::env::consts::ARCH {
            "x86_64" => self.x86_64.as_deref(),
            "aarch64" => self.aarch64.as_deref(),
            _ => None,
        }
    }

    fn validate(&self, agent_id: &str, field: &str) -> Result<()> {
        let Some(x86_64) = self.x86_64.as_deref() else {
            return Err(StackError::RegistryLoad {
                reason: format!("agent `{agent_id}` {field}.arch.x86_64 is required"),
            });
        };
        validate_nonempty(agent_id, &format!("{field}.arch.x86_64"), x86_64)?;
        let Some(aarch64) = self.aarch64.as_deref() else {
            return Err(StackError::RegistryLoad {
                reason: format!("agent `{agent_id}` {field}.arch.aarch64 is required"),
            });
        };
        validate_nonempty(agent_id, &format!("{field}.arch.aarch64"), aarch64)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveKind {
    None,
    #[serde(rename = "tar.gz")]
    TarGz,
    Zip,
}

fn validate_nonempty(agent_id: &str, field: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        Err(StackError::RegistryLoad {
            reason: format!("agent `{agent_id}` {field} is empty"),
        })
    } else {
        Ok(())
    }
}

fn validate_required_tool(agent_id: &str, field: &str, value: &str) -> Result<()> {
    validate_nonempty(agent_id, field, value)?;
    if value.contains('/') {
        return Err(StackError::RegistryLoad {
            reason: format!("agent `{agent_id}` {field} entry `{value}` must be a command name"),
        });
    }
    Ok(())
}

pub fn github_repo_from_url(agent_id: &str, field: &str, url: &str) -> Result<String> {
    let rest = github_path_from_value(agent_id, field, url)?;
    let mut parts = rest.split('/').filter(|part| !part.is_empty());
    let owner = parts.next().ok_or_else(|| StackError::RegistryLoad {
        reason: format!("agent `{agent_id}` {field} has no owner"),
    })?;
    let repo = parts.next().ok_or_else(|| StackError::RegistryLoad {
        reason: format!("agent `{agent_id}` {field} has no repo"),
    })?;
    Ok(format!("{owner}/{repo}"))
}

pub fn github_url_from_value(agent_id: &str, field: &str, value: &str) -> Result<String> {
    let rest = github_path_from_value(agent_id, field, value)?;
    Ok(format!("https://github.com/{}", rest.trim_matches('/')))
}

fn github_path_from_value<'a>(agent_id: &str, field: &str, value: &'a str) -> Result<&'a str> {
    let value = value.trim();
    if value.is_empty() {
        return Err(StackError::RegistryLoad {
            reason: format!("agent `{agent_id}` {field} is empty"),
        });
    }
    if let Some(rest) = value.strip_prefix("https://github.com/") {
        return Ok(rest);
    }
    if value.starts_with("http://") || value.starts_with("https://") {
        return Err(StackError::RegistryLoad {
            reason: format!(
                "agent `{agent_id}` {field} must be a GitHub path or https://github.com/ URL"
            ),
        });
    }
    Ok(value)
}
