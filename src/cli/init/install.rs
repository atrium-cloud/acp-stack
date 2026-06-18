use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::Config;
use crate::error::{Result, StackError};
use crate::runtime::install::agent_installer::{InstallerOutcome, install_resolved, run_installer};
use crate::runtime::install::agent_registry::RegistryCatalog;
use crate::secrets::SecretStore;
use crate::state::StateStore;

use super::registry_apply::is_custom_agent;

pub(super) fn should_install_agent(config: &Config, registry: &RegistryCatalog) -> Result<bool> {
    // A custom agent carries its own `[agent.install]` escape hatch and is not
    // in the registry, so the registry support checks below do not apply.
    if is_custom_agent(config, registry) {
        #[cfg(feature = "test-fixtures")]
        if crate::dev_gates::fixture_enabled(crate::dev_gates::TEST_SKIP_AGENT_INSTALL_ENV) {
            return Ok(false);
        }
        return Ok(true);
    }
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

// CONSTANTS — agent install retry. The installer engine records every attempt
// to `installer_runs`, so a retried install is fully audited. Preflight has
// already validated config/support before this point, so failures here are
// predominantly transient (network, registry, release availability).
pub(super) const MAX_INSTALL_ATTEMPTS: u32 = 10;
const INSTALL_RETRY_BASE_DELAY: Duration = Duration::from_secs(2);
const INSTALL_RETRY_MAX_DELAY: Duration = Duration::from_secs(60);
const INSTALL_RETRY_MAX_EXPONENT: u32 = 5;

/// Exponential backoff with a cap, for the 1-based `attempt` that just failed:
/// `base * 2^(attempt-1)`, clamped to `INSTALL_RETRY_MAX_DELAY`.
pub(super) fn install_retry_backoff(attempt: u32) -> Duration {
    let exponent = attempt.saturating_sub(1).min(INSTALL_RETRY_MAX_EXPONENT);
    INSTALL_RETRY_BASE_DELAY
        .checked_mul(1u32 << exponent)
        .unwrap_or(INSTALL_RETRY_MAX_DELAY)
        .min(INSTALL_RETRY_MAX_DELAY)
}

/// Whether an install failure is worth retrying. Deterministic failures — a
/// hash mismatch, a missing `creates` target, missing prerequisites, an
/// unconfigured agent, or a corrupt registry — will fail identically on every
/// attempt, so retrying them just makes the operator wait. Ambiguous failures
/// (a failed install command, a spawn error) may be transient (network mid
/// install), so those stay retryable.
fn install_error_is_retryable(error: &StackError) -> bool {
    !matches!(
        error,
        StackError::AgentNotConfigured
            | StackError::AgentInstallerCreatesMissing { .. }
            | StackError::AgentInstallerPrerequisitesMissing { .. }
            | StackError::AgentInstallerWorkingDirectoryMissing { .. }
            | StackError::AgentSha256Mismatch { .. }
            | StackError::RegistryLoad { .. }
    )
}

/// Run an agent install with bounded exponential-backoff retry.
/// `attempt_install` performs one install attempt (and renders its own
/// progress); `on_retry` runs after a failed-but-retryable attempt to log and
/// sleep. Returns the first success, or the last error once attempts are
/// exhausted or a non-retryable (deterministic) error is hit. Kept generic over
/// the two closures so the retry/backoff logic is unit-testable without touching
/// the installer or sleeping.
pub(super) fn run_install_with_retry(
    mut attempt_install: impl FnMut(u32) -> Result<InstallerOutcome>,
    mut on_retry: impl FnMut(u32, &StackError, Duration),
) -> Result<InstallerOutcome> {
    let mut attempt = 1u32;
    loop {
        match attempt_install(attempt) {
            Ok(outcome) => return Ok(outcome),
            Err(error) => {
                if attempt >= MAX_INSTALL_ATTEMPTS || !install_error_is_retryable(&error) {
                    return Err(error);
                }
                let delay = install_retry_backoff(attempt);
                on_retry(attempt, &error, delay);
                attempt += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn fake_outcome() -> InstallerOutcome {
        InstallerOutcome::AlreadyPresent {
            path: PathBuf::from("/tmp/agent"),
            sha256: String::new(),
        }
    }

    fn fake_error() -> StackError {
        StackError::InvalidParam {
            field: "install",
            reason: "transient".to_owned(),
        }
    }

    fn deterministic_error() -> StackError {
        StackError::AgentSha256Mismatch {
            expected: "a".to_owned(),
            actual: "b".to_owned(),
        }
    }

    #[test]
    fn retry_stops_immediately_on_deterministic_error() {
        let attempts = Cell::new(0u32);
        let result = run_install_with_retry(
            |attempt| {
                attempts.set(attempt);
                Err::<InstallerOutcome, _>(deterministic_error())
            },
            |_, _, _| panic!("a deterministic error must not be retried"),
        );
        assert!(result.is_err());
        assert_eq!(attempts.get(), 1, "should fail on the first attempt");
    }

    #[test]
    fn backoff_grows_then_caps() {
        assert_eq!(install_retry_backoff(1), Duration::from_secs(2));
        assert_eq!(install_retry_backoff(2), Duration::from_secs(4));
        assert_eq!(install_retry_backoff(4), Duration::from_secs(16));
        // capped at INSTALL_RETRY_MAX_DELAY (60s) once 2*2^n exceeds it.
        assert_eq!(install_retry_backoff(9), Duration::from_secs(60));
    }

    #[test]
    fn retry_succeeds_after_transient_failures() {
        let attempts = Cell::new(0u32);
        let retries = Cell::new(0u32);
        let outcome = run_install_with_retry(
            |attempt| {
                attempts.set(attempt);
                if attempt < 3 {
                    Err(fake_error())
                } else {
                    Ok(fake_outcome())
                }
            },
            |_, _, _| retries.set(retries.get() + 1),
        )
        .expect("install should succeed on the third attempt");
        assert!(matches!(outcome, InstallerOutcome::AlreadyPresent { .. }));
        assert_eq!(attempts.get(), 3);
        assert_eq!(retries.get(), 2, "two retries before the third attempt");
    }

    #[test]
    fn retry_exhausts_after_max_attempts() {
        let attempts = Cell::new(0u32);
        let result = run_install_with_retry(
            |attempt| {
                attempts.set(attempt);
                Err::<InstallerOutcome, _>(fake_error())
            },
            |_, _, _| {},
        );
        assert!(result.is_err());
        assert_eq!(attempts.get(), MAX_INSTALL_ATTEMPTS);
    }
}
