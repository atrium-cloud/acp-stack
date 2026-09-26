use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::Config;
use crate::error::{Result, StackError};
use crate::runtime::agent::provider_keys::{
    resolve_agent_environment, resolve_agent_environment_without_secrets,
};
use crate::runtime::install::agent_installer::{
    HarnessInstall, InstallerOutcome, install_resolved, resolve_creates_for_init_resume,
    run_installer,
};
use crate::runtime::install::agent_registry::{RegistryCatalog, effective_registry_entry};
use crate::runtime::install::install_ownership::{
    BinaryOwnership, ComponentRole, InstallComponent, classify_component, install_components,
};
use crate::secrets::SecretStore;
use crate::state::StateStore;

use super::registry_apply::is_custom_agent;
use super::{ExistingAgentArg, InitArgs};

// CONSTANTS: agent CLI version pins.
/// Longest accepted agent CLI version; release tags and npm versions are far shorter.
const MAX_AGENT_VERSION_BYTES: usize = 128;
/// `AgentVersionUnsupported` reason for an agent installed by its own `[agent.install]` recipe.
const CUSTOM_AGENT_VERSION_REASON: &str =
    "a custom agent installs through its own shell recipe, which takes no version";

/// Check a version's shape before it reaches a release URL or an npm spec: letters, digits,
/// `.`, `_`, `+`, and `-`, starting with a letter or digit.
pub(super) fn validate_agent_version_value(field: &'static str, version: &str) -> Result<()> {
    let well_formed = !version.is_empty()
        && version.len() <= MAX_AGENT_VERSION_BYTES
        && version.starts_with(|character: char| character.is_ascii_alphanumeric())
        && version.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '+' | '-')
        });
    if well_formed {
        return Ok(());
    }
    Err(StackError::InvalidParam {
        field,
        reason: format!(
            "must be a release tag or package version of at most {MAX_AGENT_VERSION_BYTES} letters, digits, `.`, `_`, `+`, or `-`"
        ),
    })
}

/// Reject `--agent-version`/`--existing-agent` combinations no install can honor, before any
/// step runs. An interactive run asks for a version `replace-version` is missing instead.
pub(super) fn validate_existing_agent_args(args: &InitArgs, interactive: bool) -> Result<()> {
    if let Some(version) = args.agent_version.as_deref() {
        validate_agent_version_value("--agent-version", version)?;
        if let Some(agent_id) = args.custom_agent_id.as_deref() {
            return Err(StackError::AgentVersionUnsupported {
                agent_id: agent_id.to_owned(),
                version: version.to_owned(),
                reason: CUSTOM_AGENT_VERSION_REASON,
            });
        }
    }
    match (args.existing_agent, args.agent_version.is_some()) {
        (
            Some(choice @ (ExistingAgentArg::UseExisting | ExistingAgentArg::ReplaceLatest)),
            true,
        ) => Err(StackError::InvalidParam {
            field: "--existing-agent",
            reason: format!(
                "`{}` conflicts with --agent-version",
                choice.as_config_value()
            ),
        }),
        (Some(ExistingAgentArg::ReplaceVersion), false) if !interactive => {
            Err(StackError::InvalidParam {
                field: "--existing-agent",
                reason: "`replace-version` requires --agent-version".to_owned(),
            })
        }
        _ => Ok(()),
    }
}

/// Why the configured agent's install lanes cannot honor a CLI version pin, or `None` when they
/// can. Runs once the agent is final.
pub(super) fn agent_version_pin_blocker(
    config: &Config,
    registry: &RegistryCatalog,
) -> Result<Option<&'static str>> {
    // An `[agent.install]` recipe drives the install whenever it is set, registry id or not.
    if config.agent.install.is_some() {
        return Ok(Some(CUSTOM_AGENT_VERSION_REASON));
    }
    let entry = registry.lookup_required(&config.agent.id)?;
    let entry = effective_registry_entry(entry, &config.agent)?;
    let harness = entry
        .harness
        .as_ref()
        .ok_or_else(|| StackError::RegistryLoad {
            reason: format!("registry entry `{}` has no harness block", entry.id),
        })?;
    Ok(harness.install.version_pin_blocker())
}

/// Refuse a CLI version pin the agent's install lanes cannot honor. Runs once the agent is final.
pub(super) fn ensure_agent_version_installable(
    config: &Config,
    registry: &RegistryCatalog,
    version: &str,
) -> Result<()> {
    match agent_version_pin_blocker(config, registry)? {
        Some(reason) => Err(StackError::AgentVersionUnsupported {
            agent_id: config.agent.id.clone(),
            version: version.to_owned(),
            reason,
        }),
        None => Ok(()),
    }
}

/// The binaries installing the configured agent lays down, each classified by who put it where
/// the command resolver finds it. An `[agent.install]` recipe keeps whatever its `creates`
/// resolves, so an agent installed by one has none to classify.
pub(super) fn detect_existing_agent_binaries(
    config: &Config,
    registry: &RegistryCatalog,
    store: &StateStore,
    home: &Path,
) -> Result<Vec<(InstallComponent, BinaryOwnership)>> {
    if config.agent.install.is_some() {
        return Ok(Vec::new());
    }
    let entry = registry.lookup_required(&config.agent.id)?;
    let workspace_root = Path::new(&config.workspace.root);
    install_components(&config.agent, entry)?
        .into_iter()
        .map(|component| {
            let ownership =
                classify_component(store, &config.agent.id, &component, workspace_root, home)?;
            Ok((component, ownership))
        })
        .collect()
}

/// What a completed `agent_install` step recorded in its payload.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct RecordedAgentInstall {
    /// `None` when the step found no agent CLI acp-stack did not install.
    pub(super) existing_agent: Option<ExistingAgentArg>,
    /// The `[agent].harness_version` the step installed.
    pub(super) harness_version: Option<String>,
}

pub(super) fn recorded_agent_install(payload_json: &str) -> Result<RecordedAgentInstall> {
    let corrupted = |reason: &str| StackError::InitRunCorrupted {
        reason: format!("agent install step payload {reason}"),
    };
    let payload: serde_json::Value =
        serde_json::from_str(payload_json).map_err(|_| corrupted("is invalid"))?;
    let existing_agent = payload
        .get("existing_agent")
        .and_then(|existing| existing.get("choice"))
        .map(|choice| {
            choice
                .as_str()
                .and_then(ExistingAgentArg::from_config_value)
                .ok_or_else(|| corrupted("records an unknown existing_agent choice"))
        })
        .transpose()?;
    let harness_version = match payload.get("harness_version") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(version)) => Some(version.clone()),
        Some(_) => return Err(corrupted("records a non-string harness_version")),
    };
    Ok(RecordedAgentInstall {
        existing_agent,
        harness_version,
    })
}

/// Whether a resumed `agent_install` step may be skipped: every binary it lays down still
/// resolves and spawns, and was installed by acp-stack, except an agent CLI the recorded choice
/// kept. A replacement choice therefore never passes on a binary acp-stack did not install.
#[allow(clippy::too_many_arguments)]
pub(super) fn installer_postcondition_holds(
    config: &Config,
    registry: &RegistryCatalog,
    store: &StateStore,
    choice: Option<ExistingAgentArg>,
    workspace_root: &Path,
    local_bin_dir: &Path,
    home: &Path,
) -> Result<bool> {
    let (target, extra_path_dirs): (&str, Vec<&Path>) =
        if let Some(install) = config.agent.install.as_ref() {
            (install.creates.as_str(), Vec::new())
        } else {
            (config.agent.command.as_str(), vec![local_bin_dir])
        };
    let entry_point_runs = resolve_creates_for_init_resume(
        target,
        workspace_root,
        &extra_path_dirs,
        config.agent.expected_sha256.as_deref(),
        home,
    )
    .is_some();
    if !entry_point_runs {
        return Ok(false);
    }
    for (component, ownership) in detect_existing_agent_binaries(config, registry, store, home)? {
        let holds = match (component.role, ownership) {
            (_, BinaryOwnership::Absent) => false,
            (_, BinaryOwnership::Installed(_)) => true,
            (ComponentRole::Harness, BinaryOwnership::Kept(_) | BinaryOwnership::Foreign(_)) => {
                choice == Some(ExistingAgentArg::UseExisting)
            }
            (ComponentRole::Adapter, BinaryOwnership::Kept(_) | BinaryOwnership::Foreign(_)) => {
                false
            }
        };
        if !holds
            || resolve_creates_for_init_resume(
                &component.command,
                workspace_root,
                &[local_bin_dir],
                None,
                home,
            )
            .is_none()
        {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(super) fn should_install_agent(config: &Config, registry: &RegistryCatalog) -> Result<bool> {
    // A custom agent carries its own `[agent.install]` escape hatch, so the registry checks below do not apply.
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

/// Run the installer for the configured agent. `harness` applies to registry agents only: a
/// custom agent's `[agent.install]` recipe has no separate CLI to keep.
pub(super) fn install_configured_agent(
    home: &Path,
    config: &Config,
    registry: &RegistryCatalog,
    store: &StateStore,
    harness: &HarnessInstall,
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
            home,
        );
    }
    let entry = registry.lookup_required(&config.agent.id)?;
    install_resolved(
        &config.agent,
        entry,
        harness,
        Default::default(),
        &workspace_root,
        &local_bin_dir(home),
        store,
        Some(&log_base),
        home,
    )
}

fn resolve_agent_env(home: &Path, config: &Config) -> Result<HashMap<String, String>> {
    if let Some(environment) = resolve_agent_environment_without_secrets(config) {
        return Ok(environment.env);
    }
    let store = SecretStore::open(home)?;
    Ok(resolve_agent_environment(config, &store)?.env)
}

pub(super) fn operator_registry_override(home: &Path) -> PathBuf {
    crate::runtime::install::operator_registry_override(home)
}

pub(super) fn local_bin_dir(home: &Path) -> PathBuf {
    crate::runtime::install::local_bin_dir(home)
}

// CONSTANTS: agent install retry.
pub(super) const MAX_INSTALL_ATTEMPTS: u32 = 10;
const INSTALL_RETRY_BASE_DELAY: Duration = Duration::from_secs(2);
const INSTALL_RETRY_MAX_DELAY: Duration = Duration::from_secs(60);
const INSTALL_RETRY_MAX_EXPONENT: u32 = 5;
/// Wall-clock ceiling on RETRIES, checked between attempts, so the worst case is the budget plus one in-flight attempt. It bounds a pathological installer that times out every try, which the attempt cap alone does not.
pub(super) const INSTALL_RETRY_TOTAL_BUDGET: Duration = Duration::from_secs(20 * 60);

/// Exponential backoff for the 1-based `attempt` that just failed, clamped to `INSTALL_RETRY_MAX_DELAY`.
pub(super) fn install_retry_backoff(attempt: u32) -> Duration {
    crate::time_util::exponential_backoff_delay(
        attempt,
        INSTALL_RETRY_BASE_DELAY,
        INSTALL_RETRY_MAX_DELAY,
        INSTALL_RETRY_MAX_EXPONENT,
    )
}

/// Whether an install failure is worth retrying: the listed failures are deterministic given the same recipe and host, so they fail identically on every attempt.
fn install_error_is_retryable(error: &StackError) -> bool {
    !matches!(
        error,
        StackError::AgentNotConfigured
            | StackError::AgentInstallerBinaryUnrunnable { .. }
            | StackError::AgentInstallerCreatesMissing { .. }
            | StackError::AgentInstallerPrerequisitesMissing { .. }
            | StackError::AgentInstallerWorkingDirectoryMissing { .. }
            | StackError::AgentSha256Mismatch { .. }
            | StackError::AgentVersionUnsupported { .. }
            | StackError::RegistryLoad { .. }
    )
}

/// Run an agent install with bounded exponential-backoff retry; generic over its closures so the retry logic is testable without the installer or real sleeps.
pub(super) fn run_install_with_retry(
    mut attempt_install: impl FnMut(u32) -> Result<InstallerOutcome>,
    mut on_retry: impl FnMut(u32, &StackError, Duration),
    mut elapsed: impl FnMut() -> Duration,
) -> Result<InstallerOutcome> {
    let mut attempt = 1u32;
    loop {
        match attempt_install(attempt) {
            Ok(outcome) => return Ok(outcome),
            Err(error) => {
                if attempt >= MAX_INSTALL_ATTEMPTS
                    || !install_error_is_retryable(&error)
                    || elapsed() >= INSTALL_RETRY_TOTAL_BUDGET
                {
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
mod existing_agent_tests {
    use super::*;
    use crate::runtime::install::agent_installer::InstalledArtifact;
    use crate::state::{INSTALLER_OPERATION_INSTALL, InstallerRunInput};

    const REGISTRY: &str = r#"
[[agents]]
id = "npm-cli"
name = "Npm CLI"
kind = "native"
headless_compatible = true
support_doc = "docs/agents/npm-cli.md"

[agents.harness]
id = "existing-agent-test-cli"

[agents.harness.install.npm]
package = "@example/npm-cli"
creates = "existing-agent-test-cli"

[[agents]]
id = "script-cli"
name = "Script CLI"
kind = "native"
headless_compatible = true
support_doc = "docs/agents/script-cli.md"

[agents.harness]
id = "script-cli"

[agents.harness.install.shell]
script = "exit 1"
creates = "script-cli"

[[agents]]
id = "bundled-cli"
name = "Bundled CLI"
kind = "adapter"
headless_compatible = true
support_doc = "docs/agents/bundled-cli.md"

[agents.adapter]
id = "bundled-cli-acp"

[agents.adapter.install.npm]
package = "@example/bundled-cli-acp"
creates = "bundled-cli-acp"

[agents.harness]
id = "bundled-cli-sdk"

[agents.harness.install]
provided_by = "adapter"
"#;

    fn registry() -> RegistryCatalog {
        RegistryCatalog::from_toml(REGISTRY).expect("registry")
    }

    fn config_for(agent_id: &str, command: &str, workspace_root: &Path) -> Config {
        let mut config = crate::config::load_config_from_str(include_str!(
            "../../../tests/fixtures/valid-opencode-stack.toml"
        ))
        .expect("fixture parses");
        config.agent.id = agent_id.to_owned();
        config.agent.command = command.to_owned();
        config.agent.install = None;
        config.workspace.root = workspace_root.display().to_string();
        config
    }

    fn args_with(existing_agent: Option<ExistingAgentArg>, version: Option<&str>) -> InitArgs {
        InitArgs {
            existing_agent,
            agent_version: version.map(str::to_owned),
            ..InitArgs::default()
        }
    }

    #[test]
    fn existing_agent_flags_reject_combinations_no_install_honors() {
        use ExistingAgentArg::*;
        for choice in [UseExisting, ReplaceLatest] {
            let error = validate_existing_agent_args(&args_with(Some(choice), Some("1.2.3")), true)
                .expect_err("a version contradicts keeping or taking latest");
            assert!(error.to_string().contains("conflicts with --agent-version"));
        }
        let error = validate_existing_agent_args(&args_with(Some(ReplaceVersion), None), false)
            .expect_err("a run that cannot ask needs the version up front");
        assert!(error.to_string().contains("requires --agent-version"));
        validate_existing_agent_args(&args_with(Some(ReplaceVersion), None), true)
            .expect("an interactive run asks for the version instead");
        validate_existing_agent_args(&args_with(Some(ReplaceVersion), Some("v1.2.3")), false)
            .expect("replace-version with its version");
        validate_existing_agent_args(&args_with(None, Some("1.2.3")), false)
            .expect("a version alone implies replace-version");
        let error = validate_existing_agent_args(&args_with(None, Some("../1.2.3")), false)
            .expect_err("a version never carries path separators");
        assert!(matches!(
            error,
            StackError::InvalidParam {
                field: "--agent-version",
                ..
            }
        ));
    }

    #[test]
    fn a_version_pin_needs_a_github_or_npm_lane() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let registry = registry();
        ensure_agent_version_installable(
            &config_for("npm-cli", "existing-agent-test-cli", tempdir.path()),
            &registry,
            "1.2.3",
        )
        .expect("npm can install a chosen version");
        for (agent_id, reason) in [
            ("script-cli", "vendor's install script"),
            ("bundled-cli", "ships inside its ACP adapter"),
        ] {
            let error = ensure_agent_version_installable(
                &config_for(agent_id, agent_id, tempdir.path()),
                &registry,
                "1.2.3",
            )
            .expect_err("no lane can fetch a chosen version");
            assert_eq!(error.error_code(), "agent.version_unsupported");
            assert!(error.to_string().contains(reason), "{error}");
        }
        let mut escape_hatch = config_for("npm-cli", "existing-agent-test-cli", tempdir.path());
        escape_hatch.agent.install = Some(crate::config::AgentInstallConfig {
            install_type: "shell".to_owned(),
            creates: "existing-agent-test-cli".to_owned(),
            shell: Some("exit 1".to_owned()),
        });
        let error = ensure_agent_version_installable(&escape_hatch, &registry, "1.2.3")
            .expect_err("an `[agent.install]` recipe takes no version");
        assert!(error.to_string().contains("own shell recipe"), "{error}");
    }

    #[test]
    fn a_recorded_install_is_read_back_from_the_step_payload() {
        assert_eq!(
            recorded_agent_install(
                r#"{"existing_agent": {"path": "/usr/bin/agent", "choice": "use-existing"}, "harness_version": "v1.2.3"}"#
            )
            .expect("valid payload"),
            RecordedAgentInstall {
                existing_agent: Some(ExistingAgentArg::UseExisting),
                harness_version: Some("v1.2.3".to_owned()),
            }
        );
        assert_eq!(
            recorded_agent_install(
                r#"{"label": "installed", "existing_agent": null, "harness_version": null}"#
            )
            .expect("valid payload"),
            RecordedAgentInstall::default()
        );
        assert_eq!(
            recorded_agent_install(r#"{"label": "installed", "resume": {"verified": true}}"#)
                .expect("a payload without the keys reads as neither"),
            RecordedAgentInstall::default()
        );
        assert!(matches!(
            recorded_agent_install(r#"{"existing_agent": {"choice": "keep"}}"#),
            Err(StackError::InitRunCorrupted { .. })
        ));
        assert!(matches!(
            recorded_agent_install(r#"{"harness_version": 3}"#),
            Err(StackError::InitRunCorrupted { .. })
        ));
    }

    #[test]
    fn a_resumed_install_is_skipped_only_when_its_binaries_match_the_choice() {
        let home = tempfile::tempdir().expect("home");
        let store = StateStore::open(home.path().join("state.sqlite")).expect("state");
        store.migrate().expect("migrate");
        let registry = registry();
        let config = config_for("npm-cli", "existing-agent-test-cli", home.path());
        let local_bin = local_bin_dir(home.path());
        let holds = |choice| {
            installer_postcondition_holds(
                &config,
                &registry,
                &store,
                choice,
                home.path(),
                &local_bin,
                home.path(),
            )
            .expect("postcondition")
        };
        assert!(!holds(None), "nothing installed yet");

        let binary = local_bin.join("existing-agent-test-cli");
        std::fs::create_dir_all(&local_bin).expect("bin dir");
        std::fs::write(&binary, "#!/bin/sh\necho 1.0.0\n").expect("binary");
        std::fs::set_permissions(&binary, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .expect("chmod");
        assert!(
            !holds(None),
            "a binary acp-stack did not install fails the default replacement"
        );
        assert!(!holds(Some(ExistingAgentArg::ReplaceLatest)));
        assert!(!holds(Some(ExistingAgentArg::ReplaceVersion)));
        assert!(
            holds(Some(ExistingAgentArg::UseExisting)),
            "use-existing is satisfied by the binary it keeps"
        );

        let artifact = InstalledArtifact::of(&binary).expect("artifact");
        let path = binary.display().to_string();
        store
            .append_installer_run(InstallerRunInput {
                agent_id: "npm-cli",
                started_at: "2026-09-26T00:00:00.000000000Z",
                finished_at: Some("2026-09-26T00:00:01.000000000Z"),
                status: "ran",
                stdout: "",
                stderr: "",
                exit_status: Some(0),
                step: "install",
                version: Some("1.0.0"),
                operation: INSTALLER_OPERATION_INSTALL,
                method: Some("npm"),
                log_dir: None,
                apply_run_id: None,
                path: Some(&path),
                sha256: Some(&artifact.sha256),
            })
            .expect("row");
        assert!(
            holds(None),
            "a binary acp-stack installed passes every choice"
        );
        assert!(holds(Some(ExistingAgentArg::ReplaceLatest)));
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
            || Duration::ZERO,
        );
        assert!(result.is_err());
        assert_eq!(attempts.get(), 1, "should fail on the first attempt");
    }

    #[test]
    fn spawn_gate_failure_is_not_retried() {
        let error = StackError::AgentInstallerBinaryUnrunnable {
            path: PathBuf::from("/tmp/stub-agent"),
            source: std::io::Error::new(std::io::ErrorKind::InvalidData, "no executable header"),
        };
        assert!(
            !install_error_is_retryable(&error),
            "an unrunnable binary fails identically on every attempt",
        );
    }

    #[test]
    fn backoff_grows_then_caps() {
        assert_eq!(install_retry_backoff(1), Duration::from_secs(2));
        assert_eq!(install_retry_backoff(2), Duration::from_secs(4));
        assert_eq!(install_retry_backoff(4), Duration::from_secs(16));
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
            || Duration::ZERO,
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
            || Duration::ZERO,
        );
        assert!(result.is_err());
        assert_eq!(attempts.get(), MAX_INSTALL_ATTEMPTS);
    }

    #[test]
    fn retry_stops_at_total_budget() {
        let attempts = Cell::new(0u32);
        // Each attempt "costs" the full installer timeout, so the budget stops the loop before the attempt cap.
        let result = run_install_with_retry(
            |attempt| {
                attempts.set(attempt);
                Err::<InstallerOutcome, _>(fake_error())
            },
            |_, _, _| {},
            || INSTALL_RETRY_TOTAL_BUDGET * attempts.get(),
        );
        assert!(result.is_err());
        assert_eq!(
            attempts.get(),
            1,
            "budget exhausted after the first attempt"
        );
    }
}
