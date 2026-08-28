//! Privilege-escalation policy for `scope = "system"` install actions:
//! the probe, the escalated-script wrapper, and the operator-facing
//! notice lines shared by `acps init` and `acps deps apply`.

use super::*;

/// `sudo -n` never prompts: it exits non-zero immediately when a password
/// would be required, so neither the probe nor an escalated run can block
/// on stdin or a controlling terminal.
pub(crate) const SUDO_PROGRAM: &str = "sudo";
pub(crate) const SUDO_NON_INTERACTIVE_FLAG: &str = "-n";
/// Bounds the `sudo -n true` probe so a wedged sudoers backend (LDAP/SSSD)
/// cannot stall an apply before a single dep has run.
const SUDO_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// Grace period for reaping a killed action. An escalated action runs as
/// root, so a non-root parent's SIGKILL is refused with EPERM; an unbounded
/// `wait()` there would hang the whole apply.
pub(crate) const KILL_REAP_GRACE: Duration = Duration::from_secs(5);
/// Provenance line prepended to an escalated action's persisted stdout, so
/// `installer_runs.method` can stay `shell` while history still shows sudo.
pub(crate) const ESCALATED_STDOUT_MARKER: &str = "[acps] escalated via `sudo -n`";

/// How the apply runner reaches root for `scope = "system"` actions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrivilegeEscalation {
    /// euid == 0 — system-scope actions run directly.
    NotNeeded,
    /// euid != 0 but `sudo -n true` succeeded. `sudo_path` is resolved once at
    /// probe time so probe and run cannot pick different binaries.
    Sudo { sudo_path: PathBuf, uid: u32 },
    /// euid != 0 and passwordless sudo is unavailable.
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

/// Probe how system-scope actions can reach root; environment facts collapse
/// to `Unavailable` rather than Err. Deliberately uncached, so a long-lived
/// daemon cannot pin stale sudoers state across a config change.
pub fn probe_privilege_escalation(home: &Path) -> PrivilegeEscalation {
    probe_privilege_escalation_with(current_uid(), resolve_command(SUDO_PROGRAM), home)
}

/// Testable core of [`probe_privilege_escalation`]; uid and sudo path are
/// injected so tests need not mutate the process-global PATH.
pub(crate) fn probe_privilege_escalation_with(
    uid: u32,
    sudo_path: Option<PathBuf>,
    home: &Path,
) -> PrivilegeEscalation {
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
        .envs(scrubbed_env(home));
    apply_non_interactive_env(&mut command);
    detach_into_new_session(&mut command);
    let Ok(mut child) = command.spawn() else {
        return PrivilegeEscalation::Unavailable { uid };
    };
    match wait_with_timeout(&mut child, Instant::now() + SUDO_PROBE_TIMEOUT) {
        Ok(Some(status)) if status.success() => PrivilegeEscalation::Sudo { sudo_path, uid },
        Ok(Some(_)) => PrivilegeEscalation::Unavailable { uid },
        Ok(None) | Err(_) => {
            // The probe child may still be alive; reap the group so it cannot
            // outlive the probe.
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
pub(crate) fn escalation_for(
    config: &Config,
    feature: Option<&str>,
    home: &Path,
) -> PrivilegeEscalation {
    if pending_system_candidates(config, feature).is_empty() {
        PrivilegeEscalation::NotNeeded
    } else {
        probe_privilege_escalation(home)
    }
}

/// Re-export the non-interactive vars inside the escalated shell, since
/// sudoers `env_reset` drops them before the operator's script runs. Names and
/// values come from the compile-time [`NON_INTERACTIVE_ENV`] table, never
/// operator input, so no quoting is needed.
pub(crate) fn escalated_script(script: &str) -> String {
    let mut out = String::new();
    for (name, value) in NON_INTERACTIVE_ENV {
        writeln!(&mut out, "export {name}={value}").expect("write to String");
    }
    out.push_str(script);
    out
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
/// apply` so the two confirmation prompts cannot drift.
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
            lines
        }
    }
}

pub(crate) fn current_uid() -> u32 {
    // SAFETY: `geteuid()` is always safe — no preconditions.
    unsafe { libc::geteuid() }
}
