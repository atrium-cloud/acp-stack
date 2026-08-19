//! Narrow `acps deps apply` runner.
//!
//! Phase 4 / Dependency Apply: lets operators run the install snippet
//! they declared per-dependency in `[dependencies.commands.install]`,
//! captures stdout/stderr/exit, verifies a `creates` postcheck, and
//! persists one `installer_runs` row per action with `step =
//! "deps_apply"` so the audit log is unified with the agent installer.
//!
//! Scope is deliberately narrow per the Phase 4 spec:
//!
//! - Only commands with an explicit `install` block are eligible.
//!   Missing-but-declared deps without an install action surface as
//!   "no install action declared" — the runtime never guesses an
//!   apt/brew/yum invocation.
//! - System-scoped actions (`scope = "system"`) run directly as root,
//!   escalate through passwordless `sudo -n` when the process is
//!   non-root, and refuse (recording `privilege_required`) when
//!   neither applies — the runner never prompts for a password and
//!   never downgrades a system action to user scope.
//! - Caller must confirm before any action runs (`--yes` flag on the
//!   CLI, `confirmation: true` body field on the API).

use std::collections::HashMap;
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use chrono::{SecondsFormat, Utc};
use serde::Serialize;

use crate::config::{Config, DependencyEntry, DependencyInstallScope};
use crate::error::{Result, StackError};
use crate::runtime::dependencies::deps::{DepStatus, check_dependencies};
use crate::runtime::process_runner::{
    CaptureOutcome, NON_INTERACTIVE_ENV, apply_non_interactive_env, detach_into_new_session,
    join_reader_bounded, kill_process_group, wait_with_timeout,
};
use crate::state::{
    INSTALLER_OUTPUT_CAP_BYTES, INSTALLER_STATUS_RUNNING, InstallerRunFinish, InstallerRunInput,
    StateStore, next_deps_apply_run_id,
};

mod escalation;
mod shell;

pub use escalation::*;
pub(crate) use shell::*;

/// Canonical `installer_runs.agent_id` and `installer_runs.step` value the
/// deps-apply runner stamps onto every audit row. Centralized so the health
/// report and CLI status that pivot on this label cannot drift from the
/// writer.
pub const DEPS_APPLY_AGENT_ID: &str = "deps_apply";
pub const DEPS_APPLY_STEP: &str = "deps_apply";

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// One declared command dep filtered through the apply runner. Used to
/// drive the confirmation prompt + per-row outcome reporting.
#[derive(Debug, Clone, Serialize)]
pub struct DepApplyCandidate {
    pub name: String,
    pub scope: DependencyInstallScope,
    pub shell: String,
    pub creates: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct DepApplyResult {
    pub name: String,
    pub outcome: DepApplyOutcome,
    /// Status of the dep's `creates` binary AFTER the action ran.
    /// `available = true` confirms the action actually installed the
    /// thing it claimed to install.
    pub post_status: DepStatus,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "lowercase", tag = "kind")]
pub enum DepApplyOutcome {
    /// Action ran and the `creates` postcheck resolved.
    Installed,
    /// `creates` already resolved before the action ran; the action
    /// was skipped entirely. Mirrors the agent installer's
    /// "already_present" semantics.
    AlreadyPresent,
    /// Action declared `scope = "system"` but the process isn't root
    /// and passwordless sudo is unavailable. No subprocess was spawned.
    PrivilegeRequired { uid: u32 },
    /// Action ran but `creates` did not resolve afterwards, OR the
    /// action exited non-zero. Tail of stderr included for context.
    Failed {
        exit_code: Option<i32>,
        stderr_tail: String,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct DepsApplyReport {
    pub apply_run_id: String,
    pub before: Vec<DepStatus>,
    pub after: Vec<DepStatus>,
    pub results: Vec<DepApplyResult>,
}

/// Filter declared command deps down to those that:
/// 1. Have an explicit `install` block.
/// 2. Match the optional `feature` filter (any when `None`).
pub fn candidates_for(config: &Config, feature: Option<&str>) -> Vec<DepApplyCandidate> {
    config
        .dependencies
        .commands
        .iter()
        .filter_map(|entry| {
            let install = entry.install.as_ref()?;
            if let Some(filter) = feature
                && entry.feature.as_deref() != Some(filter)
            {
                return None;
            }
            Some(DepApplyCandidate {
                name: entry.name.clone(),
                scope: install.scope,
                shell: install.shell.clone(),
                creates: install
                    .creates
                    .clone()
                    .unwrap_or_else(|| entry.name.clone()),
            })
        })
        .collect()
}

/// Candidates whose install action is still actionable — the `creates` target
/// does not yet resolve. Init's deps-apply step uses this to decide whether
/// there is anything to apply and to skip cleanly when everything is present.
pub fn pending_candidates(config: &Config, feature: Option<&str>) -> Vec<DepApplyCandidate> {
    candidates_for(config, feature)
        .into_iter()
        .filter(|candidate| resolve_command(&candidate.creates).is_none())
        .collect()
}

/// Pending candidates whose install action declares `scope = "system"`.
/// Drives the escalation probe, the preflight notice, and the skip warning.
pub fn pending_system_candidates(config: &Config, feature: Option<&str>) -> Vec<DepApplyCandidate> {
    pending_candidates(config, feature)
        .into_iter()
        .filter(|candidate| candidate.scope == DependencyInstallScope::System)
        .collect()
}

/// Run every eligible install action and return a structured report
/// containing the before-state, after-state, and per-action outcome.
/// The caller is responsible for confirming with the operator before
/// invoking this — the runner never prompts; it just runs.
pub fn apply_dependencies(
    config: &Config,
    feature: Option<&str>,
    state: Option<&StateStore>,
    shell_program: &str,
) -> Result<DepsApplyReport> {
    let escalation = escalation_for(config, feature);
    apply_dependencies_with_escalation(
        config,
        feature,
        state,
        shell_program,
        &escalation,
        |_, _, _| Ok(()),
    )
}

/// Explicit-escalation entry point. Callers that already probed for their
/// confirmation prompt (init's preflight, `acps deps apply`) pass the
/// decision in so one invocation performs exactly one probe; it is also
/// the seam that lets tests inject a fake escalation.
pub fn apply_dependencies_with_escalation(
    config: &Config,
    feature: Option<&str>,
    state: Option<&StateStore>,
    shell_program: &str,
    escalation: &PrivilegeEscalation,
    mut progress: impl FnMut(usize, usize, &str) -> Result<()>,
) -> Result<DepsApplyReport> {
    // before/after must honor each dep's `install.creates` (which may
    // be an absolute path), not just PATH on `entry.name`. The plain
    // `check_dependencies` checker resolves `entry.name`, so an
    // install action whose `creates = "/opt/foo/bin/agent"` would
    // succeed but `report.after` would still say "missing". Compose
    // per-entry `check_one` for command deps with `install` blocks
    // and fall through to the standard checker for everything else.
    let before = compute_before_after_report(config);
    let mut results = Vec::new();
    let apply_run_id = next_deps_apply_run_id();
    let actions: Vec<_> = config
        .dependencies
        .commands
        .iter()
        .filter_map(|entry| {
            let install = entry.install.as_ref()?;
            if let Some(filter) = feature
                && entry.feature.as_deref() != Some(filter)
            {
                return None;
            }
            Some((entry, install))
        })
        .collect();
    let total = actions.len();
    for (index, (entry, install)) in actions.into_iter().enumerate() {
        progress(index + 1, total, &entry.name)?;
        results.push(apply_one(
            entry,
            install,
            state,
            shell_program,
            &apply_run_id,
            escalation,
        )?);
    }
    let after = compute_before_after_report(config);
    Ok(DepsApplyReport {
        apply_run_id,
        before,
        after,
        results,
    })
}

/// Per-dep status that uses `check_one` for command deps with an
/// `install` block (so absolute `creates` paths resolve) and the
/// default checker for everything else (packages, runtimes, MCP, and
/// command deps without an install action).
fn compute_before_after_report(config: &Config) -> Vec<DepStatus> {
    let mut report = check_dependencies(config).dependencies;
    for entry in &config.dependencies.commands {
        if entry.install.is_none() {
            continue;
        }
        if let Some(existing) = report.iter_mut().find(|s| s.name == entry.name) {
            *existing = check_one(entry);
        }
    }
    report
}

/// Persist one `installer_runs` audit row for a deps-apply action. Every
/// row this module writes shares the same agent/step/operation/method
/// provenance, so only the per-outcome fields are parameters. A `None`
/// store means the caller runs without state (CLI dry paths, tests).
#[allow(clippy::too_many_arguments)]
fn append_deps_run(
    state: Option<&StateStore>,
    apply_run_id: &str,
    started_at: &str,
    finished_at: &str,
    status: &str,
    stdout: &str,
    stderr: &str,
    exit_status: Option<i32>,
) -> Result<()> {
    let Some(store) = state else {
        return Ok(());
    };
    store.append_installer_run(InstallerRunInput {
        agent_id: DEPS_APPLY_AGENT_ID,
        started_at,
        finished_at: Some(finished_at),
        status,
        stdout,
        stderr,
        exit_status,
        step: DEPS_APPLY_STEP,
        version: None,
        operation: crate::state::INSTALLER_OPERATION_INSTALL,
        method: Some(crate::state::INSTALLER_METHOD_SHELL),
        log_dir: None,
        apply_run_id: Some(apply_run_id),
    })?;
    Ok(())
}

/// Insert the `running` row for a deps-apply action whose shell is about to
/// spawn; the row is finalized in place by `finish_deps_run` so an in-flight
/// action is visible to concurrent readers. Returns the row id, or `None`
/// when running without state.
fn begin_deps_run(
    state: Option<&StateStore>,
    apply_run_id: &str,
    started_at: &str,
) -> Result<Option<String>> {
    let Some(store) = state else {
        return Ok(None);
    };
    let run = store.append_installer_run(InstallerRunInput {
        agent_id: DEPS_APPLY_AGENT_ID,
        started_at,
        finished_at: None,
        status: INSTALLER_STATUS_RUNNING,
        stdout: "",
        stderr: "",
        exit_status: None,
        step: DEPS_APPLY_STEP,
        version: None,
        operation: crate::state::INSTALLER_OPERATION_INSTALL,
        method: Some(crate::state::INSTALLER_METHOD_SHELL),
        log_dir: None,
        apply_run_id: Some(apply_run_id),
    })?;
    Ok(Some(run.id))
}

/// Finalize the `running` row for a finished deps-apply action. Without a
/// store there is no row to update and nothing to do.
#[allow(clippy::too_many_arguments)]
fn finish_deps_run(
    state: Option<&StateStore>,
    run_id: &str,
    started_at: &str,
    finished_at: &str,
    status: &str,
    stdout: &str,
    stderr: &str,
    exit_status: Option<i32>,
) -> Result<()> {
    let Some(store) = state else {
        return Ok(());
    };
    store.finish_installer_run(
        run_id,
        InstallerRunFinish {
            started_at,
            finished_at: Some(finished_at),
            status,
            stdout,
            stderr,
            exit_status,
            version: None,
            log_dir: None,
        },
    )?;
    Ok(())
}

fn apply_one(
    entry: &DependencyEntry,
    install: &crate::config::DependencyInstallAction,
    state: Option<&StateStore>,
    shell_program: &str,
    apply_run_id: &str,
    escalation: &PrivilegeEscalation,
) -> Result<DepApplyResult> {
    let creates = install
        .creates
        .clone()
        .unwrap_or_else(|| entry.name.clone());
    let started_at = current_timestamp();

    // Idempotence shortcut: if `creates` already resolves, skip the
    // shell entirely. Same contract as the agent installer's
    // `already_present` outcome — re-running `acps deps apply` after
    // a successful run is a no-op.
    if let Some(_path) = resolve_command(&creates) {
        let finished_at = current_timestamp();
        append_deps_run(
            state,
            apply_run_id,
            &started_at,
            &finished_at,
            "skipped",
            "",
            "",
            Some(0),
        )?;
        let post_status = check_one(entry);
        return Ok(DepApplyResult {
            name: entry.name.clone(),
            outcome: DepApplyOutcome::AlreadyPresent,
            post_status,
        });
    }

    // Re-derive the decision at the execution point: `NotNeeded` also
    // stands in for "nothing was pending at probe time, so no probe ran".
    // If a system-scope action became pending between probe and apply
    // (operator think-time at the confirm prompt, or an earlier action
    // changing PATH), trusting the stale value would run a root-intended
    // script as the unprivileged user — the downgrade this module promises
    // never happens. A non-root euid under `NotNeeded` therefore refuses.
    let effective_escalation = match escalation {
        PrivilegeEscalation::NotNeeded => {
            let uid = current_uid();
            if uid == 0 {
                PrivilegeEscalation::NotNeeded
            } else {
                PrivilegeEscalation::Unavailable { uid }
            }
        }
        other => other.clone(),
    };
    let sudo = if install.scope == DependencyInstallScope::System {
        match &effective_escalation {
            PrivilegeEscalation::NotNeeded => None,
            PrivilegeEscalation::Sudo { sudo_path, .. } => Some(sudo_path.as_path()),
            PrivilegeEscalation::Unavailable { uid } => {
                let uid = *uid;
                let finished_at = current_timestamp();
                let candidate = DepApplyCandidate {
                    name: entry.name.clone(),
                    scope: install.scope,
                    shell: install.shell.clone(),
                    creates: creates.clone(),
                };
                let stderr_message = format!(
                    "dep `{name}` declares scope=system but the runtime is uid={uid} and passwordless sudo is unavailable (`sudo -n true` failed); run it manually: {manual}",
                    name = entry.name,
                    manual = manual_privileged_command(shell_program, &candidate),
                );
                append_deps_run(
                    state,
                    apply_run_id,
                    &started_at,
                    &finished_at,
                    "privilege_required",
                    "",
                    &cap_stream(&stderr_message),
                    None,
                )?;
                let post_status = check_one(entry);
                return Ok(DepApplyResult {
                    name: entry.name.clone(),
                    outcome: DepApplyOutcome::PrivilegeRequired { uid },
                    post_status,
                });
            }
        }
    } else {
        None
    };

    let timeout = install
        .timeout_secs
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_TIMEOUT);
    // The running row makes the in-flight action visible to concurrent
    // readers; it is finalized in place below once the shell exits.
    let running_run_id = begin_deps_run(state, apply_run_id, &started_at)?;
    let shell_result = run_shell(shell_program, &install.shell, timeout, sudo);
    let (exit_code, stdout, stderr, timed_out, stderr_tail) = match shell_result {
        Ok(captured) => captured,
        Err(error) => {
            // A spawn/IO failure must not leave the row reading as in-flight
            // forever: finalize it as `error` before propagating. A failed
            // finalize is warn-logged — the original error is the one the
            // caller needs.
            if let Some(run_id) = &running_run_id
                && let Err(finish_error) = finish_deps_run(
                    state,
                    run_id,
                    &started_at,
                    &current_timestamp(),
                    "error",
                    "",
                    &cap_stream(&error.to_string()),
                    None,
                )
            {
                tracing::warn!(error = %finish_error, run_id, "deps apply: failed to finalize running row after shell error");
            }
            return Err(error);
        }
    };
    let finished_at = current_timestamp();

    let post_status = check_one(entry);
    let outcome = if timed_out {
        DepApplyOutcome::Failed {
            exit_code: None,
            stderr_tail: stderr_tail.clone(),
        }
    } else if exit_code != Some(0) {
        DepApplyOutcome::Failed {
            exit_code,
            stderr_tail: stderr_tail.clone(),
        }
    } else if !post_status.available {
        DepApplyOutcome::Failed {
            exit_code,
            stderr_tail: format!(
                "shell exited 0 but `creates = {creates}` did not resolve on PATH",
            ),
        }
    } else {
        DepApplyOutcome::Installed
    };

    let status_label = match &outcome {
        DepApplyOutcome::Installed => "installed",
        DepApplyOutcome::AlreadyPresent => "skipped",
        DepApplyOutcome::PrivilegeRequired { .. } => "privilege_required",
        DepApplyOutcome::Failed { .. } => "failed",
    };
    // Timed-out runs use `ExitStatus::default()` (success on
    // every platform) because we never observed a real exit
    // code from the killed process. Persisting `status.code()`
    // for that case would let `acps installer history` show a
    // failed timeout row with `exit_status = 0`, contradicting
    // the operator-facing outcome which reports timeout as
    // `exit_code: None`. Match the outcome contract instead.
    let persisted_exit = if timed_out { None } else { exit_code };
    let persisted_stdout = if sudo.is_some() {
        cap_stream(&format!("{ESCALATED_STDOUT_MARKER}\n{stdout}"))
    } else {
        cap_stream(&stdout)
    };
    match running_run_id {
        Some(run_id) => {
            if let Err(error) = finish_deps_run(
                state,
                &run_id,
                &started_at,
                &finished_at,
                status_label,
                &persisted_stdout,
                &cap_stream(&stderr),
                persisted_exit,
            ) {
                // A failed finalize must neither abort the remaining actions
                // nor leave the row reading as in-flight: warn-log it and
                // mark the row `error` best-effort (mirrors the agent
                // installer's finalize_tracked_step). The step's own outcome
                // still surfaces in the report below.
                tracing::warn!(%error, run_id, dep = %entry.name, "deps apply: failed to finalize running row");
                let reason = format!("deps apply finalize failed: {error}");
                if let Err(mark_error) = finish_deps_run(
                    state,
                    &run_id,
                    &started_at,
                    &finished_at,
                    "error",
                    "",
                    &cap_stream(&reason),
                    persisted_exit,
                ) {
                    tracing::warn!(error = %mark_error, run_id, "deps apply: failed to mark unfinalizable row as error");
                }
            }
        }
        // No store handle (dry paths, tests): nothing was inserted at start.
        None => append_deps_run(
            state,
            apply_run_id,
            &started_at,
            &finished_at,
            status_label,
            &persisted_stdout,
            &cap_stream(&stderr),
            persisted_exit,
        )?,
    }
    Ok(DepApplyResult {
        name: entry.name.clone(),
        outcome,
        post_status,
    })
}

/// Per-entry availability check. When the install action declares a
/// `creates` path, we resolve THAT path (which may be absolute) rather
/// than re-PATH-looking up `entry.name`. Without this, a dep whose
/// install action drops a binary outside `$PATH` (e.g. an absolute
/// `creates = "/opt/foo/bin/agent"`) would be reported as missing
/// after a perfectly successful install.
fn check_one(entry: &DependencyEntry) -> DepStatus {
    let creates = entry
        .install
        .as_ref()
        .and_then(|i| i.creates.clone())
        .unwrap_or_else(|| entry.name.clone());
    match resolve_command(&creates) {
        Some(path) => DepStatus {
            name: entry.name.clone(),
            kind: crate::runtime::dependencies::deps::DepKind::Command,
            required: entry.required,
            available: true,
            path: Some(path.to_string_lossy().into_owned()),
            feature: entry.feature.clone(),
            reason: None,
        },
        None => DepStatus {
            name: entry.name.clone(),
            kind: crate::runtime::dependencies::deps::DepKind::Command,
            required: entry.required,
            available: false,
            path: None,
            feature: entry.feature.clone(),
            reason: Some(format!("`{creates}` not found on PATH")),
        },
    }
}

fn current_timestamp() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true)
}

pub fn candidate_summary_line(candidate: &DepApplyCandidate) -> String {
    let scope = match candidate.scope {
        DependencyInstallScope::User => "user",
        DependencyInstallScope::System => "system",
    };
    // Surface the literal shell snippet alongside the metadata. A
    // confirmation prompt that hides the command being approved is
    // a footgun — the operator needs to see exactly what will run.
    // Long snippets are shown verbatim; truncating them would
    // re-introduce the same hidden-blob problem.
    let mut buf = String::new();
    write!(
        &mut buf,
        "{name} (scope={scope}, creates={creates})\n      shell: {shell}",
        name = candidate.name,
        scope = scope,
        creates = candidate.creates,
        shell = candidate.shell,
    )
    .expect("write to String");
    buf
}

/// Render an operator-facing one-line summary for one candidate. Used
/// by both the CLI confirmation prompt and the API audit message.
#[cfg(test)]
mod tests;
