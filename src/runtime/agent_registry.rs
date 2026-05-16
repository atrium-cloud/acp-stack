//! Hand-curated catalog of ACP-speaking agents and their adapters.
//!
//! The embedded `data/registry.toml` is the runtime source of truth for
//! `acps agent install`. It supersedes the upstream
//! `cdn.agentclientprotocol.com/registry/v1/latest/registry.json` so the
//! runtime can make conservative support claims. The embedded catalog starts
//! with OpenCode only, while the schema still supports future adapter-backed
//! entries that need both an ACP adapter and the upstream harness it wraps.
//!
//! Operators can override entries or add private ones by placing a
//! `~/.config/acp-stack/registry.toml` file alongside the main config.
//! Override semantics are full-entry-by-id: an override with the same `id`
//! replaces the embedded entry; new `id`s are added.

use std::fs;
use std::path::Path;

use serde::Deserialize;

use crate::error::{Result, StackError};

const EMBEDDED_REGISTRY: &str = include_str!("../../data/registry.toml");

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryCatalog {
    agents: Vec<RegistryEntry>,
}

impl RegistryCatalog {
    /// Parse the binary-embedded registry. Surfaced as a fallible call so
    /// the compile-time `include_str!` failure is the only way to ship an
    /// invalid registry; runtime parse failures bubble up as
    /// `StackError::RegistryLoad` for tests that swap in alternate TOML.
    pub fn load_embedded() -> Result<Self> {
        Self::from_toml(EMBEDDED_REGISTRY)
    }

    /// Load the embedded registry, then layer an operator override file on
    /// top if it exists at `override_path`. A missing override file is not
    /// an error — it is the common case for fresh installs.
    pub fn load_with_override(override_path: &Path) -> Result<Self> {
        let mut catalog = Self::load_embedded()?;
        if override_path.exists() {
            let body =
                fs::read_to_string(override_path).map_err(|source| StackError::RegistryLoad {
                    reason: format!(
                        "failed to read operator override {}: {source}",
                        override_path.display()
                    ),
                })?;
            let overlay = Self::from_toml(&body)?;
            catalog.merge(overlay);
        }
        Ok(catalog)
    }

    pub fn from_toml(body: &str) -> Result<Self> {
        let parsed: RegistryFile =
            toml::from_str(body).map_err(|source| StackError::RegistryLoad {
                reason: format!("registry TOML is invalid: {source}"),
            })?;
        let catalog = Self {
            agents: parsed.agents,
        };
        catalog.validate()?;
        Ok(catalog)
    }

    pub fn lookup(&self, id: &str) -> Option<&RegistryEntry> {
        self.agents.iter().find(|entry| entry.id == id)
    }

    pub fn entries(&self) -> &[RegistryEntry] {
        &self.agents
    }

    /// Full-entry replacement by id; new ids are appended. The override file
    /// is intentionally coarse: a partial-field merge would invite drift
    /// where an upstream rename silently kept an operator's stale harness.
    fn merge(&mut self, overlay: RegistryCatalog) {
        for entry in overlay.agents {
            match self.agents.iter().position(|e| e.id == entry.id) {
                Some(idx) => self.agents[idx] = entry,
                None => self.agents.push(entry),
            }
        }
    }

    fn validate(&self) -> Result<()> {
        for entry in &self.agents {
            match entry.kind {
                RegistryKind::Native => {
                    if entry.harness.is_some() {
                        return Err(StackError::RegistryLoad {
                            reason: format!(
                                "agent `{}` is kind=\"native\" but declares a [harness] block",
                                entry.id
                            ),
                        });
                    }
                }
                RegistryKind::Adapter => {
                    if entry.harness.is_none() {
                        return Err(StackError::RegistryLoad {
                            reason: format!(
                                "agent `{}` is kind=\"adapter\" but has no [harness] block",
                                entry.id
                            ),
                        });
                    }
                }
            }
            match (&entry.adapter_install, entry.headless_compatible) {
                (Some(install), _) => install.validate(&entry.id, "adapter_install")?,
                (None, true) => {
                    return Err(StackError::RegistryLoad {
                        reason: format!(
                            "agent `{}` is supported but has no [adapter_install] block",
                            entry.id
                        ),
                    });
                }
                (None, false) => {}
            }
            if let Some(harness) = &entry.harness {
                harness.install.validate(&entry.id, "harness.install")?;
            }
        }
        let mut seen = std::collections::HashSet::new();
        for entry in &self.agents {
            if !seen.insert(entry.id.as_str()) {
                return Err(StackError::RegistryLoad {
                    reason: format!("duplicate registry id `{}`", entry.id),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RegistryEntry {
    pub id: String,
    pub name: String,
    pub kind: RegistryKind,
    #[serde(default)]
    pub headless_compatible: bool,
    #[serde(default)]
    pub homepage: Option<String>,
    #[serde(default)]
    pub adapter_install: Option<InstallSpec>,
    #[serde(default)]
    pub harness: Option<HarnessSpec>,
}

impl RegistryEntry {
    pub fn ensure_supported(&self) -> Result<()> {
        if self.headless_compatible {
            Ok(())
        } else {
            Err(StackError::AgentUnsupported {
                name: self.name.clone(),
            })
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RegistryKind {
    Native,
    Adapter,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct HarnessSpec {
    pub id: String,
    #[serde(default)]
    pub source_url: Option<String>,
    pub install: InstallSpec,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum InstallSpec {
    Npx {
        package: String,
    },
    Uvx {
        package: String,
    },
    Shell {
        script: String,
        creates: String,
    },
    GithubRelease {
        repo: String,
        asset_pattern: String,
        archive: ArchiveKind,
        binary_name: String,
        #[serde(default)]
        checksums_asset: Option<String>,
    },
}

impl InstallSpec {
    fn validate(&self, agent_id: &str, field: &str) -> Result<()> {
        let nonempty = |label: &str, value: &str| -> Result<()> {
            if value.trim().is_empty() {
                Err(StackError::RegistryLoad {
                    reason: format!("agent `{agent_id}` {field}.{label} is empty"),
                })
            } else {
                Ok(())
            }
        };
        match self {
            InstallSpec::Npx { package } => nonempty("package", package),
            InstallSpec::Uvx { package } => nonempty("package", package),
            InstallSpec::Shell { script, creates } => {
                nonempty("script", script)?;
                nonempty("creates", creates)
            }
            InstallSpec::GithubRelease {
                repo,
                asset_pattern,
                binary_name,
                ..
            } => {
                nonempty("repo", repo)?;
                nonempty("asset_pattern", asset_pattern)?;
                nonempty("binary_name", binary_name)
            }
        }
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

#[derive(Debug, Deserialize)]
struct RegistryFile {
    #[serde(default)]
    agents: Vec<RegistryEntry>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_registry_parses() {
        let catalog = RegistryCatalog::load_embedded().expect("embedded registry must parse");
        assert!(
            !catalog.entries().is_empty(),
            "embedded registry must have at least one entry"
        );
    }

    #[test]
    fn lookup_returns_matching_entry() {
        let catalog = RegistryCatalog::load_embedded().expect("registry");
        let opencode = catalog
            .lookup("opencode")
            .expect("opencode must be present in the embedded registry");
        assert_eq!(opencode.kind, RegistryKind::Native);
        assert!(opencode.headless_compatible);
    }

    #[test]
    fn lookup_returns_none_for_unknown_id() {
        let catalog = RegistryCatalog::load_embedded().expect("registry");
        assert!(catalog.lookup("does-not-exist").is_none());
    }

    #[test]
    fn embedded_registry_only_advertises_opencode_headless_support() {
        let catalog = RegistryCatalog::load_embedded().expect("registry");
        let supported: Vec<_> = catalog
            .entries()
            .iter()
            .filter(|entry| entry.headless_compatible)
            .map(|entry| entry.id.as_str())
            .collect();
        assert_eq!(supported, ["opencode"]);
    }

    #[test]
    fn embedded_registry_is_intentionally_opencode_only() {
        let catalog = RegistryCatalog::load_embedded().expect("registry");
        let ids: Vec<_> = catalog
            .entries()
            .iter()
            .map(|entry| entry.id.as_str())
            .collect();
        assert_eq!(ids, ["opencode"]);
    }

    #[test]
    fn validate_rejects_adapter_without_harness() {
        let body = r#"
[[agents]]
id = "bad"
name = "Bad"
kind = "adapter"

[agents.adapter_install]
type = "npx"
package = "bad"
"#;
        let err =
            RegistryCatalog::from_toml(body).expect_err("must reject adapter without harness");
        match err {
            StackError::RegistryLoad { reason } => {
                assert!(reason.contains("kind=\"adapter\""), "reason: {reason}");
            }
            other => panic!("expected RegistryLoad, got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_native_with_harness() {
        let body = r#"
[[agents]]
id = "bad"
name = "Bad"
kind = "native"

[agents.adapter_install]
type = "npx"
package = "bad"

[agents.harness]
id = "should-not-be-here"

[agents.harness.install]
type = "npx"
package = "ignored"
"#;
        let err = RegistryCatalog::from_toml(body).expect_err("must reject native with harness");
        match err {
            StackError::RegistryLoad { reason } => {
                assert!(reason.contains("kind=\"native\""), "reason: {reason}");
            }
            other => panic!("expected RegistryLoad, got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_duplicate_ids() {
        let body = r#"
[[agents]]
id = "dup"
name = "First"
kind = "native"

[agents.adapter_install]
type = "npx"
package = "first"

[[agents]]
id = "dup"
name = "Second"
kind = "native"

[agents.adapter_install]
type = "npx"
package = "second"
"#;
        let err = RegistryCatalog::from_toml(body).expect_err("must reject duplicate ids");
        match err {
            StackError::RegistryLoad { reason } => {
                assert!(reason.contains("duplicate"), "reason: {reason}");
            }
            other => panic!("expected RegistryLoad, got {other:?}"),
        }
    }

    #[test]
    fn override_replaces_entry_by_id() {
        let base = RegistryCatalog::load_embedded().expect("registry");
        let overlay_body = r#"
[[agents]]
id = "opencode"
name = "OpenCode (private fork)"
kind = "native"

[agents.adapter_install]
type = "npx"
package = "@private/opencode"
"#;
        let overlay = RegistryCatalog::from_toml(overlay_body).expect("overlay parses");
        let mut catalog = base;
        catalog.merge(overlay);
        let entry = catalog.lookup("opencode").expect("entry exists");
        assert_eq!(entry.kind, RegistryKind::Native);
        assert_eq!(entry.name, "OpenCode (private fork)");
    }
}
