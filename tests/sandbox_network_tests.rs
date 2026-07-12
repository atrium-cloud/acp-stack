//! Behavior of the network-isolation supervisor (`acps __sandbox-supervise`):
//! fail-closed setup, teardown ordering, provider env/stdio hygiene, namespace
//! lifetime, and the end-to-end deny-all / veth-provider guarantees.
//!
//! Requires Linux and `CAP_SYS_ADMIN` (a privileged container) like the mount
//! isolation tests; the veth case additionally needs `ip`/`nsenter` and
//! `CAP_NET_ADMIN`. Every case is ignored by default. Privileged Linux runners invoke this test
//! binary with `--ignored`; capability probes then fail hard so a misconfigured
//! runner cannot report the guarantees green while asserting nothing.

#![cfg(target_os = "linux")]

use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use acp_stack::runtime::sandbox::supervise::{SETUP_FAILED_EXIT, TEARDOWN_FAILED_EXIT};

/// True when this process can create the namespaces (incl. `--net`) the
/// isolated wrapper relies on.
fn unshare_net_usable() -> bool {
    Command::new("unshare")
        .args([
            "--net",
            "--mount",
            "--pid",
            "--fork",
            "--mount-proc",
            "--",
            "true",
        ])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Assert a capability required by an explicitly requested privileged test.
fn require_capability(available: bool, reason: &str) {
    assert!(
        available,
        "required sandbox test capability missing on this runner: {reason}"
    );
}

/// `version_arg` differs per tool: iproute2's `ip` only understands `-V`.
fn bin_available(name: &str, version_arg: &str) -> bool {
    Command::new(name)
        .arg(version_arg)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Build a `__sandbox-supervise` invocation mirroring the production wrapper:
/// diag fd 3 is wired to the test's stderr the same way the daemon wires its
/// own stderr, and the inner chain is `unshare --net … __sandbox-exec -- work`.
fn supervise_command(
    provider: &[&str],
    timeout: &str,
    stderr_mode: &str,
    workload: &[&str],
) -> Command {
    let acps = env!("CARGO_BIN_EXE_acps");
    let mut cmd = Command::new(acps);
    cmd.args([
        "__sandbox-supervise",
        "--diag-fd",
        "3",
        "--provider-timeout",
        timeout,
        "--provider-stderr",
        stderr_mode,
    ]);
    for arg in provider {
        cmd.args(["--provider-arg", arg]);
    }
    cmd.arg("--");
    cmd.args([
        "unshare",
        "--net",
        "--mount",
        "--uts",
        "--ipc",
        "--pid",
        "--fork",
        "--mount-proc",
        "--kill-child",
        "--propagation",
        "private",
        "--",
        acps,
        "__sandbox-exec",
        "--",
    ]);
    cmd.args(workload);
    // SAFETY: dup2 is async-signal-safe; this mirrors the daemon's diag wiring.
    unsafe {
        cmd.pre_exec(|| {
            if libc::dup2(libc::STDERR_FILENO, 3) == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    cmd
}

/// Write an executable provider script. Provider processes start with a
/// cleared environment, so the script sets its own PATH — exactly what real
/// providers must do.
fn write_provider_script(dir: &Path, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("provider.sh");
    let script = format!(
        "#!/bin/sh\nPATH=/usr/sbin:/usr/bin:/sbin:/bin\nexport PATH\nphase=\"$1\"\nmarkdir=\"$2\"\n{body}\n"
    );
    std::fs::write(&path, script).expect("write provider script");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("chmod provider script");
    path
}

fn wait_for_file(path: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    path.exists()
}

fn process_identity(pid: i32) -> Option<(char, u64)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, fields) = stat.rsplit_once(") ")?;
    let fields: Vec<&str> = fields.split_whitespace().collect();
    let state = fields.first()?.chars().next()?;
    let start_time = fields.get(19)?.parse().ok()?;
    Some((state, start_time))
}

fn process_parent(pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, fields) = stat.rsplit_once(") ")?;
    fields.split_whitespace().nth(1)?.parse().ok()
}

fn process_children(pid: i32) -> Vec<i32> {
    std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"))
        .expect("read process children")
        .split_whitespace()
        .map(|value| value.parse().expect("child pid parses"))
        .collect()
}

fn tracked_process(pid: i32) -> (i32, u64) {
    let (_, start_time) = process_identity(pid).expect("tracked process must be live");
    (pid, start_time)
}

fn wait_for_process_identity_gone(pid: i32, start_time: u64, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        match process_identity(pid) {
            None => return true,
            Some((state, current_start_time))
                if state == 'Z' || current_start_time != start_time =>
            {
                return true;
            }
            Some(_) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Some(_) => return false,
        }
    }
}

fn read_provider_processes(path: &Path) -> [(i32, u64); 2] {
    let raw = std::fs::read_to_string(path).expect("read provider pid marker");
    let mut pids = raw.split_whitespace().map(|value| {
        value
            .parse::<i32>()
            .expect("provider pid marker must contain numeric pids")
    });
    let provider_pid = pids.next().expect("provider pid missing");
    let child_pid = pids.next().expect("provider child pid missing");
    assert!(
        pids.next().is_none(),
        "unexpected provider pid marker: {raw}"
    );
    [provider_pid, child_pid].map(|pid| {
        let (_, start_time) = process_identity(pid).expect("provider process must be live");
        (pid, start_time)
    })
}

#[test]
#[ignore = "requires privileged Linux sandbox capabilities"]
fn setup_failure_prevents_workload_and_still_tears_down() {
    require_capability(
        unshare_net_usable(),
        "unshare --net / CAP_SYS_ADMIN unavailable",
    );
    let tmp = tempfile::tempdir().expect("tempdir");
    let markdir = tmp.path();
    let provider = write_provider_script(
        markdir,
        "touch \"$markdir/$phase-ran\"\n[ \"$phase\" = setup ] && exit 1\nexit 0",
    );
    let workload_marker = markdir.join("workload-ran");

    let status = supervise_command(
        &[provider.to_str().unwrap(), markdir.to_str().unwrap()],
        "10s",
        "daemon",
        &["/bin/touch", workload_marker.to_str().unwrap()],
    )
    .status()
    .expect("run supervise");

    assert_eq!(status.code(), Some(SETUP_FAILED_EXIT));
    assert!(markdir.join("setup-ran").exists(), "setup must have run");
    assert!(
        markdir.join("teardown-ran").exists(),
        "teardown must run after a failed setup (partial-setup cleanup)"
    );
    assert!(
        !workload_marker.exists(),
        "the workload must never execute when setup fails"
    );
}

#[test]
#[ignore = "requires privileged Linux sandbox capabilities"]
fn setup_timeout_kills_provider_and_prevents_workload() {
    require_capability(
        unshare_net_usable(),
        "unshare --net / CAP_SYS_ADMIN unavailable",
    );
    let tmp = tempfile::tempdir().expect("tempdir");
    let markdir = tmp.path();
    let provider = write_provider_script(
        markdir,
        "touch \"$markdir/$phase-ran\"\n[ \"$phase\" = setup ] && sleep 60\nexit 0",
    );
    let workload_marker = markdir.join("workload-ran");

    let started = Instant::now();
    let status = supervise_command(
        &[provider.to_str().unwrap(), markdir.to_str().unwrap()],
        "1s",
        "daemon",
        &["/bin/touch", workload_marker.to_str().unwrap()],
    )
    .status()
    .expect("run supervise");

    assert_eq!(status.code(), Some(SETUP_FAILED_EXIT));
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "a 1s setup timeout must not wait for the 60s provider sleep"
    );
    assert!(!workload_marker.exists());
    assert!(markdir.join("teardown-ran").exists());
}

#[test]
#[ignore = "requires privileged Linux sandbox capabilities"]
fn teardown_runs_after_normal_exit_and_status_is_mirrored() {
    require_capability(
        unshare_net_usable(),
        "unshare --net / CAP_SYS_ADMIN unavailable",
    );
    let tmp = tempfile::tempdir().expect("tempdir");
    let markdir = tmp.path();
    let provider = write_provider_script(markdir, "touch \"$markdir/$phase-ran\"\nexit 0");

    // Success path: workload exit 0 is preserved.
    let status = supervise_command(
        &[provider.to_str().unwrap(), markdir.to_str().unwrap()],
        "10s",
        "daemon",
        &["/bin/true"],
    )
    .status()
    .expect("run supervise");
    assert_eq!(status.code(), Some(0));
    assert!(markdir.join("setup-ran").exists());
    assert!(markdir.join("teardown-ran").exists());

    // Failure path: a nonzero workload exit code is preserved verbatim.
    let status = supervise_command(
        &[provider.to_str().unwrap(), markdir.to_str().unwrap()],
        "10s",
        "daemon",
        &["/bin/sh", "-c", "exit 7"],
    )
    .status()
    .expect("run supervise");
    assert_eq!(status.code(), Some(7));
}

#[test]
#[ignore = "requires privileged Linux sandbox capabilities"]
fn teardown_failure_after_workload_success_exits_nonzero() {
    require_capability(
        unshare_net_usable(),
        "unshare --net / CAP_SYS_ADMIN unavailable",
    );
    let tmp = tempfile::tempdir().expect("tempdir");
    let markdir = tmp.path();
    let provider = write_provider_script(markdir, "[ \"$phase\" = teardown ] && exit 1\nexit 0");

    let status = supervise_command(
        &[provider.to_str().unwrap(), markdir.to_str().unwrap()],
        "10s",
        "daemon",
        &["/bin/true"],
    )
    .status()
    .expect("run supervise");
    assert_eq!(status.code(), Some(TEARDOWN_FAILED_EXIT));

    // A workload failure is preserved even when teardown also fails.
    let status = supervise_command(
        &[provider.to_str().unwrap(), markdir.to_str().unwrap()],
        "10s",
        "daemon",
        &["/bin/sh", "-c", "exit 9"],
    )
    .status()
    .expect("run supervise");
    assert_eq!(status.code(), Some(9));
}

/// Spawn a supervised workload, wait for its start marker, SIGTERM the
/// supervisor, and return the supervisor's final status.
fn sigterm_supervised_workload(
    markdir: &Path,
    provider: &Path,
    workload_script: &str,
) -> std::process::ExitStatus {
    let mut child = supervise_command(
        &[provider.to_str().unwrap(), markdir.to_str().unwrap()],
        "10s",
        "daemon",
        &["/bin/sh", "-c", workload_script],
    )
    .spawn()
    .expect("spawn supervise");
    assert!(
        wait_for_file(&markdir.join("workload-ran"), Duration::from_secs(20)),
        "workload never started"
    );
    // SAFETY: child.id() is our direct, still-running child.
    let rc = unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    assert_eq!(rc, 0, "SIGTERM to the supervisor failed");
    child.wait().expect("wait supervise")
}

#[test]
#[ignore = "requires privileged Linux sandbox capabilities"]
fn sigterm_reaches_a_cooperating_workload_and_teardown_runs() {
    require_capability(
        unshare_net_usable(),
        "unshare --net / CAP_SYS_ADMIN unavailable",
    );
    let tmp = tempfile::tempdir().expect("tempdir");
    let markdir = tmp.path();
    let provider = write_provider_script(markdir, "touch \"$markdir/$phase-ran\"\nexit 0");

    // The workload is PID 1 of its PID namespace, and a namespace init only
    // receives signals it has handlers for — so a graceful shutdown requires a
    // cooperating workload, like every real agent harness. `sleep & wait` lets
    // the shell run the trap immediately instead of after sleep finishes.
    let status = sigterm_supervised_workload(
        markdir,
        &provider,
        &format!(
            "trap 'exit 7' TERM; touch {}/workload-ran; sleep 60 & wait $!",
            markdir.display()
        ),
    );

    assert_eq!(
        status.code(),
        Some(7),
        "the forwarded SIGTERM must reach the workload and its exit must be mirrored, got {status:?}"
    );
    assert!(
        markdir.join("teardown-ran").exists(),
        "teardown must run on signal shutdown"
    );
}

#[test]
#[ignore = "requires privileged Linux sandbox capabilities"]
fn sigterm_escalates_to_kill_for_a_stubborn_workload() {
    require_capability(
        unshare_net_usable(),
        "unshare --net / CAP_SYS_ADMIN unavailable",
    );
    let tmp = tempfile::tempdir().expect("tempdir");
    let markdir = tmp.path();
    let provider = write_provider_script(markdir, "touch \"$markdir/$phase-ran\"\nexit 0");

    // No trap: as namespace init the workload discards the SIGTERM entirely,
    // so the supervisor's grace window must expire and SIGKILL the chain —
    // shutdown never hangs on an uncooperative workload.
    let started = Instant::now();
    let status = sigterm_supervised_workload(
        markdir,
        &provider,
        &format!("touch {}/workload-ran && sleep 60", markdir.display()),
    );

    assert_eq!(
        status.signal(),
        Some(libc::SIGKILL),
        "the escalation SIGKILL must be mirrored, got {status:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "shutdown must complete within the grace escalation, not wait out the workload"
    );
    assert!(
        markdir.join("teardown-ran").exists(),
        "teardown must run even on escalated shutdown"
    );
}

#[test]
#[ignore = "requires privileged Linux sandbox capabilities"]
fn namespace_handle_stays_usable_during_teardown() {
    require_capability(
        unshare_net_usable(),
        "unshare --net / CAP_SYS_ADMIN unavailable",
    );
    require_capability(bin_available("nsenter", "--version"), "nsenter unavailable");
    let tmp = tempfile::tempdir().expect("tempdir");
    let markdir = tmp.path();
    // Teardown runs after the workload (and the whole unshare chain) has died;
    // entering the namespace still works because the supervisor's fd holds it.
    let provider = write_provider_script(
        markdir,
        "if [ \"$phase\" = teardown ]; then\n\
         nsenter --net=\"$ACPS_SANDBOX_NETWORK_NAMESPACE\" true || exit 1\n\
         touch \"$markdir/teardown-entered-ns\"\nfi\nexit 0",
    );

    let status = supervise_command(
        &[provider.to_str().unwrap(), markdir.to_str().unwrap()],
        "10s",
        "daemon",
        &["/bin/true"],
    )
    .status()
    .expect("run supervise");
    assert_eq!(status.code(), Some(0));
    assert!(markdir.join("teardown-entered-ns").exists());
}

#[test]
#[ignore = "requires privileged Linux sandbox capabilities"]
fn provider_env_is_exactly_the_contract() {
    require_capability(
        unshare_net_usable(),
        "unshare --net / CAP_SYS_ADMIN unavailable",
    );
    let tmp = tempfile::tempdir().expect("tempdir");
    let markdir = tmp.path();
    let provider = write_provider_script(markdir, "env | sort > \"$markdir/env-$phase\"\nexit 0");

    let status = supervise_command(
        &[provider.to_str().unwrap(), markdir.to_str().unwrap()],
        "10s",
        "daemon",
        &["/bin/true"],
    )
    .env("ACPS_TEST_AGENT_SECRET", "leak-me-if-you-can")
    .status()
    .expect("run supervise");
    assert_eq!(status.code(), Some(0));

    let setup_env = std::fs::read_to_string(markdir.join("env-setup")).expect("setup env dump");
    let teardown_env =
        std::fs::read_to_string(markdir.join("env-teardown")).expect("teardown env dump");
    for env in [&setup_env, &teardown_env] {
        assert!(
            !env.contains("ACPS_TEST_AGENT_SECRET"),
            "agent env leaked into the provider: {env}"
        );
        assert!(env.contains("ACPS_SANDBOX_NETWORK_PROTOCOL=1"));
        assert!(env.contains("ACPS_SANDBOX_NETWORK_ID="));
        assert!(env.contains("ACPS_SANDBOX_NETWORK_NAMESPACE=/proc/"));
    }
    assert!(
        setup_env.contains("ACPS_SANDBOX_NETWORK_PID="),
        "the namespace-owning pid is guaranteed during setup"
    );
    assert!(
        !teardown_env.contains("ACPS_SANDBOX_NETWORK_PID="),
        "the pid is not guaranteed during teardown and must not be exposed"
    );
    // Nothing beyond the contract; the shell itself adds PATH (the script's own
    // export) and bookkeeping vars like PWD/SHLVL/_ depending on which shell
    // backs /bin/sh.
    let shell_managed = ["PATH", "PWD", "OLDPWD", "SHLVL", "_"];
    for line in setup_env.lines() {
        let key = line.split('=').next().unwrap_or_default();
        assert!(
            key.starts_with("ACPS_SANDBOX_NETWORK_") || shell_managed.contains(&key),
            "unexpected provider env var `{line}`"
        );
    }
}

#[test]
#[ignore = "requires privileged Linux sandbox capabilities"]
fn provider_runs_from_a_trusted_cwd() {
    require_capability(
        unshare_net_usable(),
        "unshare --net / CAP_SYS_ADMIN unavailable",
    );
    let tmp = tempfile::tempdir().expect("tempdir");
    let markdir = tmp.path();
    let provider = write_provider_script(markdir, "pwd > \"$markdir/cwd-$phase\"\nexit 0");

    // The supervisor itself runs from an agent-writable cwd (as it does for
    // mediated commands); the privileged provider must not inherit it.
    let status = supervise_command(
        &[provider.to_str().unwrap(), markdir.to_str().unwrap()],
        "10s",
        "daemon",
        &["/bin/true"],
    )
    .current_dir(markdir)
    .status()
    .expect("run supervise");
    assert_eq!(status.code(), Some(0));

    for phase in ["setup", "teardown"] {
        let cwd = std::fs::read_to_string(markdir.join(format!("cwd-{phase}")))
            .expect("provider cwd dump");
        assert_eq!(
            cwd.trim(),
            "/",
            "the provider must run from / during {phase}, not the workload cwd"
        );
    }
}

#[test]
#[ignore = "requires privileged Linux sandbox capabilities"]
fn sigkill_of_the_supervisor_kills_the_chain() {
    require_capability(
        unshare_net_usable(),
        "unshare --net / CAP_SYS_ADMIN unavailable",
    );
    let tmp = tempfile::tempdir().expect("tempdir");
    let markdir = tmp.path();
    let provider = write_provider_script(markdir, "exit 0");

    let mut child = supervise_command(
        &[provider.to_str().unwrap(), markdir.to_str().unwrap()],
        "10s",
        "daemon",
        &[
            "/bin/sh",
            "-c",
            &format!("touch {}/workload-ran && sleep 60", markdir.display()),
        ],
    )
    .spawn()
    .expect("spawn supervise");
    assert!(
        wait_for_file(&markdir.join("workload-ran"), Duration::from_secs(20)),
        "workload never started"
    );
    let supervisor_pid = child.id() as i32;
    let unshare_pid: i32 = std::fs::read_to_string(format!(
        "/proc/{supervisor_pid}/task/{supervisor_pid}/children"
    ))
    .expect("read supervisor children")
    .split_whitespace()
    .next()
    .expect("supervisor has an unshare child")
    .parse()
    .expect("child pid parses");

    // SIGKILL bypasses every supervisor cleanup path; PR_SET_PDEATHSIG on the
    // chain is the only thing standing between this and an orphaned workload.
    // SAFETY: supervisor_pid is our direct, still-running child.
    let rc = unsafe { libc::kill(supervisor_pid, libc::SIGKILL) };
    assert_eq!(rc, 0, "SIGKILL to the supervisor failed");
    let status = child.wait().expect("wait supervise");
    assert_eq!(status.signal(), Some(libc::SIGKILL));

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        // cmdline read failing (or coming back empty, for a zombie) means the
        // unshare process is gone; matching on content guards pid reuse.
        let alive = std::fs::read_to_string(format!("/proc/{unshare_pid}/cmdline"))
            .map(|cmdline| cmdline.contains("unshare"))
            .unwrap_or(false);
        if !alive {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the unshare chain survived the supervisor's SIGKILL — pdeathsig did not fire"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
#[ignore = "requires privileged Linux sandbox capabilities"]
fn sigkill_during_provider_setup_kills_provider_group() {
    require_capability(
        unshare_net_usable(),
        "unshare --net / CAP_SYS_ADMIN unavailable",
    );
    let tmp = tempfile::tempdir().expect("tempdir");
    let markdir = tmp.path();
    let workload_marker = markdir.join("workload-ran");
    let provider = write_provider_script(
        markdir,
        "if [ \"$phase\" = setup ]; then\n\
         sleep 60 &\nchild=$!\nprintf '%s %s\\n' \"$$\" \"$child\" > \"$markdir/provider-setup-pids\"\nwait \"$child\"\nfi\nexit 0",
    );

    let mut child = supervise_command(
        &[provider.to_str().unwrap(), markdir.to_str().unwrap()],
        "30s",
        "daemon",
        &[
            "/bin/sh",
            "-c",
            &format!("touch {}", workload_marker.display()),
        ],
    )
    .spawn()
    .expect("spawn supervise");
    let pid_marker = markdir.join("provider-setup-pids");
    assert!(
        wait_for_file(&pid_marker, Duration::from_secs(20)),
        "provider setup never reached its blocking child"
    );
    let provider_processes = read_provider_processes(&pid_marker);
    let provider_pid = provider_processes[0].0;
    let monitor_pid = process_parent(provider_pid).expect("provider monitor pid");
    let supervisor_pid = child.id() as i32;
    let unshare_pid = process_children(supervisor_pid)
        .into_iter()
        .find(|pid| *pid != monitor_pid)
        .expect("supervisor unshare child");
    let workload_pid = process_children(unshare_pid)
        .into_iter()
        .next()
        .expect("unshare workload child");
    let mut tracked = provider_processes.to_vec();
    tracked.extend([
        tracked_process(monitor_pid),
        tracked_process(unshare_pid),
        tracked_process(workload_pid),
    ]);

    // SAFETY: child is our direct, still-running supervisor process.
    assert_eq!(unsafe { libc::kill(supervisor_pid, libc::SIGKILL) }, 0);
    assert_eq!(
        child.wait().expect("wait supervisor").signal(),
        Some(libc::SIGKILL)
    );
    for (pid, start_time) in tracked {
        assert!(
            wait_for_process_identity_gone(pid, start_time, Duration::from_secs(10)),
            "sandbox process {pid} survived supervisor SIGKILL during setup"
        );
    }
    assert!(
        !workload_marker.exists(),
        "workload ran before provider setup completed"
    );
}

#[test]
#[ignore = "requires privileged Linux sandbox capabilities"]
fn sigkill_of_provider_monitor_kills_group_and_fails_setup() {
    require_capability(
        unshare_net_usable(),
        "unshare --net / CAP_SYS_ADMIN unavailable",
    );
    let tmp = tempfile::tempdir().expect("tempdir");
    let markdir = tmp.path();
    let workload_marker = markdir.join("workload-ran");
    let provider = write_provider_script(
        markdir,
        "if [ \"$phase\" = setup ]; then\n\
         sleep 60 &\nchild=$!\nprintf '%s %s\\n' \"$$\" \"$child\" > \"$markdir/provider-setup-pids\"\nwait \"$child\"\nfi\nexit 0",
    );

    let mut child = supervise_command(
        &[provider.to_str().unwrap(), markdir.to_str().unwrap()],
        "30s",
        "daemon",
        &[
            "/bin/sh",
            "-c",
            &format!("touch {}", workload_marker.display()),
        ],
    )
    .spawn()
    .expect("spawn supervise");
    let pid_marker = markdir.join("provider-setup-pids");
    assert!(
        wait_for_file(&pid_marker, Duration::from_secs(20)),
        "provider setup never reached its blocking child"
    );
    let provider_processes = read_provider_processes(&pid_marker);
    let monitor_pid = process_parent(provider_processes[0].0).expect("provider monitor pid");

    // SAFETY: the monitor is a live process identified through its provider.
    assert_eq!(unsafe { libc::kill(monitor_pid, libc::SIGKILL) }, 0);
    assert_eq!(
        child.wait().expect("wait supervisor").code(),
        Some(SETUP_FAILED_EXIT)
    );
    for (pid, start_time) in provider_processes {
        assert!(
            wait_for_process_identity_gone(pid, start_time, Duration::from_secs(10)),
            "provider process {pid} survived monitor SIGKILL"
        );
    }
    assert!(
        !workload_marker.exists(),
        "workload ran after its provider monitor died during setup"
    );
}

#[test]
#[ignore = "requires privileged Linux sandbox capabilities"]
fn sigkill_during_provider_teardown_kills_provider_group() {
    require_capability(
        unshare_net_usable(),
        "unshare --net / CAP_SYS_ADMIN unavailable",
    );
    let tmp = tempfile::tempdir().expect("tempdir");
    let markdir = tmp.path();
    let workload_pid_marker = markdir.join("workload-pid");
    let provider = write_provider_script(
        markdir,
        "if [ \"$phase\" = teardown ]; then\n\
         sleep 60 &\nchild=$!\nprintf '%s %s\\n' \"$$\" \"$child\" > \"$markdir/provider-teardown-pids\"\nwait \"$child\"\nfi\nexit 0",
    );

    let mut child = supervise_command(
        &[provider.to_str().unwrap(), markdir.to_str().unwrap()],
        "30s",
        "daemon",
        &[
            "/bin/sh",
            "-c",
            &format!(
                "printf '%s\\n' \"$$\" > {} && sleep 1",
                workload_pid_marker.display()
            ),
        ],
    )
    .spawn()
    .expect("spawn supervise");
    assert!(
        wait_for_file(&workload_pid_marker, Duration::from_secs(20)),
        "workload never recorded its pid"
    );
    let workload_pid: i32 = std::fs::read_to_string(&workload_pid_marker)
        .expect("read workload pid")
        .trim()
        .parse()
        .expect("workload pid parses");
    let unshare_pid = process_parent(workload_pid).expect("workload unshare parent");
    let workload_chain = [tracked_process(unshare_pid), tracked_process(workload_pid)];
    let pid_marker = markdir.join("provider-teardown-pids");
    assert!(
        wait_for_file(&pid_marker, Duration::from_secs(20)),
        "provider teardown never reached its blocking child"
    );
    let provider_processes = read_provider_processes(&pid_marker);
    let monitor_pid = process_parent(provider_processes[0].0).expect("provider monitor pid");
    let mut tracked = provider_processes.to_vec();
    tracked.push(tracked_process(monitor_pid));
    tracked.extend(workload_chain);

    // SAFETY: child is our direct, still-running supervisor process.
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGKILL) }, 0);
    assert_eq!(
        child.wait().expect("wait supervisor").signal(),
        Some(libc::SIGKILL)
    );
    for (pid, start_time) in tracked {
        assert!(
            wait_for_process_identity_gone(pid, start_time, Duration::from_secs(10)),
            "sandbox process {pid} survived supervisor SIGKILL during teardown"
        );
    }
}

#[test]
#[ignore = "requires privileged Linux sandbox capabilities"]
fn sigterm_during_teardown_does_not_abort_it() {
    require_capability(
        unshare_net_usable(),
        "unshare --net / CAP_SYS_ADMIN unavailable",
    );
    let tmp = tempfile::tempdir().expect("tempdir");
    let markdir = tmp.path();
    let provider = write_provider_script(
        markdir,
        "if [ \"$phase\" = teardown ]; then\n\
         touch \"$markdir/teardown-started\"\nsleep 2\ntouch \"$markdir/teardown-done\"\nfi\nexit 0",
    );

    let mut child = supervise_command(
        &[provider.to_str().unwrap(), markdir.to_str().unwrap()],
        "10s",
        "daemon",
        &["/bin/true"],
    )
    .spawn()
    .expect("spawn supervise");
    assert!(
        wait_for_file(&markdir.join("teardown-started"), Duration::from_secs(20)),
        "teardown never started"
    );
    // SAFETY: child.id() is our direct, still-running child.
    let rc = unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    assert_eq!(rc, 0, "SIGTERM to the supervisor failed");

    // Teardown is cleanup: a shutdown signal must not cut it short (that would
    // guarantee a host-side leak), and the workload's real status survives.
    let status = child.wait().expect("wait supervise");
    assert_eq!(
        status.code(),
        Some(0),
        "the workload's exit status must be mirrored, not the shutdown signal: {status:?}"
    );
    assert!(
        markdir.join("teardown-done").exists(),
        "teardown must run to completion despite the SIGTERM"
    );
}

#[test]
#[ignore = "requires privileged Linux sandbox capabilities"]
fn provider_stdout_is_discarded_and_stderr_is_routed() {
    require_capability(
        unshare_net_usable(),
        "unshare --net / CAP_SYS_ADMIN unavailable",
    );
    let tmp = tempfile::tempdir().expect("tempdir");
    let markdir = tmp.path();
    let provider = write_provider_script(
        markdir,
        "echo PROVIDER-STDOUT-MARKER\necho PROVIDER-STDERR-MARKER >&2\nexit 0",
    );
    let provider_args = [provider.to_str().unwrap(), markdir.to_str().unwrap()];

    // `daemon` mode: stdout never reaches the workload streams; stderr reaches
    // the diagnostic fd (wired to the test's stderr pipe here).
    let output = supervise_command(
        &provider_args,
        "10s",
        "daemon",
        &["/bin/echo", "WORKLOAD-OUT"],
    )
    .output()
    .expect("run supervise");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0));
    assert!(stdout.contains("WORKLOAD-OUT"));
    assert!(
        !stdout.contains("PROVIDER-STDOUT-MARKER"),
        "provider stdout leaked into the workload stdout stream: {stdout}"
    );
    assert!(
        stderr.contains("PROVIDER-STDERR-MARKER"),
        "provider stderr must reach the diagnostic channel in daemon mode: {stderr}"
    );

    // `null` mode: provider stderr is discarded too.
    let output = supervise_command(
        &provider_args,
        "10s",
        "null",
        &["/bin/echo", "WORKLOAD-OUT"],
    )
    .output()
    .expect("run supervise");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0));
    assert!(!stdout.contains("PROVIDER-STDOUT-MARKER"));
    assert!(
        !stderr.contains("PROVIDER-STDERR-MARKER"),
        "provider stderr must be discarded in null mode: {stderr}"
    );
}

#[test]
#[ignore = "requires privileged Linux sandbox capabilities"]
fn workload_namespace_matches_supervisor_handle_and_ids_are_unique() {
    require_capability(
        unshare_net_usable(),
        "unshare --net / CAP_SYS_ADMIN unavailable",
    );
    let tmp = tempfile::tempdir().expect("tempdir");
    let markdir = tmp.path();
    let provider = write_provider_script(
        markdir,
        "if [ \"$phase\" = setup ]; then\n\
         readlink \"$ACPS_SANDBOX_NETWORK_NAMESPACE\" > \"$markdir/handle-ns-$ACPS_SANDBOX_NETWORK_ID\"\n\
         echo \"$ACPS_SANDBOX_NETWORK_ID\" >> \"$markdir/ids\"\nfi\nexit 0",
    );
    let provider_args = [provider.to_str().unwrap(), markdir.to_str().unwrap()];

    // Two overlapping spawns: distinct IDs, distinct namespaces, and each
    // workload's own ns/net equals the handle its supervisor captured.
    let spawn = |tag: &str| {
        supervise_command(
            &provider_args,
            "10s",
            "daemon",
            &[
                "/bin/sh",
                "-c",
                &format!(
                    "readlink /proc/self/ns/net > {}/workload-ns-{tag} && sleep 1",
                    markdir.display()
                ),
            ],
        )
        .spawn()
        .expect("spawn supervise")
    };
    let mut first = spawn("a");
    let mut second = spawn("b");
    assert!(first.wait().expect("wait first").success());
    assert!(second.wait().expect("wait second").success());

    let ids: Vec<String> = std::fs::read_to_string(markdir.join("ids"))
        .expect("ids file")
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(ids.len(), 2);
    assert_ne!(ids[0], ids[1], "concurrent spawns must get distinct IDs");

    let mut handle_inodes: Vec<String> = ids
        .iter()
        .map(|id| {
            std::fs::read_to_string(markdir.join(format!("handle-ns-{id}")))
                .expect("handle ns dump")
                .trim()
                .to_owned()
        })
        .collect();
    let mut workload_inodes: Vec<String> = ["a", "b"]
        .iter()
        .map(|tag| {
            std::fs::read_to_string(markdir.join(format!("workload-ns-{tag}")))
                .expect("workload ns dump")
                .trim()
                .to_owned()
        })
        .collect();
    assert_ne!(
        workload_inodes[0], workload_inodes[1],
        "concurrent spawns must get distinct namespaces"
    );
    handle_inodes.sort();
    workload_inodes.sort();
    assert_eq!(
        handle_inodes, workload_inodes,
        "the supervisor handle must name the workload's own netns"
    );
}

#[test]
#[ignore = "requires privileged Linux sandbox capabilities"]
fn deny_all_namespace_cannot_reach_a_parent_listener() {
    require_capability(
        unshare_net_usable(),
        "unshare --net / CAP_SYS_ADMIN unavailable",
    );
    require_capability(
        bin_available("bash", "--version"),
        "bash (for /dev/tcp) unavailable",
    );
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind parent listener");
    let port = listener.local_addr().expect("listener addr").port();
    // Distinct sentinel exit codes so a tooling failure (bash missing, wrapper
    // error, exit 120/121) can never masquerade as "unreachable".
    let connect = format!("if exec 3<>/dev/tcp/127.0.0.1/{port}; then exit 42; else exit 43; fi");

    // Positive control: reachable from the parent namespace.
    let direct = Command::new("bash")
        .args(["-c", &connect])
        .status()
        .expect("run direct connect");
    assert_eq!(
        direct.code(),
        Some(42),
        "control connect must succeed outside"
    );

    // Isolated with no provider: deny-all, not even loopback.
    let status = supervise_command(&[], "10s", "daemon", &["bash", "-c", &connect])
        .status()
        .expect("run supervise");
    assert_eq!(
        status.code(),
        Some(43),
        "a deny-all namespace must fail the connect itself (not a tooling error)"
    );
}

#[test]
#[ignore = "requires privileged Linux sandbox capabilities"]
fn veth_provider_enables_only_the_configured_endpoint() {
    require_capability(
        unshare_net_usable(),
        "unshare --net / CAP_SYS_ADMIN unavailable",
    );
    require_capability(
        bin_available("ip", "-V") && bin_available("nsenter", "--version"),
        "ip/nsenter unavailable",
    );
    // CAP_NET_ADMIN probe: creating (and removing) a throwaway link.
    let probe = Command::new("ip")
        .args([
            "link",
            "add",
            "acpsprobe0",
            "type",
            "veth",
            "peer",
            "name",
            "acpsprobe1",
        ])
        .stderr(Stdio::null())
        .status()
        .expect("run ip probe");
    if !probe.success() {
        require_capability(false, "CAP_NET_ADMIN unavailable (cannot create veth)");
    } else {
        let removed = Command::new("ip")
            .args(["link", "del", "acpsprobe0"])
            .status()
            .expect("remove ip probe");
        assert!(removed.success());
    }
    // Pre-clean a stale interface a previously panicked/killed run may have
    // left behind, so setup does not fail 120 for stale-state reasons.
    let precleaned = Command::new("ip")
        .args(["link", "del", "acpstest0"])
        .stderr(Stdio::null())
        .status()
        .expect("pre-clean stale veth");
    if precleaned.success() {
        eprintln!("note: removed stale acpstest0 from a previous run");
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let markdir = tmp.path();
    // Lifecycle provider: veth pair, child end moved into the namespace and
    // addressed; only 10.199.99.1 is reachable from inside. Teardown is
    // idempotent (`|| true`): namespace destruction usually removed the pair.
    let provider = write_provider_script(
        markdir,
        "if [ \"$phase\" = setup ]; then\n\
         ip link add acpstest0 type veth peer name acpstest1 || exit 1\n\
         ip link set acpstest1 netns \"$ACPS_SANDBOX_NETWORK_PID\" || exit 1\n\
         ip addr add 10.199.99.1/30 dev acpstest0 || exit 1\n\
         ip link set acpstest0 up || exit 1\n\
         nsenter --net=\"$ACPS_SANDBOX_NETWORK_NAMESPACE\" sh -c \
         'ip link set lo up && ip addr add 10.199.99.2/30 dev acpstest1 && ip link set acpstest1 up' || exit 1\n\
         fi\n\
         if [ \"$phase\" = teardown ]; then ip link del acpstest0 2>/dev/null || true; fi\n\
         exit 0",
    );

    // Sentinel codes prove which probe failed: the configured endpoint must
    // answer, and an address outside the provider's /30 must NOT be reachable
    // — the "only" half of the guarantee.
    let status = supervise_command(
        &[provider.to_str().unwrap(), markdir.to_str().unwrap()],
        "10s",
        "daemon",
        &[
            "/bin/sh",
            "-c",
            "ping -c 1 -W 2 10.199.99.1 || exit 44; \
             ping -c 1 -W 1 10.88.77.66 && exit 45; \
             exit 0",
        ],
    )
    .status()
    .expect("run supervise");
    assert_eq!(
        status.code(),
        Some(0),
        "44 = configured endpoint unreachable, 45 = unconfigured address reachable"
    );

    // Exiting destroyed the namespace, which destroyed the veth peer — the
    // host-side interface must be gone (teardown also deletes it explicitly).
    let leftover = Command::new("ip")
        .args(["link", "show", "acpstest0"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("check leftover veth");
    assert!(
        !leftover.success(),
        "the host-side veth must not survive the spawn"
    );
}
