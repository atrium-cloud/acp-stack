//! Installed-vs-upstream version comparison for managed registry agents.
//! Shared by `acps agent check` and the `GET /v1/agent/update/status` route.

use serde::Serialize;

use crate::error::Result;
use crate::runtime::install::agent_installer::{STEP_ADAPTER, STEP_HARNESS, STEP_INSTALL};
use crate::runtime::install::agent_registry::{RegistryEntry, RegistryKind};
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
    let install = match step {
        STEP_HARNESS | STEP_INSTALL => entry.harness.as_ref().map(|h| &h.install),
        STEP_ADAPTER => entry.adapter.as_ref().map(|a| &a.install),
        _ => None,
    };
    let Some(install) = install else {
        return Ok(None);
    };
    if let Some(npm) = &install.npm {
        return resolver.npm(&npm.package).map(Some);
    }
    if let Some(_github) = &install.github {
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
    // Shell-recipe installs have no machine-checkable upstream.
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

pub fn agent_check_has_failure(report: &[(String, AgentVersionStatus)]) -> bool {
    report.iter().any(|(_, status)| {
        matches!(
            status,
            AgentVersionStatus::Stale { .. } | AgentVersionStatus::NotInstalled
        )
    })
}
