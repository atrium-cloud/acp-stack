//! Installed-vs-upstream version comparison for managed registry agents.
//! Shared by `acps agent check` and the `GET /v1/agent/update/status` route.

use serde::Serialize;

use crate::error::Result;
use crate::runtime::install::agent_installer::{STEP_ADAPTER, STEP_HARNESS, STEP_INSTALL};
use crate::runtime::install::agent_registry::{RegistryCatalog, RegistryEntry, RegistryKind};
use crate::state::InstallerRun;

/// Result of comparing the installed managed-agent version against upstream.
/// Carried as a typed enum so the CLI printer, the API route, and test cases
/// can pattern-match the four states deterministically.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum AgentVersionStatus {
    /// Installed and upstream agree on a non-empty version.
    UpToDate { version: String },
    /// Both versions are known and they differ — operator should re-run install.
    Stale { installed: String, latest: String },
    /// We could not derive an upstream version (shell-recipe install, missing
    /// registry kind, or upstream API error captured as a fall-through).
    Unknown { reason: String },
    /// No successful installer row for this step yet.
    NotInstalled,
}

/// Sources of "latest version" used by the version check. Trait-based so unit
/// tests can substitute a deterministic mock; the production runtime injects
/// `LiveLatestVersionResolver` which actually hits npm and GitHub.
pub trait LatestVersionResolver {
    fn npm(&self, package: &str) -> Result<String>;
    fn github(&self, repo: &str) -> Result<String>;
}

pub struct LiveLatestVersionResolver;

impl LatestVersionResolver for LiveLatestVersionResolver {
    fn npm(&self, package: &str) -> Result<String> {
        crate::runtime::install::npm_registry::latest_version(package)
    }
    fn github(&self, repo: &str) -> Result<String> {
        crate::runtime::install::github_release::latest_release_tag(repo)
    }
}

/// Resolve the registry-declared upstream version for the given step. Returns
/// `Ok(Some)` when the registry entry pins this step to a known source
/// (npm package, GitHub release), `Ok(None)` when the install kind has no
/// queryable upstream (shell recipes), and `Err` when the upstream lookup
/// itself fails. Caller decides how to surface each variant in the report.
fn resolve_upstream_version_for_step(
    entry: &RegistryEntry,
    step: &str,
    resolver: &dyn LatestVersionResolver,
) -> Result<Option<String>> {
    let (install, shell_rerun) = match step {
        STEP_HARNESS | STEP_INSTALL => match entry.harness.as_ref() {
            Some(harness) => (&harness.install, harness.update.shell_rerun),
            None => return Ok(None),
        },
        STEP_ADAPTER => match entry.adapter.as_ref() {
            Some(adapter) => (&adapter.install, adapter.update.shell_rerun),
            None => return Ok(None),
        },
        _ => return Ok(None),
    };
    if let Some(npm) = &install.npm {
        return resolver.npm(&npm.package).map(Some);
    }
    // Only a shell_rerun recipe fetches from its declared repo; elsewhere
    // `github` is a source pointer.
    if install.github.is_some() || (install.shell.is_some() && shell_rerun) {
        let github_url = if step == STEP_ADAPTER {
            entry
                .adapter
                .as_ref()
                .and_then(|a| a.github.as_deref())
                .or(entry.github.as_deref())
        } else {
            entry.github.as_deref()
        };
        let Some(github_url) = github_url else {
            return Ok(None);
        };
        let repo = crate::runtime::install::agent_registry::github_repo_from_url(
            &entry.id, "github", github_url,
        )?;
        return resolver.github(&repo).map(Some);
    }
    Ok(None)
}

/// Compare an installed version against an optional upstream version. Pure
/// function so the comparison rules can be unit-tested without touching the
/// network or the registry.
pub fn compare_versions(installed: &str, latest: Option<&str>) -> AgentVersionStatus {
    match latest {
        None => AgentVersionStatus::Unknown {
            reason: format!(
                "no machine-checkable upstream for this step (installed `{installed}`); run `acps installer history` for the full row"
            ),
        },
        Some(latest) => {
            if normalize_version(installed) == normalize_version(latest) {
                AgentVersionStatus::UpToDate {
                    version: installed.to_owned(),
                }
            } else {
                AgentVersionStatus::Stale {
                    installed: installed.to_owned(),
                    latest: latest.to_owned(),
                }
            }
        }
    }
}

/// Strip a leading `v` so a `v0.11.1` installer row compares equal to a
/// `0.11.1` npm registry response (and vice versa). Other normalization (e.g.
/// pre-release tags) is deliberately not applied — we want to flag any other
/// drift as stale.
pub(crate) fn normalize_version(value: &str) -> &str {
    value
        .trim()
        .strip_prefix('v')
        .unwrap_or_else(|| value.trim())
}

/// Walk the registry's expected managed steps for an agent and pair each one
/// with a freshness verdict. Missing successful rows are reported explicitly
/// so partial adapter installs cannot look healthy.
pub fn build_agent_check_report(
    entry: &RegistryEntry,
    agent: &crate::config::AgentConfig,
    installed_rows: &[InstallerRun],
    resolver: &dyn LatestVersionResolver,
) -> Vec<(String, AgentVersionStatus)> {
    // An operator adapter override reshapes the expected steps; a failed conversion becomes an
    // Unknown verdict rather than aborting the whole report.
    let entry =
        match crate::runtime::install::agent_registry::effective_registry_entry(entry, agent) {
            Ok(entry) => entry,
            Err(err) => {
                return vec![(
                    STEP_ADAPTER.to_owned(),
                    AgentVersionStatus::Unknown {
                        reason: format!("invalid [agent.adapter_override]: {err}"),
                    },
                )];
            }
        };
    let entry = entry.as_ref();
    let expected_steps = expected_agent_check_steps(entry);
    let mut out = Vec::with_capacity(expected_steps.len());
    for step in expected_steps {
        let Some(row) = installed_rows.iter().find(|row| row.step == step) else {
            out.push((step.to_owned(), AgentVersionStatus::NotInstalled));
            continue;
        };
        let latest = match resolve_upstream_version_for_step(entry, step, resolver) {
            Ok(value) => value,
            Err(err) => {
                out.push((
                    step.to_owned(),
                    AgentVersionStatus::Unknown {
                        reason: format!("upstream lookup failed: {err}"),
                    },
                ));
                continue;
            }
        };
        let status = match row.version.as_deref() {
            Some(installed) => compare_versions(installed, latest.as_deref()),
            None => AgentVersionStatus::Unknown {
                reason: if latest.is_some() {
                    "installed version was not recorded; run `acps installer history` for the full row"
                        .to_owned()
                } else {
                    "no machine-checkable upstream for this step; run `acps installer history` for the full row"
                        .to_owned()
                },
            },
        };
        out.push((step.to_owned(), status));
    }
    out
}

fn expected_agent_check_steps(entry: &RegistryEntry) -> Vec<&'static str> {
    if entry.kind == RegistryKind::Adapter {
        let harness_is_provided_by_adapter = entry
            .harness
            .as_ref()
            .is_some_and(|harness| harness.install.is_provided_by_adapter());
        if harness_is_provided_by_adapter {
            vec![STEP_ADAPTER]
        } else {
            vec![STEP_HARNESS, STEP_ADAPTER]
        }
    } else {
        vec![STEP_INSTALL]
    }
}

/// The installed components an agent launch actually uses, resolved from the
/// effective registry entry so stale installer rows from a previous install
/// shape of the same agent id never leak into a version report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledComponents {
    pub adapter_id: Option<String>,
    pub steps: Vec<&'static str>,
}

impl InstalledComponents {
    /// Registry-unavailable fallback: shape from the daemon-populated
    /// `[agent].adapter` metadata alone. Only the registry knows whether the
    /// adapter bundles its harness, so the harness step is left out rather
    /// than risk pairing the launch with a stale split-harness row.
    pub fn from_agent_config(agent: &crate::config::AgentConfig) -> Self {
        match agent.adapter.as_ref() {
            Some(adapter) => Self {
                adapter_id: Some(adapter.id.clone()),
                steps: vec![STEP_ADAPTER],
            },
            None => Self::native(),
        }
    }

    fn native() -> Self {
        Self {
            adapter_id: None,
            steps: vec![STEP_INSTALL],
        }
    }
}

pub fn installed_components(
    registry: &RegistryCatalog,
    agent: &crate::config::AgentConfig,
) -> InstalledComponents {
    // Escape-hatch custom agents are absent from the registry and install through the `install` step.
    let Some(entry) = registry.lookup(&agent.id) else {
        return InstalledComponents::native();
    };
    let entry = match crate::runtime::install::agent_registry::effective_registry_entry(
        entry, agent,
    ) {
        Ok(entry) => entry,
        Err(error) => {
            tracing::warn!(agent = %agent.id, %error, "ignoring [agent.adapter_override] for installed component resolution");
            std::borrow::Cow::Borrowed(entry)
        }
    };
    let entry = entry.as_ref();
    if entry.kind != RegistryKind::Adapter {
        return InstalledComponents::native();
    }
    InstalledComponents {
        adapter_id: entry.adapter.as_ref().map(|adapter| adapter.id.clone()),
        steps: expected_agent_check_steps(entry),
    }
}

pub fn agent_check_has_failure(report: &[(String, AgentVersionStatus)]) -> bool {
    report.iter().any(|(_, status)| {
        matches!(
            status,
            AgentVersionStatus::Stale { .. } | AgentVersionStatus::NotInstalled
        )
    })
}

#[cfg(test)]
mod installed_components_tests {
    use super::*;

    const REGISTRY: &str = r#"
[[agents]]
id = "native-agent"
name = "Native Agent"
kind = "native"
headless_compatible = true
support_doc = "docs/agents/native-agent.md"

[agents.harness]
id = "native-agent"

[agents.harness.install.npm]
package = "@example/native-agent"
creates = "native-agent"

[[agents]]
id = "split-agent"
name = "Split Adapter Agent"
kind = "adapter"
headless_compatible = true
support_doc = "docs/agents/split-agent.md"

[agents.adapter]
id = "split-acp"
github = "example/split-acp"

[agents.adapter.install.npm]
package = "@example/split-acp"
creates = "split-acp"

[agents.harness]
id = "split-harness"

[agents.harness.install.npm]
package = "@example/split-harness"
creates = "split-harness"

[[agents]]
id = "bundled-agent"
name = "Bundled Adapter Agent"
kind = "adapter"
headless_compatible = true
support_doc = "docs/agents/bundled-agent.md"

[agents.adapter]
id = "bundled-acp"
github = "example/bundled-acp"

[agents.adapter.install.npm]
package = "@example/bundled-acp"
creates = "bundled-acp"

[agents.harness]
id = "bundled-sdk"

[agents.harness.install]
provided_by = "adapter"
"#;

    fn registry() -> RegistryCatalog {
        RegistryCatalog::from_toml(REGISTRY).expect("test registry parses")
    }

    fn agent(id: &str) -> crate::config::AgentConfig {
        let mut config = crate::config::load_config_from_str(include_str!(
            "../../../tests/fixtures/valid-opencode-stack.toml"
        ))
        .expect("fixture parses");
        config.agent.id = id.to_owned();
        config.agent.adapter = None;
        config.agent.adapter_override = None;
        config.agent
    }

    #[test]
    fn native_entry_uses_the_install_step_only() {
        let components = installed_components(&registry(), &agent("native-agent"));
        assert_eq!(
            components,
            InstalledComponents {
                adapter_id: None,
                steps: vec![STEP_INSTALL],
            }
        );
    }

    #[test]
    fn adapter_entry_with_separate_harness_uses_both_steps() {
        let components = installed_components(&registry(), &agent("split-agent"));
        assert_eq!(
            components,
            InstalledComponents {
                adapter_id: Some("split-acp".to_owned()),
                steps: vec![STEP_HARNESS, STEP_ADAPTER],
            }
        );
    }

    #[test]
    fn adapter_entry_with_bundled_harness_uses_the_adapter_step_only() {
        let components = installed_components(&registry(), &agent("bundled-agent"));
        assert_eq!(
            components,
            InstalledComponents {
                adapter_id: Some("bundled-acp".to_owned()),
                steps: vec![STEP_ADAPTER],
            }
        );
    }

    #[test]
    fn adapter_override_turns_a_native_entry_adapter_shaped() {
        let mut agent = agent("native-agent");
        agent.adapter_override = Some(crate::config::AgentAdapterOverrideConfig {
            command: "custom-acp".to_owned(),
            args: Vec::new(),
            github: Some("example/custom-acp".to_owned()),
            install: crate::config::AgentAdapterOverrideInstall {
                shell: None,
                npm: Some(crate::config::AgentAdapterOverrideNpmInstall {
                    package: "custom-acp".to_owned(),
                    creates: "custom-acp".to_owned(),
                }),
                github: None,
            },
            update: Default::default(),
        });
        let components = installed_components(&registry(), &agent);
        assert_eq!(
            components,
            InstalledComponents {
                adapter_id: Some("custom-acp".to_owned()),
                steps: vec![STEP_HARNESS, STEP_ADAPTER],
            }
        );
    }

    #[test]
    fn unknown_agent_is_treated_as_native() {
        let components = installed_components(&registry(), &agent("escape-hatch"));
        assert_eq!(components, InstalledComponents::native());
    }

    #[test]
    fn config_fallback_follows_the_populated_adapter_metadata() {
        let mut agent = agent("split-agent");
        assert_eq!(
            InstalledComponents::from_agent_config(&agent),
            InstalledComponents::native()
        );
        agent.adapter = Some(crate::config::AgentAdapterConfig {
            id: "split-acp".to_owned(),
            name: "Split Adapter Agent".to_owned(),
            upstream_agent: "split-harness".to_owned(),
            source_url: None,
        });
        assert_eq!(
            InstalledComponents::from_agent_config(&agent),
            InstalledComponents {
                adapter_id: Some("split-acp".to_owned()),
                steps: vec![STEP_ADAPTER],
            }
        );
    }
}
