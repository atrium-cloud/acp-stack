use crate::config::Config;
use crate::error::{Result, StackError};
use crate::fs_util::{
    create_dir_owner_only, home_dir, parent_dir, pre_create_owner_only, set_owner_only_file,
};
use crate::runtime::install::agent_installer::{STEP_ADAPTER, STEP_HARNESS, STEP_INSTALL};
use crate::runtime::install::agent_registry::{RegistryCatalog, RegistryEntry, RegistryKind};
use crate::state::{StateStore, default_state_path};

use super::install::operator_registry_override;

/// Result of comparing the installed managed-agent version against upstream.
/// Carried as a typed enum so the CLI printer and test cases can pattern-match
/// the four states deterministically.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum AgentCheckStatus {
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

/// Sources of "latest version" used by `acps agent check`. Trait-based so unit
/// tests can substitute a deterministic mock; the production runtime injects
/// `LiveLatestVersionResolver` which actually hits npm and GitHub.
pub(super) trait LatestVersionResolver {
    fn npm(&self, package: &str) -> Result<String>;
    fn github(&self, repo: &str) -> Result<String>;
}

struct LiveLatestVersionResolver;

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
    // Shell-recipe installs have no machine-checkable upstream; let the caller
    // render this as "unknown, manual check required".
    Ok(None)
}

/// Compare an installed version against an optional upstream version. Pure
/// function so the comparison rules can be unit-tested without touching the
/// network or the registry.
pub(super) fn compare_versions(installed: &str, latest: Option<&str>) -> AgentCheckStatus {
    match latest {
        None => AgentCheckStatus::Unknown {
            reason: format!(
                "no machine-checkable upstream for this step (installed `{installed}`); run `acps installer history` for the full row"
            ),
        },
        Some(latest) => {
            if normalize_version(installed) == normalize_version(latest) {
                AgentCheckStatus::UpToDate {
                    version: installed.to_owned(),
                }
            } else {
                AgentCheckStatus::Stale {
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
fn normalize_version(value: &str) -> &str {
    value
        .trim()
        .strip_prefix('v')
        .unwrap_or_else(|| value.trim())
}

pub(super) fn run_agent_check() -> Result<()> {
    let home = home_dir()?;
    let config = Config::load_from_default_path()?;
    let registry = RegistryCatalog::load_with_override(&operator_registry_override(&home))?;
    let entry =
        registry
            .lookup(&config.agent.id)
            .ok_or_else(|| StackError::AgentRegistryMissing {
                id: config.agent.id.clone(),
            })?;
    let state_path = default_state_path(&home);
    let state_dir = parent_dir(&state_path)?;
    create_dir_owner_only(state_dir)?;
    pre_create_owner_only(&state_path)?;
    let store = StateStore::open(&state_path)?;
    store.migrate()?;
    set_owner_only_file(&state_path)?;
    let installed_rows = store.latest_successful_installer_runs_for_agent(&config.agent.id)?;

    let resolver = LiveLatestVersionResolver;
    let report = build_agent_check_report(entry, &installed_rows, &resolver);
    let has_failure = agent_check_has_failure(&report);

    println!("agent check: {}", config.agent.id);
    if report.is_empty() {
        println!(
            "no installer runs recorded for `{}`; run `acps agent install` first",
            config.agent.id
        );
        return Ok(());
    }
    for (step, status) in &report {
        match status {
            AgentCheckStatus::UpToDate { version } => {
                println!("{step}: up-to-date ({version})");
            }
            AgentCheckStatus::Stale { installed, latest } => {
                println!("{step}: stale (installed {installed}, latest {latest})");
            }
            AgentCheckStatus::Unknown { reason } => {
                println!("{step}: unknown ({reason})");
            }
            AgentCheckStatus::NotInstalled => {
                println!("{step}: not installed");
            }
        }
    }
    if has_failure {
        return Err(StackError::AgentCheckStale);
    }
    Ok(())
}

/// Walk the registry's expected managed steps for an agent and pair each one
/// with a freshness verdict. Missing successful rows are reported explicitly
/// so partial adapter installs cannot look healthy.
pub(super) fn build_agent_check_report(
    entry: &RegistryEntry,
    installed_rows: &[crate::state::InstallerRun],
    resolver: &dyn LatestVersionResolver,
) -> Vec<(String, AgentCheckStatus)> {
    let expected_steps = expected_agent_check_steps(entry);
    let mut out = Vec::with_capacity(expected_steps.len());
    for step in expected_steps {
        let Some(row) = installed_rows.iter().find(|row| row.step == *step) else {
            out.push(((*step).to_owned(), AgentCheckStatus::NotInstalled));
            continue;
        };
        let latest = match resolve_upstream_version_for_step(entry, step, resolver) {
            Ok(value) => value,
            Err(err) => {
                out.push((
                    (*step).to_owned(),
                    AgentCheckStatus::Unknown {
                        reason: format!("upstream lookup failed: {err}"),
                    },
                ));
                continue;
            }
        };
        let status = match row.version.as_deref() {
            Some(installed) => compare_versions(installed, latest.as_deref()),
            None => AgentCheckStatus::Unknown {
                reason: if latest.is_some() {
                    "installed version was not recorded; run `acps installer history` for the full row"
                        .to_owned()
                } else {
                    "no machine-checkable upstream for this step; run `acps installer history` for the full row"
                        .to_owned()
                },
            },
        };
        out.push(((*step).to_owned(), status));
    }
    out
}

fn expected_agent_check_steps(entry: &RegistryEntry) -> &'static [&'static str] {
    if entry.kind == RegistryKind::Adapter {
        &[STEP_HARNESS, STEP_ADAPTER]
    } else {
        &[STEP_INSTALL]
    }
}

pub(super) fn agent_check_has_failure(report: &[(String, AgentCheckStatus)]) -> bool {
    report.iter().any(|(_, status)| {
        matches!(
            status,
            AgentCheckStatus::Stale { .. } | AgentCheckStatus::NotInstalled
        )
    })
}
