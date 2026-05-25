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
//! - System-scoped actions (`scope = "system"`) refuse to run when
//!   the daemon isn't root, so OS package managers don't get invoked
//!   from a stale CLI by mistake.
//! - Caller must confirm before any action runs (`--yes` flag on the
//!   CLI, `confirmation: true` body field on the API).

use std::collections::HashMap;
use std::fmt::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use chrono::{SecondsFormat, Utc};
use serde::Serialize;

use crate::config::{Config, DependencyEntry, DependencyInstallScope};
use crate::error::{Result, StackError};
use crate::runtime::dependencies::deps::{DepStatus, check_dependencies};
use crate::runtime::process_runner::{
    STDERR_TAIL_BYTES, join_reader_bounded, kill_process_group, read_to_cap, read_to_cap_with_tail,
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
    /// Action declared `scope = "system"` but the daemon isn't root.
    /// No subprocess was spawned.
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
    for entry in &config.dependencies.commands {
        let Some(install) = entry.install.as_ref() else {
            continue;
        };
        if let Some(filter) = feature
            && entry.feature.as_deref() != Some(filter)
        {
            continue;
        }
        results.push(apply_one(
            entry,
            install,
            state,
            shell_program,
            &apply_run_id,
        )?);
    }
    let after = compute_before_after_report(config);
    Ok(DepsApplyReport {
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

    if install.scope == DependencyInstallScope::System {
        let uid = current_uid();
        if uid != 0 {
            let finished_at = current_timestamp();
            let stderr_message = format!(
                "dep `{name}` declares scope=system but the runtime is uid={uid}; \
                 re-run the install action through sudo or root",
                name = entry.name,
            );
            if let Some(store) = state {
                store.append_installer_run(InstallerRunInput {
                    agent_id: DEPS_APPLY_AGENT_ID,
                    started_at: &started_at,
                    finished_at: Some(&finished_at),
                    status: "privilege_required",
                    stdout: "",
                    stderr: &stderr_message,
                    exit_status: None,
                    step: DEPS_APPLY_STEP,
                    version: None,
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

    let timeout = install
        .timeout_secs
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_TIMEOUT);
    let (exit_code, stdout, stderr, timed_out, stderr_tail) =
        run_shell(shell_program, &install.shell, timeout)?;
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
        store.append_installer_run(InstallerRunInput {
            agent_id: DEPS_APPLY_AGENT_ID,
            started_at: &started_at,
            finished_at: Some(&finished_at),
            status: status_label,
            stdout: &cap_stream(&stdout),
            stderr: &cap_stream(&stderr),
            exit_status: persisted_exit,
            step: DEPS_APPLY_STEP,
            version: None,
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
) -> Result<(Option<i32>, String, String, bool, String)> {
    let mut command = Command::new(shell_program);
    command
        .arg("-c")
        .arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_clear()
        .envs(scrubbed_env());
    // Put the shell into its own process group so a timeout-induced
    // kill reaches every grandchild it forked. Without this,
    // `child.kill()` only stops the shell — a `sleep 999` it spawned
    // would keep the stdout/stderr pipes open and the join threads
    // would block forever, defeating the timeout the user thinks
    // they're getting. Same pattern as agent_installer.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
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
                    kill_process_group(&mut child);
                    timed_out = true;
                    let _ = child.wait();
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

/// Convenience used by the CLI to print a confirmation summary before
/// invoking the runner. Returns the same candidate set the runner will
/// process, plus a flag indicating whether any `system`-scoped action
/// is present (so the prompt can warn the operator).
pub fn summarize_candidates(candidates: &[DepApplyCandidate]) -> (usize, bool) {
    let count = candidates.len();
    let any_system = candidates
        .iter()
        .any(|c| c.scope == DependencyInstallScope::System);
    (count, any_system)
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
        let toml_text = include_str!("../../../tests/fixtures/valid-acp-stack.toml");
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

    #[test]
    fn apply_refuses_system_scope_when_not_root() {
        // Test runs as a non-root user (CI + dev shells), so a
        // scope=system dep that's actually missing must short-circuit
        // to PrivilegeRequired without spawning anything.
        let config = config_with_dep(DependencyEntry {
            name: "definitely-not-installed-acps-priv-check".into(),
            required: true,
            feature: None,
            install: Some(DependencyInstallAction {
                // Shell is intentionally destructive-looking to make it
                // obvious if the test bug let it actually run.
                shell: "echo SHOULD NOT EXECUTE >&2; exit 99".into(),
                creates: Some("definitely-not-installed-acps-priv-check".into()),
                scope: DependencyInstallScope::System,
                timeout_secs: None,
            }),
        });
        let report = apply_dependencies(&config, None, None, "/bin/sh").expect("apply");
        if current_uid() == 0 {
            // Test was inexplicably run as root; outcome reflects
            // that the shell DID execute and failed.
            assert!(matches!(
                report.results[0].outcome,
                DepApplyOutcome::Failed { .. }
            ));
        } else {
            assert!(
                matches!(
                    report.results[0].outcome,
                    DepApplyOutcome::PrivilegeRequired { .. }
                ),
                "non-root test must short-circuit to PrivilegeRequired; got {:?}",
                report.results[0].outcome,
            );
        }
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
