//! Process-execution primitives shared by the command supervisor and the ACP
//! terminal handlers. These are the mechanics that must behave identically for
//! every daemon-mediated child: sandbox wrapping, TOCTOU-safe cwd entry,
//! env-cleared spawn under a fresh process group, and grace-escalated kill.
//! Policy (permissions, review flags) stays with each caller — this module
//! never decides whether to run, only how.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::{Child, Command};
use tokio::time::timeout;

use crate::runtime::process_runner::kill_tokio_process_group;

use super::policy::ResolvedCommandCwd;
use super::process::send_terminate;

/// Resolve the program + argv to spawn, applying the sandbox backend. `off`
/// runs the program verbatim; other modes wrap it the same way the agent
/// harness is wrapped, so a mediated child cannot read the daemon's secrets
/// either.
pub(crate) fn sandboxed_program(
    program: &Path,
    args: &[String],
    sandbox: &crate::config::SandboxConfig,
    network: Option<&crate::extensions::NetworkProviderExtension>,
    workspace_root: &Path,
) -> std::io::Result<(PathBuf, Vec<String>)> {
    if matches!(sandbox.mode, crate::config::SandboxMode::Off) {
        return Ok((program.to_path_buf(), args.to_vec()));
    }
    let home = crate::fs_util::home_dir().map_err(std::io::Error::other)?;
    let wrapped = crate::runtime::sandbox::wrap(
        sandbox,
        network,
        program,
        args,
        &home,
        workspace_root,
        crate::ownership::process_euid(),
        crate::ownership::process_egid(),
    )
    .map_err(std::io::Error::other)?;
    Ok((wrapped.program, wrapped.args))
}

pub(crate) fn spawn_child(
    program: &Path,
    args: &[String],
    cwd: &ResolvedCommandCwd,
    env: Option<&HashMap<String, String>>,
    sandbox: &crate::config::SandboxConfig,
    network: Option<&crate::extensions::NetworkProviderExtension>,
) -> std::io::Result<Child> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    #[cfg(unix)]
    let cwd_handle = cwd.open_verified()?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let cwd_fd = cwd_handle.as_raw_fd();
        unsafe {
            cmd.pre_exec(move || {
                if libc::fchdir(cwd_fd) == -1 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
    }
    #[cfg(not(unix))]
    cmd.current_dir(cwd.path());
    // Network-isolated spawns need the daemon's stderr at the supervisor's
    // diagnostic fd; stdout/stderr below are captured pipes, not a channel the
    // supervisor may write to.
    #[cfg(unix)]
    let diag_handle =
        crate::runtime::sandbox::wire_supervise_diag_fd(sandbox, network, &mut cmd, args)?;
    cmd.env_clear();
    if let Some(env) = env {
        for (key, value) in env {
            cmd.env(key, value);
        }
    }
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    // SIGKILL the child if the owning task is ever dropped — daemon shutdown,
    // tokio runtime exit, or a panic in the owner itself. Without this a
    // running child can outlive `acps serve`.
    cmd.kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    let child = cmd.spawn();
    #[cfg(unix)]
    drop(cwd_handle);
    #[cfg(unix)]
    drop(diag_handle);
    child
}

pub(crate) enum GraceKillOutcome {
    /// The child exited (or `wait` failed) within the grace window after
    /// SIGTERM. Carries the wait result so callers can distinguish a clean
    /// exit from a wait error.
    ExitedWithinGrace(std::io::Result<std::process::ExitStatus>),
    /// The grace window elapsed; the whole process group was SIGKILLed and
    /// the child reaped.
    KilledAfterGrace,
}

/// SIGTERM the child's process group, wait up to `grace`, then escalate to a
/// process-group SIGKILL and reap.
pub(crate) async fn kill_with_grace(child: &mut Child, grace: Duration) -> GraceKillOutcome {
    send_terminate(child);
    match timeout(grace, child.wait()).await {
        Ok(result) => GraceKillOutcome::ExitedWithinGrace(result),
        Err(_) => {
            kill_tokio_process_group(child);
            if let Err(error) = child.wait().await {
                tracing::warn!(error = %error, "wait after SIGKILL failed while escalating child termination");
            }
            GraceKillOutcome::KilledAfterGrace
        }
    }
}
