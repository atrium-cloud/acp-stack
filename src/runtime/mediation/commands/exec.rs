//! Process-execution mechanics shared by the command supervisor and the ACP
//! terminal handlers: sandbox wrapping, TOCTOU-safe cwd entry, env-cleared
//! spawn under a fresh process group, and grace-escalated kill. Policy stays
//! with each caller; this module never decides whether to run, only how.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::{Child, Command};
use tokio::time::timeout;

use crate::runtime::process_runner::kill_tokio_process_group;

use super::policy::ResolvedCommandCwd;
use super::process::send_terminate;

/// Resolve the program + argv to spawn, wrapping it in the sandbox backend so
/// a mediated child cannot read the daemon's secrets.
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
    // The network provider's declaration must land after the caller's env so
    // it wins on conflict.
    let mut workload_env = env.cloned().unwrap_or_default();
    crate::extensions::apply_workload_env(&mut workload_env, network);
    for (key, value) in &workload_env {
        cmd.env(key, value);
    }
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    // Without this a running child can outlive `acps serve`.
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
    /// The child exited (or `wait` failed) within the grace window.
    ExitedWithinGrace(std::io::Result<std::process::ExitStatus>),
    /// The grace window elapsed; the process group was SIGKILLed and reaped.
    KilledAfterGrace,
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::extensions::NetworkProviderExtension;
    use std::collections::BTreeMap;

    fn network_provider(entries: &[(&str, &str)]) -> NetworkProviderExtension {
        NetworkProviderExtension {
            name: "egress".to_owned(),
            provider: Vec::new(),
            provider_timeout: None,
            provider_stderr: crate::config::SandboxProviderStderr::default(),
            workload_env: entries
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect::<BTreeMap<_, _>>(),
        }
    }

    async fn run_echo(
        variable: &str,
        env: Option<&HashMap<String, String>>,
        network: Option<&NetworkProviderExtension>,
    ) -> String {
        let workspace = tempfile::tempdir().expect("workspace");
        let cwd = super::super::policy::resolve_cwd_under_workspace(workspace.path(), ".")
            .expect("workspace cwd");
        let child = spawn_child(
            Path::new("/bin/sh"),
            &[
                "-c".to_owned(),
                format!("printf %s \"${variable}\""),
                "workload".to_owned(),
            ],
            &cwd,
            env,
            &crate::config::SandboxConfig::default(),
            network,
        )
        .expect("spawn workload");
        let output = child.wait_with_output().await.expect("workload output");
        String::from_utf8(output.stdout).expect("utf8 stdout")
    }

    #[tokio::test]
    async fn workload_env_reaches_a_mediated_child_without_caller_env() {
        let stdout = run_echo(
            "HTTPS_PROXY",
            None,
            Some(&network_provider(&[(
                "HTTPS_PROXY",
                "http://127.0.0.1:3128",
            )])),
        )
        .await;
        assert_eq!(stdout, "http://127.0.0.1:3128");
    }

    #[tokio::test]
    async fn workload_env_overrides_a_conflicting_caller_value() {
        let caller = HashMap::from([("HTTPS_PROXY".to_owned(), "http://stale:1".to_owned())]);
        let stdout = run_echo(
            "HTTPS_PROXY",
            Some(&caller),
            Some(&network_provider(&[(
                "HTTPS_PROXY",
                "http://127.0.0.1:3128",
            )])),
        )
        .await;
        assert_eq!(stdout, "http://127.0.0.1:3128");
    }

    #[tokio::test]
    async fn caller_env_survives_when_no_provider_is_declared() {
        let caller = HashMap::from([("GREETING".to_owned(), "hi".to_owned())]);
        assert_eq!(run_echo("GREETING", Some(&caller), None).await, "hi");
    }
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
