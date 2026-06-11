use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::error::Result;
use crate::runtime::install::agent_installer::{InstallerOutcome, install_resolved, run_installer};
use crate::runtime::install::agent_registry::RegistryCatalog;
use crate::secrets::SecretStore;
use crate::state::StateStore;

pub(super) fn should_install_agent(config: &Config, registry: &RegistryCatalog) -> Result<bool> {
    let entry = registry.lookup_required(&config.agent.id)?;
    entry.ensure_supported()?;
    #[cfg(feature = "test-fixtures")]
    if crate::dev_gates::fixture_enabled(crate::dev_gates::TEST_SKIP_AGENT_INSTALL_ENV) {
        return Ok(false);
    }
    #[cfg(feature = "test-fixtures")]
    if let Some(placebo_path) =
        crate::runtime::install::agent_registry::development_placebo_registry_path()
    {
        let placebo_id = placebo_path.display().to_string();
        if entry
            .harness
            .as_ref()
            .is_some_and(|harness| harness.id == placebo_id)
            && !Path::new(&config.workspace.root).is_dir()
        {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Run the installer for the configured agent. The TTY-only "try the next
/// install path?" prompt that used to live here is gone: `install_resolved`
/// already walks `shell → npm → github_release` in sequence, and any
/// remaining failure is captured by the init orchestrator's
/// `agent_install` step. The operator re-attempts by running
/// `acps init --resume`, which re-executes the failed step using the
/// current registry — picking up a newer harness version, a now-reachable
/// npm registry, or a freshly released GitHub artifact without ever
/// requiring a TTY.
pub(super) fn install_configured_agent(
    home: &Path,
    config: &Config,
    registry: &RegistryCatalog,
    store: &StateStore,
) -> Result<InstallerOutcome> {
    let workspace_root = PathBuf::from(config.workspace.root.clone());
    let log_base = crate::state::default_installer_log_base(home);
    if let Some(install) = config.agent.install.as_ref() {
        let env = resolve_agent_env(home, config)?;
        return run_installer(
            &config.agent.id,
            install,
            config.agent.expected_sha256.as_deref(),
            env,
            &workspace_root,
            store,
            Some(&log_base),
        );
    }
    let entry = registry.lookup_required(&config.agent.id)?;
    install_resolved(
        &config.agent,
        entry,
        Default::default(),
        &workspace_root,
        &local_bin_dir(home),
        store,
        Some(&log_base),
    )
}

fn resolve_agent_env(home: &Path, config: &Config) -> Result<HashMap<String, String>> {
    if config.agent.env.is_empty() {
        return Ok(HashMap::new());
    }
    let store = SecretStore::open(home)?;
    let mut env = HashMap::with_capacity(config.agent.env.len());
    for name in &config.agent.env {
        let value = store.get(name)?;
        env.insert(name.clone(), value.to_owned());
    }
    Ok(env)
}

pub(super) fn operator_registry_override(home: &Path) -> PathBuf {
    crate::runtime::install::operator_registry_override(home)
}

pub(super) fn local_bin_dir(home: &Path) -> PathBuf {
    crate::runtime::install::local_bin_dir(home)
}
