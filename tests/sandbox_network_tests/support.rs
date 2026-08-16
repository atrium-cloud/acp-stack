use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// True when this process can create the namespaces (incl. `--net`) the
/// isolated wrapper relies on.
pub(crate) fn unshare_net_usable() -> bool {
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
pub(crate) fn require_capability(available: bool, reason: &str) {
    assert!(
        available,
        "required sandbox test capability missing on this runner: {reason}"
    );
}

/// `version_arg` differs per tool: iproute2's `ip` only understands `-V`.
pub(crate) fn bin_available(name: &str, version_arg: &str) -> bool {
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
pub(crate) fn supervise_command(
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
pub(crate) fn write_provider_script(dir: &Path, body: &str) -> PathBuf {
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

pub(crate) fn wait_for_file(path: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    path.exists()
}

pub(crate) fn process_identity(pid: i32) -> Option<(char, u64)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, fields) = stat.rsplit_once(") ")?;
    let fields: Vec<&str> = fields.split_whitespace().collect();
    let state = fields.first()?.chars().next()?;
    let start_time = fields.get(19)?.parse().ok()?;
    Some((state, start_time))
}

pub(crate) fn process_parent(pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, fields) = stat.rsplit_once(") ")?;
    fields.split_whitespace().nth(1)?.parse().ok()
}

pub(crate) fn process_children(pid: i32) -> Vec<i32> {
    std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"))
        .expect("read process children")
        .split_whitespace()
        .map(|value| value.parse().expect("child pid parses"))
        .collect()
}

pub(crate) fn tracked_process(pid: i32) -> (i32, u64) {
    let (_, start_time) = process_identity(pid).expect("tracked process must be live");
    (pid, start_time)
}

pub(crate) fn wait_for_process_identity_gone(pid: i32, start_time: u64, timeout: Duration) -> bool {
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

pub(crate) fn read_provider_processes(path: &Path) -> [(i32, u64); 2] {
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
