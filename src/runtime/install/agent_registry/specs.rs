//! Install and update specs carried by a registry entry.
//!
//! These types mirror the `[agents.harness]` / `[agents.adapter]` blocks of
//! `data/agents.toml` and own their own validation, so a malformed catalog is
//! rejected at parse time rather than at install time. The catalog in the
//! parent module drives this by calling into [`HarnessSpec::validate`] and
//! [`AdapterSpec::validate`].

use super::*;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessSpec {
    pub id: String,
    pub install: InstallSet,
    #[serde(default)]
    pub update: UpdateSet,
}

impl HarnessSpec {
    pub(super) fn validate(&self, agent_id: &str, github: Option<&str>) -> Result<()> {
        validate_nonempty(agent_id, "harness.id", &self.id)?;
        self.install.validate(agent_id, "harness.install", github)?;
        if self.install.is_provided_by_adapter() && !self.update.is_empty() {
            return Err(StackError::RegistryLoad {
                reason: format!(
                    "agent `{agent_id}` harness.update cannot be set when harness.install is provided by adapter"
                ),
            });
        }
        self.update.validate(agent_id, "harness.update")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdapterSpec {
    pub id: String,
    #[serde(default)]
    pub sync_id: Option<String>,
    #[serde(default)]
    pub github: Option<String>,
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
}

impl UpdateSet {
    pub fn is_empty(&self) -> bool {
        self.apt.is_none()
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
}

impl ShellInstall {
    fn validate(&self, agent_id: &str, field: &str) -> Result<()> {
        validate_nonempty(agent_id, &format!("{field}.script"), &self.script)?;
        validate_nonempty(agent_id, &format!("{field}.creates"), &self.creates)?;
        for tool in &self.required_tools {
            validate_required_tool(agent_id, &format!("{field}.required_tools"), tool)?;
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
