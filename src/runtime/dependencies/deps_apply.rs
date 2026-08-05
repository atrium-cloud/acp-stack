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
    NON_INTERACTIVE_ENV, STDERR_TAIL_BYTES, apply_non_interactive_env, detach_into_new_session,
    join_reader_bounded, kill_process_group, read_to_cap, read_to_cap_with_tail, wait_with_timeout,
};
use crate::state::{
    INSTALLER_OUTPUT_CAP_BYTES, InstallerRunInput, StateStore, next_deps_apply_run_id,
};

/// Canonical `installer_runs.agent_id` and `installer_runs.step` value the
/// deps-apply runner stamps onto every audit row. Centralized so the health
/// report and CLI status that pivot on this label cannot drift from the
/// writer.
pub const DEPS_APPLY_AGENT_ID: &str = "deps_apply";
pub const DEPS_APPLY_STEP: &str = "deps_apply";

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// `sudo -n` never prompts: it exits non-zero immediately when a password
/// would be required, so neither the probe nor an escalated run can block
/// on stdin or a controlling terminal.
const SUDO_PROGRAM: &str = "sudo";
const SUDO_NON_INTERACTIVE_FLAG: &str = "-n";
/// Upper bound on the `sudo -n true` probe. A healthy probe returns in
/// milliseconds; the bound exists so a wedged sudoers backend (LDAP/SSSD)
/// cannot stall an apply before a single dep has run.
const SUDO_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// Grace period for reaping a killed action. An escalated action runs as
/// root, so a non-root parent's SIGKILL is refused with EPERM; an unbounded
/// `wait()` there would hang the whole apply.
const KILL_REAP_GRACE: Duration = Duration::from_secs(5);
/// Provenance line prepended to the persisted stdout of an escalated
/// action. Keeps `installer_runs.method` stable at `shell` (health and
/// `acps status` pivot on it) while `acps installer history` still shows
/// sudo was used.
const ESCALATED_STDOUT_MARKER: &str = "[acps] escalated via `sudo -n`";
/// Per-stream cap on captured output before we start dropping bytes.
/// Reuses the state-layer constant so a future bump in installer_runs
/// row size automatically applies to deps_apply too.
const STREAM_CAP_BYTES: usize = INSTALLER_OUTPUT_CAP_BYTES;

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

/// How the apply runner reaches root for `scope = "system"` actions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrivilegeEscalation {
    /// euid == 0 — system-scope actions run directly.
    NotNeeded,
    /// euid != 0 but `sudo -n true` succeeded. `sudo_path` is resolved
    /// once at probe time so the probe and the actual run cannot pick
    /// different binaries.
    Sudo { sudo_path: PathBuf, uid: u32 },
    /// euid != 0 and passwordless sudo is unavailable (missing binary,
    /// password required, or probe timeout).
    Unavailable { uid: u32 },
}

impl PrivilegeEscalation {
    pub fn is_available(&self) -> bool {
        !matches!(self, PrivilegeEscalation::Unavailable { .. })
    }

    pub fn uid(&self) -> u32 {
        match self {
            PrivilegeEscalation::NotNeeded => 0,
            PrivilegeEscalation::Sudo { uid, .. } | PrivilegeEscalation::Unavailable { uid } => {
                *uid
            }
        }
    }
}

/// Probe how system-scope actions can reach root. Never returns Err: a
/// missing `sudo`, a password-gated sudoers rule, and a hung probe are all
/// environment facts, not acps failures — they collapse to `Unavailable`.
/// Not cached process-wide: a long-lived daemon must not pin sudoers state
/// across a config change, so callers probe once per apply invocation.
pub fn probe_privilege_escalation() -> PrivilegeEscalation {
    probe_privilege_escalation_with(current_uid(), resolve_command(SUDO_PROGRAM))
}

/// Testable core of [`probe_privilege_escalation`]: uid and resolved sudo
/// path are injected so tests don't have to mutate the process-global PATH
/// (which races with parallel tests spawning shells).
fn probe_privilege_escalation_with(uid: u32, sudo_path: Option<PathBuf>) -> PrivilegeEscalation {
    if uid == 0 {
        return PrivilegeEscalation::NotNeeded;
    }
    let Some(sudo_path) = sudo_path else {
        return PrivilegeEscalation::Unavailable { uid };
    };
    let mut command = Command::new(&sudo_path);
    command
        .arg(SUDO_NON_INTERACTIVE_FLAG)
        .arg("true")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_clear()
        .envs(scrubbed_env());
    apply_non_interactive_env(&mut command);
    detach_into_new_session(&mut command);
    let Ok(mut child) = command.spawn() else {
        return PrivilegeEscalation::Unavailable { uid };
    };
    match wait_with_timeout(&mut child, Instant::now() + SUDO_PROBE_TIMEOUT) {
        Ok(Some(status)) if status.success() => PrivilegeEscalation::Sudo { sudo_path, uid },
        Ok(Some(_)) => PrivilegeEscalation::Unavailable { uid },
        Ok(None) | Err(_) => {
            // Timed out or wait failed — the probe child may still be
            // alive; reap the group so it cannot outlive the probe.
            kill_process_group(&mut child);
            if reap_with_grace(&mut child, KILL_REAP_GRACE).is_none() {
                tracing::warn!(
                    "sudo probe outlived its timeout kill and was abandoned unreaped (pid={})",
                    child.id(),
                );
            }
            PrivilegeEscalation::Unavailable { uid }
        }
    }
}

/// Probe only when a pending system-scope action exists, so a satisfied
/// config never shells out to sudo.
fn escalation_for(config: &Config, feature: Option<&str>) -> PrivilegeEscalation {
    if pending_system_candidates(config, feature).is_empty() {
        PrivilegeEscalation::NotNeeded
    } else {
        probe_privilege_escalation()
    }
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
    let started_instant = Instant::now();

    // Idempotence shortcut: if `creates` already resolves, skip the
    // shell entirely. Same contract as the agent installer's
    // `already_present` outcome — re-running `acps deps apply` after
    // a successful run is a no-op.
    if let Some(_path) = resolve_command(&creates) {
        let finished_at = current_timestamp();
        if let Some(store) = state {
            store.append_installer_run(InstallerRunInput {
                agent_id: DEPS_APPLY_AGENT_ID,
                started_at: &started_at,
                finished_at: Some(&finished_at),
                status: "skipped",
                stdout: "",
                stderr: "",
                exit_status: Some(0),
                step: DEPS_APPLY_STEP,
                version: None,
                operation: crate::state::INSTALLER_OPERATION_INSTALL,
                method: Some(crate::state::INSTALLER_METHOD_SHELL),
                log_dir: None,
                apply_run_id: Some(apply_run_id),
            })?;
        }
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
                if let Some(store) = state {
                    store.append_installer_run(InstallerRunInput {
                        agent_id: DEPS_APPLY_AGENT_ID,
                        started_at: &started_at,
                        finished_at: Some(&finished_at),
                        status: "privilege_required",
                        stdout: "",
                        stderr: &cap_stream(&stderr_message),
                        exit_status: None,
                        step: DEPS_APPLY_STEP,
                        version: None,
                        operation: crate::state::INSTALLER_OPERATION_INSTALL,
                        method: Some(crate::state::INSTALLER_METHOD_SHELL),
                        log_dir: None,
                        apply_run_id: Some(apply_run_id),
                    })?;
                }
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
    let (exit_code, stdout, stderr, timed_out, stderr_tail) =
        run_shell(shell_program, &install.shell, timeout, sudo)?;
    let finished_at = current_timestamp();
    let _elapsed = started_instant.elapsed();

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
    if let Some(store) = state {
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
        store.append_installer_run(InstallerRunInput {
            agent_id: DEPS_APPLY_AGENT_ID,
            started_at: &started_at,
            finished_at: Some(&finished_at),
            status: status_label,
            stdout: &persisted_stdout,
            stderr: &cap_stream(&stderr),
            exit_status: persisted_exit,
            step: DEPS_APPLY_STEP,
            version: None,
            operation: crate::state::INSTALLER_OPERATION_INSTALL,
            method: Some(crate::state::INSTALLER_METHOD_SHELL),
            log_dir: None,
            apply_run_id: Some(apply_run_id),
        })?;
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

/// Return tuple: `(exit_code, stdout, stderr_prefix, timed_out,
/// stderr_tail)` — see `read_to_cap_with_tail` for why `stderr_tail`
/// is computed separately.
fn run_shell(
    shell_program: &str,
    script: &str,
    timeout: Duration,
    sudo: Option<&Path>,
) -> Result<(Option<i32>, String, String, bool, String)> {
    let mut command = match sudo {
        Some(sudo_path) => {
            let mut command = Command::new(sudo_path);
            command
                .arg(SUDO_NON_INTERACTIVE_FLAG)
                .arg(shell_program)
                .arg("-c")
                .arg(escalated_script(script));
            command
        }
        None => {
            let mut command = Command::new(shell_program);
            command.arg("-c").arg(script);
            command
        }
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_clear()
        .envs(scrubbed_env());
    apply_non_interactive_env(&mut command);
    // Detach into a fresh session so a timeout-induced kill reaches every
    // grandchild the shell forked (without this, `child.kill()` only stops
    // the shell — a `sleep 999` it spawned would keep the stdout/stderr
    // pipes open and the join threads would block forever), and so a dep
    // script probing /dev/tty cannot prompt. Same pattern as agent_installer.
    detach_into_new_session(&mut command);
    let mut child = command
        .spawn()
        .map_err(|source| StackError::AgentSpawnFailed { source })?;

    let stdout_handle = child.stdout.take().expect("piped stdout");
    let stderr_handle = child.stderr.take().expect("piped stderr");

    let stdout_thread = std::thread::spawn(move || read_to_cap(stdout_handle, STREAM_CAP_BYTES));
    let stderr_thread = std::thread::spawn(move || {
        read_to_cap_with_tail(stderr_handle, STREAM_CAP_BYTES, STDERR_TAIL_BYTES)
    });

    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    // On escalated runs the child is root-owned, so our
                    // SIGKILL is refused with EPERM and an unbounded
                    // `wait()` would hang the apply; the bounded reap
                    // (plus the bounded reader joins below) keeps the
                    // outcome reported as a timeout Failed either way.
                    kill_process_group(&mut child);
                    timed_out = true;
                    if reap_with_grace(&mut child, KILL_REAP_GRACE).is_none() {
                        // Root-owned (escalated) children ignore our SIGKILL,
                        // so the unreaped child lingers as a zombie until the
                        // process exits. Surface it rather than leak silently.
                        tracing::warn!(
                            "dep install action outlived its timeout kill and was abandoned unreaped (pid={})",
                            child.id(),
                        );
                    }
                    break std::process::ExitStatus::default();
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(err) => {
                return Err(StackError::AgentSpawnFailed { source: err });
            }
        }
    };
    // Always kill the process group, even on a clean shell exit. If
    // the shell forked a background grandchild that inherited
    // stdout/stderr, the reader threads would block forever waiting
    // for EOF on those pipes. Killing the group closes the pipes
    // (the child's std handles get released), so the readers see
    // EOF and the joins below return.
    kill_process_group(&mut child);
    // Bounded join: a double-forked daemon that escaped the process
    // group could still hold our pipe descriptors open. We can't
    // SIGKILL it (we don't have a pid), so we wait `READER_JOIN_GRACE`
    // for the close to land and then abandon the thread if it didn't.
    // Abandoning is fine here — the OS reaps the orphaned thread when
    // `acps` exits, and dropping the captured output is preferable to
    // hanging the entire `deps apply` call.
    let stdout = join_reader_bounded(stdout_thread).unwrap_or_default();
    let (stderr, stderr_tail) =
        join_reader_bounded(stderr_thread).unwrap_or((String::new(), String::new()));
    let exit_code = status.code();
    Ok((exit_code, stdout, stderr, timed_out, stderr_tail))
}

/// sudo resets the environment (`env_reset` in sudoers), so the
/// non-interactive vars set on the child are dropped before the operator's
/// script runs — `apt-get` would go back to prompting. Re-export them inside
/// the escalated shell instead of asking sudoers for `setenv`/`--preserve-env`
/// permission we may not have. Names and values come from the compile-time
/// [`NON_INTERACTIVE_ENV`] table (never operator input), so no quoting is
/// needed; the operator's script is appended verbatim.
fn escalated_script(script: &str) -> String {
    let mut out = String::new();
    for (name, value) in NON_INTERACTIVE_ENV {
        writeln!(&mut out, "export {name}={value}").expect("write to String");
    }
    out.push_str(script);
    out
}

/// Bounded reap after a group kill. A root-owned (escalated) child cannot be
/// signalled by a non-root parent, so a plain `wait()` would block forever.
/// Returns `None` when the child outlives the grace; callers already treat
/// that as a timeout and the pipe-reader joins are separately bounded.
fn reap_with_grace(
    child: &mut std::process::Child,
    grace: Duration,
) -> Option<std::process::ExitStatus> {
    wait_with_timeout(child, Instant::now() + grace)
        .ok()
        .flatten()
}

/// Copy-pasteable command that reproduces exactly what the runner would have
/// run for a system-scope action, for hosts where acps cannot escalate.
pub fn manual_privileged_command(shell_program: &str, candidate: &DepApplyCandidate) -> String {
    format!(
        "sudo {shell_program} -c {script}",
        script = shell_single_quote(&candidate.shell),
    )
}

/// POSIX single-quote escaping: wrap in `'…'` with embedded `'` rendered as
/// `'\''`.
fn shell_single_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Operator-facing escalation notice shared by `acps init` and `acps deps
/// apply` so the two confirmation prompts cannot drift. Empty when no
/// system-scope candidate is pending.
pub fn escalation_notice_lines(
    escalation: &PrivilegeEscalation,
    shell_program: &str,
    system_candidates: &[DepApplyCandidate],
) -> Vec<String> {
    if system_candidates.is_empty() {
        return Vec::new();
    }
    let count = system_candidates.len();
    match escalation {
        PrivilegeEscalation::NotNeeded => vec![format!(
            "note: {count} action(s) declare scope=system; the runtime is root and will run them directly."
        )],
        PrivilegeEscalation::Sudo { uid, .. } => vec![format!(
            "note: {count} action(s) declare scope=system; passwordless sudo is available (uid={uid}), so they run through `sudo -n`."
        )],
        PrivilegeEscalation::Unavailable { uid } => {
            let mut lines = vec![format!(
                "warning: {count} action(s) declare scope=system but this host is uid={uid} with no passwordless sudo; they will be skipped and recorded as privilege_required."
            )];
            for candidate in system_candidates {
                lines.push(format!(
                    "  - {name}: {manual}",
                    name = candidate.name,
                    manual = manual_privileged_command(shell_program, candidate),
                ));
            }
            // The follow-up instruction (resume vs re-run) is
            // caller-specific; init and `acps deps apply` append their own.
            lines
        }
    }
}

fn cap_stream(value: &str) -> String {
    if value.len() <= STREAM_CAP_BYTES {
        return value.to_owned();
    }
    let mut cutoff = STREAM_CAP_BYTES;
    while cutoff > 0 && !value.is_char_boundary(cutoff) {
        cutoff -= 1;
    }
    value[..cutoff].to_owned()
}

fn current_timestamp() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn current_uid() -> u32 {
    // SAFETY: `geteuid()` is always safe — no preconditions.
    unsafe { libc::geteuid() }
}

fn scrubbed_env() -> HashMap<String, String> {
    let mut env = HashMap::new();
    if let Ok(value) = std::env::var("PATH") {
        env.insert("PATH".to_owned(), value);
    }
    if let Ok(value) = std::env::var("HOME") {
        env.insert("HOME".to_owned(), value);
    }
    if let Ok(value) = std::env::var("LANG") {
        env.insert("LANG".to_owned(), value);
    }
    env
}

fn resolve_command(name: &str) -> Option<std::path::PathBuf> {
    if name.contains('/') {
        let path = Path::new(name).to_path_buf();
        return is_executable_file(&path).then_some(path);
    }
    let path_env = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_env) {
        let candidate = dir.join(name);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// True when `path` is a regular file that has at least one execute
/// bit set on Unix. A failed `chmod` after an `install` action would
/// otherwise let the postcheck report success against a non-executable
/// placeholder. On non-Unix targets, fall back to `is_file()` since
/// there's no mode bit semantic.
fn is_executable_file(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match std::fs::metadata(path) {
            Ok(meta) => (meta.mode() & 0o111) != 0,
            Err(_) => false,
        }
    }
    #[cfg(not(unix))]
    {
        true
    }
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
mod tests {
    use super::*;
    use crate::config::{DependenciesConfig, DependencyEntry, DependencyInstallAction};

    fn config_with_dep(entry: DependencyEntry) -> Config {
        let toml_text = include_str!("../../../tests/fixtures/valid-opencode-stack.toml");
        let mut config = crate::config::load_config_from_str(toml_text).expect("config");
        config.dependencies = DependenciesConfig {
            commands: vec![entry],
            ..Default::default()
        };
        config
    }

    #[test]
    fn candidates_filter_to_install_blocks_only() {
        // One dep with install, one without — only the first is a
        // candidate. Proves the "narrow, explicit" Phase 4 contract:
        // no auto-derivation, just operator-declared snippets.
        let mut config = config_with_dep(DependencyEntry {
            name: "with-install".into(),
            required: true,
            feature: None,
            install: Some(DependencyInstallAction {
                shell: "true".into(),
                creates: Some("true".into()),
                scope: DependencyInstallScope::User,
                timeout_secs: None,
            }),
        });
        config.dependencies.commands.push(DependencyEntry {
            name: "no-install".into(),
            required: true,
            feature: None,
            install: None,
        });
        let candidates = candidates_for(&config, None);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].name, "with-install");
    }

    #[test]
    fn candidates_honor_feature_filter() {
        let mut config = config_with_dep(DependencyEntry {
            name: "cloudflared".into(),
            required: true,
            feature: Some("cloudflare-tunnel".into()),
            install: Some(DependencyInstallAction {
                shell: "true".into(),
                creates: Some("true".into()),
                scope: DependencyInstallScope::User,
                timeout_secs: None,
            }),
        });
        config.dependencies.commands.push(DependencyEntry {
            name: "rg".into(),
            required: true,
            feature: Some("search".into()),
            install: Some(DependencyInstallAction {
                shell: "true".into(),
                creates: Some("true".into()),
                scope: DependencyInstallScope::User,
                timeout_secs: None,
            }),
        });
        let only_cf = candidates_for(&config, Some("cloudflare-tunnel"));
        assert_eq!(only_cf.len(), 1);
        assert_eq!(only_cf[0].name, "cloudflared");
        let none = candidates_for(&config, Some("nothing-matches"));
        assert!(none.is_empty());
    }

    #[test]
    fn apply_skips_when_creates_already_resolves() {
        // `/bin/sh` is on PATH in every environment we run tests in.
        // The runner should short-circuit to AlreadyPresent without
        // spawning the (intentionally crashing) install script.
        let config = config_with_dep(DependencyEntry {
            name: "sh".into(),
            required: true,
            feature: None,
            install: Some(DependencyInstallAction {
                shell: "exit 1".into(),
                creates: Some("sh".into()),
                scope: DependencyInstallScope::User,
                timeout_secs: None,
            }),
        });
        let report = apply_dependencies(&config, None, None, "/bin/sh").expect("apply");
        assert_eq!(report.results.len(), 1);
        assert!(
            matches!(report.results[0].outcome, DepApplyOutcome::AlreadyPresent),
            "expected AlreadyPresent shortcut; got {:?}",
            report.results[0].outcome,
        );
    }

    #[test]
    fn apply_runs_shell_and_verifies_creates_postcheck() {
        // Shell that creates a sentinel binary in a controlled
        // tempdir. We extend PATH for this test so the `creates`
        // postcheck can find it. Verifies: the shell ran, the
        // postcheck resolved, the outcome is Installed.
        let tempdir = tempfile::tempdir().expect("tempdir");
        let bin = tempdir.path().join("apply-test-marker");
        let bin_str = bin.to_string_lossy().into_owned();
        // Use the absolute path as `creates` so the postcheck doesn't
        // depend on $PATH munging.
        let config = config_with_dep(DependencyEntry {
            name: "apply-test-marker".into(),
            required: true,
            feature: None,
            install: Some(DependencyInstallAction {
                shell: format!("printf '#!/bin/sh\\nexit 0\\n' > {bin_str} && chmod 755 {bin_str}"),
                creates: Some(bin_str.clone()),
                scope: DependencyInstallScope::User,
                timeout_secs: None,
            }),
        });
        let report = apply_dependencies(&config, None, None, "/bin/sh").expect("apply");
        assert_eq!(report.results.len(), 1);
        assert!(
            matches!(report.results[0].outcome, DepApplyOutcome::Installed),
            "expected Installed; got {:?}",
            report.results[0].outcome,
        );
        assert!(bin.is_file(), "shell should have created the sentinel");
    }

    #[test]
    fn apply_marks_failed_when_shell_exits_nonzero() {
        // creates resolves to a path that the failing shell will not
        // produce; outcome must be Failed with exit_code captured.
        let config = config_with_dep(DependencyEntry {
            name: "definitely-not-installed-acps-apply-fail".into(),
            required: true,
            feature: None,
            install: Some(DependencyInstallAction {
                shell: "echo nope >&2; exit 7".into(),
                creates: Some("definitely-not-installed-acps-apply-fail".into()),
                scope: DependencyInstallScope::User,
                timeout_secs: None,
            }),
        });
        let report = apply_dependencies(&config, None, None, "/bin/sh").expect("apply");
        match &report.results[0].outcome {
            DepApplyOutcome::Failed {
                exit_code,
                stderr_tail,
            } => {
                assert_eq!(*exit_code, Some(7));
                assert!(
                    stderr_tail.contains("nope"),
                    "stderr tail missing captured stderr: {stderr_tail:?}",
                );
            }
            other => panic!("expected Failed; got {other:?}"),
        }
    }

    fn system_dep(name: &str, shell: &str, creates: &str) -> DependencyEntry {
        DependencyEntry {
            name: name.into(),
            required: true,
            feature: None,
            install: Some(DependencyInstallAction {
                shell: shell.into(),
                creates: Some(creates.into()),
                scope: DependencyInstallScope::System,
                timeout_secs: None,
            }),
        }
    }

    #[test]
    fn escalation_unavailable_still_refuses_system_scope() {
        // Injected Unavailable escalation must short-circuit to
        // PrivilegeRequired without spawning anything, regardless of the
        // uid the test actually runs under.
        let config = config_with_dep(system_dep(
            "definitely-not-installed-acps-priv-check",
            // Shell is intentionally destructive-looking to make it
            // obvious if a test bug let it actually run.
            "echo SHOULD NOT EXECUTE >&2; exit 99",
            "definitely-not-installed-acps-priv-check",
        ));
        let report = apply_dependencies_with_escalation(
            &config,
            None,
            None,
            "/bin/sh",
            &PrivilegeEscalation::Unavailable { uid: 1001 },
            |_, _, _| Ok(()),
        )
        .expect("apply");
        assert!(
            matches!(
                report.results[0].outcome,
                DepApplyOutcome::PrivilegeRequired { uid: 1001 }
            ),
            "unavailable escalation must short-circuit to PrivilegeRequired; got {:?}",
            report.results[0].outcome,
        );
    }

    #[test]
    fn not_needed_escalation_is_revalidated_against_euid_at_apply_time() {
        // `NotNeeded` doubles as "nothing was pending at probe time, so no
        // probe ran". If a system-scope action becomes pending afterwards,
        // apply_one must re-derive the decision from the live euid instead
        // of running a root-intended script unprivileged. As non-root that
        // means PrivilegeRequired; as actual root it runs directly.
        let tempdir = tempfile::tempdir().expect("tempdir");
        let bin = tempdir.path().join("system-direct-marker");
        let bin_str = bin.to_string_lossy().into_owned();
        let config = config_with_dep(system_dep(
            "system-direct-marker",
            &format!("printf '#!/bin/sh\\nexit 0\\n' > {bin_str} && chmod 755 {bin_str}"),
            &bin_str,
        ));
        let report = apply_dependencies_with_escalation(
            &config,
            None,
            None,
            "/bin/sh",
            &PrivilegeEscalation::NotNeeded,
            |_, _, _| Ok(()),
        )
        .expect("apply");
        if current_uid() == 0 {
            assert!(
                matches!(report.results[0].outcome, DepApplyOutcome::Installed),
                "root must run system scope directly; got {:?}",
                report.results[0].outcome,
            );
        } else {
            assert!(
                matches!(
                    report.results[0].outcome,
                    DepApplyOutcome::PrivilegeRequired { .. }
                ),
                "stale NotNeeded must not run system scope unprivileged; got {:?}",
                report.results[0].outcome,
            );
            assert!(!bin.exists(), "script must not have executed");
        }
    }

    /// Fake `sudo` that records its argv (one per line) and then execs the
    /// remaining command, skipping the `-n` flag. Lets the escalated code
    /// path run end to end without real privileges.
    fn write_fake_sudo(dir: &Path, argv_log: &Path) -> PathBuf {
        let path = dir.join("sudo");
        let script = format!(
            "#!/bin/sh\nfor arg in \"$@\"; do printf '%s\\n' \"$arg\" >> {log}; done\nshift\nexec \"$@\"\n",
            log = argv_log.to_string_lossy(),
        );
        std::fs::write(&path, script).expect("write fake sudo");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod fake sudo");
        }
        path
    }

    #[test]
    fn sudo_escalation_wraps_shell_invocation() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let argv_log = tempdir.path().join("sudo-argv.log");
        let fake_sudo = write_fake_sudo(tempdir.path(), &argv_log);
        let bin = tempdir.path().join("sudo-escalated-marker");
        let bin_str = bin.to_string_lossy().into_owned();
        let script = format!("printf '#!/bin/sh\\nexit 0\\n' > {bin_str} && chmod 755 {bin_str}");
        let config = config_with_dep(system_dep("sudo-escalated-marker", &script, &bin_str));
        let report = apply_dependencies_with_escalation(
            &config,
            None,
            None,
            "/bin/sh",
            &PrivilegeEscalation::Sudo {
                sudo_path: fake_sudo,
                uid: 1001,
            },
            |_, _, _| Ok(()),
        )
        .expect("apply");
        assert!(
            matches!(report.results[0].outcome, DepApplyOutcome::Installed),
            "expected Installed through fake sudo; got {:?}",
            report.results[0].outcome,
        );
        let argv = std::fs::read_to_string(&argv_log).expect("argv log");
        let lines: Vec<&str> = argv.lines().collect();
        assert_eq!(&lines[..3], &[SUDO_NON_INTERACTIVE_FLAG, "/bin/sh", "-c"]);
        let escalated = lines[3..].join("\n");
        assert!(
            escalated.ends_with(&script),
            "operator script must be verbatim and last: {escalated:?}",
        );
        assert!(
            escalated.contains("export DEBIAN_FRONTEND=noninteractive"),
            "non-interactive env must be re-exported inside the escalated shell: {escalated:?}",
        );
    }

    #[test]
    fn escalated_run_records_sudo_marker_in_stdout() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let argv_log = tempdir.path().join("sudo-argv.log");
        let fake_sudo = write_fake_sudo(tempdir.path(), &argv_log);
        let bin = tempdir.path().join("sudo-marker-audit");
        let bin_str = bin.to_string_lossy().into_owned();
        let config = config_with_dep(system_dep(
            "sudo-marker-audit",
            &format!("printf '#!/bin/sh\\nexit 0\\n' > {bin_str} && chmod 755 {bin_str}"),
            &bin_str,
        ));
        let store = StateStore::open(tempdir.path().join("state.sqlite")).expect("state open");
        store.migrate().expect("migrate");
        apply_dependencies_with_escalation(
            &config,
            None,
            Some(&store),
            "/bin/sh",
            &PrivilegeEscalation::Sudo {
                sudo_path: fake_sudo,
                uid: 1001,
            },
            |_, _, _| Ok(()),
        )
        .expect("apply");
        let rows = store
            .query_installer_runs_filtered(Some(DEPS_APPLY_AGENT_ID), 10)
            .expect("query");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, "installed");
        assert_eq!(rows[0].method.as_deref(), Some("shell"));
        assert!(
            rows[0].stdout.starts_with(ESCALATED_STDOUT_MARKER),
            "persisted stdout must lead with the escalation marker: {:?}",
            rows[0].stdout,
        );
    }

    #[test]
    fn escalated_script_reexports_non_interactive_env() {
        let script = escalated_script("apt-get install -y jq");
        for (name, value) in NON_INTERACTIVE_ENV {
            assert!(
                script.contains(&format!("export {name}={value}")),
                "missing export for {name}: {script:?}",
            );
        }
        assert!(
            script.ends_with("apt-get install -y jq"),
            "operator script must be appended verbatim and last: {script:?}",
        );
    }

    #[test]
    fn manual_privileged_command_quotes_embedded_single_quotes() {
        let candidate = DepApplyCandidate {
            name: "quoted".into(),
            scope: DependencyInstallScope::System,
            shell: "echo 'hi'".into(),
            creates: "quoted".into(),
        };
        assert_eq!(
            manual_privileged_command("/bin/sh", &candidate),
            r"sudo /bin/sh -c 'echo '\''hi'\'''",
        );
    }

    /// Write an executable script that exits with `code`, standing in for
    /// sudo in probe tests.
    fn write_exit_stub(dir: &Path, name: &str, code: i32) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\nexit {code}\n")).expect("write stub");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod stub");
        }
        path
    }

    #[test]
    fn probe_collapses_missing_and_denied_sudo_to_unavailable() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            probe_privilege_escalation_with(1001, None),
            PrivilegeEscalation::Unavailable { uid: 1001 },
        );
        // A "sudo" that exits non-zero (password required) is Unavailable.
        let denied_sudo = write_exit_stub(tempdir.path(), "sudo-denied", 1);
        assert_eq!(
            probe_privilege_escalation_with(1001, Some(denied_sudo)),
            PrivilegeEscalation::Unavailable { uid: 1001 },
        );
        // A "sudo" that exits zero advertises escalation with its path.
        // The probe deliberately collapses transient spawn errors (e.g.
        // fork EAGAIN when the whole suite runs in parallel) to
        // Unavailable, so give the load-sensitive granted case a couple
        // of retries before declaring the logic wrong.
        let granted_sudo = write_exit_stub(tempdir.path(), "sudo-granted", 0);
        let mut granted = probe_privilege_escalation_with(1001, Some(granted_sudo.clone()));
        for _ in 0..2 {
            if matches!(granted, PrivilegeEscalation::Sudo { .. }) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
            granted = probe_privilege_escalation_with(1001, Some(granted_sudo.clone()));
        }
        assert_eq!(
            granted,
            PrivilegeEscalation::Sudo {
                sudo_path: granted_sudo,
                uid: 1001,
            },
        );
        // Root short-circuits without touching the candidate path at all.
        assert_eq!(
            probe_privilege_escalation_with(0, None),
            PrivilegeEscalation::NotNeeded,
        );
    }

    #[test]
    fn escalation_notice_lines_cover_all_modes() {
        let candidate = DepApplyCandidate {
            name: "acpstack-system-dep".into(),
            scope: DependencyInstallScope::System,
            shell: "apt-get install -y jq".into(),
            creates: "jq".into(),
        };
        let candidates = vec![candidate];
        assert!(
            escalation_notice_lines(&PrivilegeEscalation::NotNeeded, "/bin/sh", &[]).is_empty(),
            "no system candidates must yield no notice",
        );
        let root = escalation_notice_lines(&PrivilegeEscalation::NotNeeded, "/bin/sh", &candidates);
        assert_eq!(root.len(), 1);
        assert!(root[0].contains("run them directly"), "{root:?}");
        let sudo = escalation_notice_lines(
            &PrivilegeEscalation::Sudo {
                sudo_path: PathBuf::from("/usr/bin/sudo"),
                uid: 1001,
            },
            "/bin/sh",
            &candidates,
        );
        assert_eq!(sudo.len(), 1);
        assert!(sudo[0].contains("`sudo -n`"), "{sudo:?}");
        let unavailable = escalation_notice_lines(
            &PrivilegeEscalation::Unavailable { uid: 1001 },
            "/bin/sh",
            &candidates,
        );
        assert!(
            unavailable[0].contains("skipped and recorded as privilege_required"),
            "{unavailable:?}",
        );
        assert!(
            unavailable
                .iter()
                .any(|line| line.contains("sudo /bin/sh -c 'apt-get install -y jq'")),
            "manual command must be listed per candidate: {unavailable:?}",
        );
    }

    #[test]
    fn pending_system_candidates_filters_scope_and_presence() {
        let mut config = config_with_dep(system_dep(
            "definitely-not-installed-acps-system-pending",
            "true",
            "definitely-not-installed-acps-system-pending",
        ));
        // Present system dep (creates resolves) — excluded.
        config
            .dependencies
            .commands
            .push(system_dep("sh-present", "true", "sh"));
        // Pending user dep — excluded by scope.
        config.dependencies.commands.push(DependencyEntry {
            name: "definitely-not-installed-acps-user-pending".into(),
            required: true,
            feature: None,
            install: Some(DependencyInstallAction {
                shell: "true".into(),
                creates: Some("definitely-not-installed-acps-user-pending".into()),
                scope: DependencyInstallScope::User,
                timeout_secs: None,
            }),
        });
        let pending = pending_system_candidates(&config, None);
        assert_eq!(pending.len(), 1);
        assert_eq!(
            pending[0].name,
            "definitely-not-installed-acps-system-pending"
        );
    }

    #[test]
    fn reap_with_grace_bounds_wait_and_reaps_exited_children() {
        let mut exited = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("exit 0")
            .spawn()
            .expect("spawn");
        let status = reap_with_grace(&mut exited, Duration::from_secs(5));
        assert!(status.is_some(), "exited child must reap within grace");

        let mut running = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("sleep 30")
            .spawn()
            .expect("spawn");
        let started = Instant::now();
        let status = reap_with_grace(&mut running, Duration::from_millis(200));
        assert!(status.is_none(), "running child must return None");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "grace must bound the wait",
        );
        let _ = running.kill();
        let _ = running.wait();
    }

    #[test]
    fn before_after_status_honors_absolute_creates_path() {
        // Regression: before/after originally went through
        // check_dependencies(config) which resolves entry.name on
        // PATH. A dep whose install.creates is an absolute path would
        // succeed but the after-status would still say "missing".
        // Now the report uses check_one for command deps with an
        // install block, so absolute `creates` resolves correctly.
        let tempdir = tempfile::tempdir().expect("tempdir");
        let bin = tempdir.path().join("apply-before-after");
        let bin_str = bin.to_string_lossy().into_owned();
        let config = config_with_dep(DependencyEntry {
            name: "apply-before-after".into(),
            required: true,
            feature: None,
            install: Some(DependencyInstallAction {
                shell: format!("printf '#!/bin/sh\\nexit 0\\n' > {bin_str} && chmod 755 {bin_str}"),
                creates: Some(bin_str.clone()),
                scope: DependencyInstallScope::User,
                timeout_secs: None,
            }),
        });
        let report = apply_dependencies(&config, None, None, "/bin/sh").expect("apply");
        let after_entry = report
            .after
            .iter()
            .find(|s| s.name == "apply-before-after")
            .expect("after row");
        assert!(
            after_entry.available,
            "report.after must honor absolute creates path; got {after_entry:?}",
        );
    }

    #[test]
    fn timeout_kills_entire_process_group() {
        // Regression: kill on just the shell child would let
        // grandchildren keep the pipes open, hanging the join
        // threads past the operator-declared timeout. With process
        // group cleanup, a `sleep 999` inside the shell is reaped
        // and the call returns within the timeout window.
        let config = config_with_dep(DependencyEntry {
            name: "definitely-not-installed-timeout-check".into(),
            required: true,
            feature: None,
            install: Some(DependencyInstallAction {
                // Background a long sleep + a foreground long sleep
                // so killing only the shell would still leave a live
                // descendant with the pipes open.
                shell: "sleep 60 & sleep 60".into(),
                creates: Some("definitely-not-installed-timeout-check".into()),
                scope: DependencyInstallScope::User,
                timeout_secs: Some(1),
            }),
        });
        let started = std::time::Instant::now();
        let report = apply_dependencies(&config, None, None, "/bin/sh").expect("apply");
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(15),
            "1s timeout must kill the whole group; took {elapsed:?}",
        );
        match &report.results[0].outcome {
            DepApplyOutcome::Failed { exit_code, .. } => {
                assert!(
                    exit_code.is_none(),
                    "timed-out runs report None exit_code, got {exit_code:?}",
                );
            }
            other => panic!("expected Failed on timeout; got {other:?}"),
        }
    }

    #[test]
    fn stderr_tail_captures_actual_tail_when_stream_blows_past_cap() {
        // Regression: the prior implementation stored only the first
        // 64 KiB of stderr and computed `tail` from that prefix —
        // for verbose installers, the actual failure diagnostic at
        // the very end would be lost. The rolling-tail buffer
        // ensures the last `STDERR_TAIL_BYTES` of the full stream
        // make it into the report, regardless of how chatty the
        // installer was.
        let marker = "FINAL_DIAGNOSTIC_AT_THE_END_aaa";
        // Push ~80 KiB of noise into STDERR (the reader's 64 KiB
        // prefix fills well before the marker arrives), then print
        // the marker, then exit 1. The marker can ONLY survive if
        // the rolling-tail buffer is doing its job. The previous
        // test wrote the noise to stdout instead, so the rolling
        // tail was never exercised.
        let shell = format!(
            "yes 'noise line that is long enough to push past 64 KiB quickly' | head -n 1500 1>&2; \
             printf %s {marker} 1>&2; exit 1"
        );
        let config = config_with_dep(DependencyEntry {
            name: "definitely-not-installed-tail-check".into(),
            required: true,
            feature: None,
            install: Some(DependencyInstallAction {
                shell,
                creates: Some("definitely-not-installed-tail-check".into()),
                scope: DependencyInstallScope::User,
                timeout_secs: Some(30),
            }),
        });
        let report = apply_dependencies(&config, None, None, "/bin/sh").expect("apply");
        match &report.results[0].outcome {
            DepApplyOutcome::Failed { stderr_tail, .. } => {
                assert!(
                    stderr_tail.contains(marker),
                    "stderr_tail must contain the final diagnostic; got {stderr_tail:?}",
                );
            }
            other => panic!("expected Failed; got {other:?}"),
        }
    }
}
