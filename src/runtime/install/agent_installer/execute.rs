//! Registry-resolved install execution.
//!
//! The parent module owns orchestration and persistence; this module drives a
//! single resolved registry entry to completion: sequencing harness and adapter
//! steps (in parallel when both are declared), walking each field's
//! `shell → npm → github_release` fallback chain, and skipping paths whose
//! prerequisite tools are absent. Every attempt produces a row so the fallback
//! chain stays visible in `acps installer history`.

use super::*;

/// Run the resolved-registry installer WITHOUT holding the state store across
/// steps. Returns all rows that should be persisted (in order) and the final
/// outcome. When `progress` is provided, each executed step is inserted as a
/// `running` row at start and finalized in place at finish (its draft then
/// carries `persisted_run_id` so the caller skips re-appending it).
pub fn install_resolved_capture(
    agent: &AgentConfig,
    entry: &RegistryEntry,
    _agent_env: HashMap<String, String>,
    workspace_root: &Path,
    dest_dir: &Path,
    progress: Option<&InstallProgress<'_>>,
) -> InstallerSequenceResult {
    let mut rows = Vec::new();
    let installer_env = HashMap::new();
    // A declared sha256 pin downgrades step-level spawn gates to the header
    // check (which never executes the file); `final_verification` then owns
    // the pin check followed by the probe.
    let pin_declared = agent.expected_sha256.is_some();
    if let Err(err) = entry.ensure_supported() {
        return InstallerSequenceResult {
            outcome: Err(err),
            rows,
        };
    }

    // Step 1: install the upstream agent harness. Native entries speak ACP from
    // this binary; most adapter-backed entries wrap it with an adapter in step 2.
    let harness = match entry.harness.as_ref() {
        Some(h) => h,
        None => {
            // The registry validator should have caught this; fail-fast with a
            // typed error if it didn't.
            return InstallerSequenceResult {
                outcome: Err(StackError::RegistryLoad {
                    reason: format!("registry entry `{}` has no harness block", entry.id),
                }),
                rows,
            };
        }
    };
    let harness_step_label = if entry.kind == RegistryKind::Adapter {
        STEP_HARNESS
    } else {
        STEP_INSTALL
    };
    if entry.kind == RegistryKind::Adapter {
        let adapter = match entry.adapter.as_ref() {
            Some(adapter) => adapter,
            None => {
                return InstallerSequenceResult {
                    outcome: Err(StackError::RegistryLoad {
                        reason: format!("registry entry `{}` has no adapter block", entry.id),
                    }),
                    rows,
                };
            }
        };

        if harness.install.is_provided_by_adapter() {
            let adapter_chain = install_one_with_fallback(
                &entry.id,
                "adapter.install",
                STEP_ADAPTER,
                &adapter.install,
                adapter.github.as_deref(),
                None,
                &installer_env,
                workspace_root,
                dest_dir,
                pin_declared,
                progress,
            );
            rows.extend(adapter_chain.rows);
            if let Some(err) = adapter_chain.terminal_error {
                return InstallerSequenceResult {
                    outcome: Err(err),
                    rows,
                };
            }

            return final_verification(agent, workspace_root, dest_dir, rows);
        }

        // Harness + adapter install in parallel. Each side tries its
        // priority chain (shell → npm → github_release for floating,
        // github → npm for pinned) internally so a single broken path
        // doesn't abort the install when a sibling would have worked.
        let harness_workspace = workspace_root.to_path_buf();
        let harness_dest = dest_dir.to_path_buf();
        let harness_env = installer_env.clone();
        let harness_install = harness.install.clone();
        let harness_github = entry.github.clone();
        let harness_version = agent.harness_version.clone();
        let harness_id = entry.id.clone();
        let adapter_workspace = workspace_root.to_path_buf();
        let adapter_dest = dest_dir.to_path_buf();
        let adapter_env = installer_env.clone();
        let adapter_install = adapter.install.clone();
        let adapter_github = adapter.github.clone();
        let adapter_id = entry.id.clone();
        // Scoped threads (not `thread::spawn`) so the borrowed `progress`
        // sink can cross; both handles are joined manually inside the scope,
        // so a panicking installer thread still lands in the `unwrap_or_else`
        // fallback instead of propagating at scope exit. The step guard in
        // `install_one_with_fallback` finalizes the panic's `running` row
        // before the unwind reaches the join.
        let (harness_chain, adapter_chain) = std::thread::scope(|scope| {
            let harness_thread = scope.spawn(move || {
                install_one_with_fallback(
                    &harness_id,
                    "harness.install",
                    STEP_HARNESS,
                    &harness_install,
                    harness_github.as_deref(),
                    harness_version.as_deref(),
                    &harness_env,
                    &harness_workspace,
                    &harness_dest,
                    pin_declared,
                    progress,
                )
            });
            let adapter_thread = scope.spawn(move || {
                install_one_with_fallback(
                    &adapter_id,
                    "adapter.install",
                    STEP_ADAPTER,
                    &adapter_install,
                    adapter_github.as_deref(),
                    None,
                    &adapter_env,
                    &adapter_workspace,
                    &adapter_dest,
                    pin_declared,
                    progress,
                )
            });
            let harness_chain = harness_thread.join().unwrap_or_else(|_| FallbackChain {
                rows: vec![InstallerRowDraft::config_error(STEP_HARNESS)],
                terminal_error: Some(StackError::AgentInitializeFailed {
                    reason: "harness installer thread panicked".to_owned(),
                }),
            });
            let adapter_chain = adapter_thread.join().unwrap_or_else(|_| FallbackChain {
                rows: vec![InstallerRowDraft::config_error(STEP_ADAPTER)],
                terminal_error: Some(StackError::AgentInitializeFailed {
                    reason: "adapter installer thread panicked".to_owned(),
                }),
            });
            (harness_chain, adapter_chain)
        });
        rows.extend(harness_chain.rows);
        rows.extend(adapter_chain.rows);
        if let Some(err) = harness_chain.terminal_error {
            return InstallerSequenceResult {
                outcome: Err(err),
                rows,
            };
        }
        if let Some(err) = adapter_chain.terminal_error {
            return InstallerSequenceResult {
                outcome: Err(err),
                rows,
            };
        }

        return final_verification(agent, workspace_root, dest_dir, rows);
    }

    let chain = install_one_with_fallback(
        &entry.id,
        "harness.install",
        harness_step_label,
        &harness.install,
        entry.github.as_deref(),
        agent.harness_version.as_deref(),
        &installer_env,
        workspace_root,
        dest_dir,
        pin_declared,
        progress,
    );
    rows.extend(chain.rows);
    if let Some(err) = chain.terminal_error {
        return InstallerSequenceResult {
            outcome: Err(err),
            rows,
        };
    }

    final_verification(agent, workspace_root, dest_dir, rows)
}

/// Result of walking the `[shell, npm, github]` chain for one install
/// field. `rows` contains the per-attempt `installer_runs` draft (so
/// every attempt is preserved for audit, not just the winner);
/// `terminal_error` is `None` when any path succeeded. When several
/// paths were attempted or skipped, the terminal error enumerates each
/// path's failure so no single path's error masks the others.
pub(crate) struct FallbackChain {
    pub(crate) rows: Vec<InstallerRowDraft>,
    pub(crate) terminal_error: Option<StackError>,
}

/// Fold the per-path outcomes into the single error surfaced to the
/// operator. A lone attempt keeps its typed error unchanged; multiple
/// entries collapse into `AgentInstallAllPathsFailed` listing each
/// path's failure (`shell: …; npm: …; github: …`), including paths
/// that were skipped for missing prerequisite tools.
fn terminal_error_from(
    attempts: &[(&'static str, String)],
    last_error: Option<StackError>,
) -> Option<StackError> {
    if attempts.len() <= 1
        && let Some(error) = last_error
    {
        return Some(error);
    }
    if attempts.is_empty() {
        return None;
    }
    // Recorded attempts with no typed error still must not read as
    // success — fold whatever was recorded into the enumerated error.
    let summary = attempts
        .iter()
        .map(|(path, error)| format!("{path}: {error}"))
        .collect::<Vec<_>>()
        .join("; ");
    Some(StackError::AgentInstallAllPathsFailed { summary })
}

/// Try each install path declared on the given field in priority order
/// (shell → npm → github_release for floating versions; github → npm for
/// pinned). Returns once one succeeds, or once all declared paths have
/// been exhausted. Each attempt is recorded so the operator can see the
/// fallback chain after the fact via `acps installer history`. When
/// `progress` is provided, every executed attempt is also visible
/// in-flight: a `running` row is inserted before the step spawns and
/// finalized in place when it exits.
#[allow(clippy::too_many_arguments)]
pub(crate) fn install_one_with_fallback(
    agent_id: &str,
    field: &str,
    step_label: &'static str,
    install: &InstallSet,
    github_url: Option<&str>,
    version_pin: Option<&str>,
    env: &HashMap<String, String>,
    workspace_root: &Path,
    dest_dir: &Path,
    pin_declared: bool,
    progress: Option<&InstallProgress<'_>>,
) -> FallbackChain {
    let mut remaining = install.clone();
    let mut rows = Vec::new();
    let mut last_error: Option<StackError> = None;
    let mut missing_tools = BTreeSet::new();
    let mut attempts: Vec<(&'static str, String)> = Vec::new();
    loop {
        let spec = match step_runners::select_install_path(
            agent_id,
            field,
            &remaining,
            github_url,
            version_pin,
        ) {
            Ok(spec) => spec,
            Err(err) => {
                if rows.is_empty() {
                    // No path was ever runnable. Surface that as the
                    // single registry error with a placeholder row so
                    // the audit log records the attempt.
                    rows.push(InstallerRowDraft::config_error(step_label));
                    return FallbackChain {
                        rows,
                        terminal_error: Some(err),
                    };
                }
                // `rows` non-empty implies a step ran and failed, so
                // `last_error` is Some here; the `else` arm is defensive
                // so a select error can never be silently dropped.
                let terminal_error = if last_error.is_some() {
                    terminal_error_from(&attempts, last_error)
                } else {
                    Some(err)
                };
                return FallbackChain {
                    rows,
                    terminal_error,
                };
            }
        };
        let kind = path_kind_of(&spec);
        let missing_for_path = missing_required_tools(&spec, workspace_root, dest_dir);
        if !missing_for_path.is_empty() {
            attempts.push((
                path_label_of(kind),
                format!("skipped, missing tools: {}", missing_for_path.join(", ")),
            ));
            for tool in missing_for_path {
                missing_tools.insert(tool);
            }
            match kind {
                InstallPathKind::Shell => remaining.shell = None,
                InstallPathKind::Npm => remaining.npm = None,
                InstallPathKind::Github => remaining.github = None,
            }
            if remaining.shell.is_none() && remaining.npm.is_none() && remaining.github.is_none() {
                return exhausted_after_missing_prerequisites(
                    agent_id,
                    field,
                    step_label,
                    rows,
                    &attempts,
                    last_error,
                    missing_tools,
                );
            }
            continue;
        }
        let step = run_guarded_install_step(step_label, path_label_of(kind), progress, || {
            run_install_step(
                step_label,
                spec,
                env,
                workspace_root,
                dest_dir,
                pin_declared,
            )
        });
        rows.push(step.row);
        match step.outcome {
            Ok(_) => {
                return FallbackChain {
                    rows,
                    terminal_error: None,
                };
            }
            Err(err) => {
                // `public_message` reads better in the enumerated summary
                // than raw Display (e.g. `status 9`, not `status Some(9)`).
                attempts.push((path_label_of(kind), err.public_message()));
                last_error = Some(err);
                // Drop the path we just exhausted so the next select
                // resolves a different one.
                match kind {
                    InstallPathKind::Shell => remaining.shell = None,
                    InstallPathKind::Npm => remaining.npm = None,
                    InstallPathKind::Github => remaining.github = None,
                }
            }
        }
        if remaining.shell.is_none() && remaining.npm.is_none() && remaining.github.is_none() {
            return FallbackChain {
                rows,
                terminal_error: terminal_error_from(&attempts, last_error),
            };
        }
    }
}

/// Run one resolved step behind a panic guard. A panicking step would
/// otherwise strand its `running` row: the thread-join fallback at the call
/// site only sees the dead thread, not the row id, and the active-runs query
/// has no age cutoff, so the row would read as in-flight forever even though
/// the daemon survived. The guard finalizes the row as `error` — associating
/// it with the panic — then resumes the unwind so the join fallback still
/// records the panic as the install's terminal error.
pub(super) fn run_guarded_install_step(
    step_label: &'static str,
    method: &'static str,
    progress: Option<&InstallProgress<'_>>,
    run: impl FnOnce() -> StepResult,
) -> StepResult {
    let run_id =
        progress.and_then(|progress| begin_tracked_step(progress, step_label, Some(method)));
    let step = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)) {
        Ok(step) => step,
        Err(payload) => {
            let mut row = InstallerRowDraft {
                started_at: current_timestamp(),
                finished_at: Some(current_timestamp()),
                status: "error".to_owned(),
                stdout: String::new(),
                stderr: format!("installer step `{step_label}` panicked"),
                exit_status: None,
                step: step_label.to_owned(),
                method: Some(method.to_owned()),
                version: None,
                log_dir: None,
                persisted_run_id: None,
            };
            if let Some(progress) = progress {
                finalize_tracked_step(progress, run_id, &mut row);
            }
            std::panic::resume_unwind(payload);
        }
    };
    let mut step = step;
    if let Some(progress) = progress {
        finalize_tracked_step(progress, run_id, &mut step.row);
    }
    step
}

pub(super) fn exhausted_after_missing_prerequisites(
    agent_id: &str,
    field: &str,
    step_label: &'static str,
    mut rows: Vec<InstallerRowDraft>,
    attempts: &[(&'static str, String)],
    last_error: Option<StackError>,
    missing_tools: BTreeSet<String>,
) -> FallbackChain {
    if !rows.is_empty() {
        return FallbackChain {
            rows,
            terminal_error: terminal_error_from(attempts, last_error),
        };
    }
    rows.push(InstallerRowDraft::config_error(step_label));
    FallbackChain {
        rows,
        terminal_error: Some(StackError::AgentInstallerPrerequisitesMissing {
            agent_id: agent_id.to_owned(),
            step: field.to_owned(),
            tools: missing_tools.into_iter().collect(),
        }),
    }
}

#[derive(Debug, Clone, Copy)]
enum InstallPathKind {
    Shell,
    Npm,
    Github,
}

fn path_label_of(kind: InstallPathKind) -> &'static str {
    match kind {
        InstallPathKind::Shell => INSTALL_METHOD_SHELL,
        InstallPathKind::Npm => INSTALL_METHOD_NPM,
        InstallPathKind::Github => INSTALL_METHOD_GITHUB,
    }
}

fn path_kind_of(spec: &ResolvedInstallSpec) -> InstallPathKind {
    match spec {
        ResolvedInstallSpec::Shell { .. } => InstallPathKind::Shell,
        ResolvedInstallSpec::Npm { .. } => InstallPathKind::Npm,
        ResolvedInstallSpec::GithubRelease { .. } => InstallPathKind::Github,
    }
}

pub(super) fn missing_required_tools(
    spec: &ResolvedInstallSpec,
    workspace_root: &Path,
    dest_dir: &Path,
) -> Vec<String> {
    let required_tools: Vec<&str> = match spec {
        ResolvedInstallSpec::Shell { required_tools, .. } => {
            required_tools.iter().map(String::as_str).collect()
        }
        ResolvedInstallSpec::Npm { .. } => vec!["npm"],
        ResolvedInstallSpec::GithubRelease { .. } => Vec::new(),
    };
    required_tools
        .into_iter()
        .filter(|tool| resolve_creates(tool, workspace_root, &[dest_dir]).is_none())
        .map(str::to_owned)
        .collect()
}
