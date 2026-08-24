use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::support::*;

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

    // As PID 1 of its namespace the workload only receives signals it has
    // handlers for, so graceful shutdown needs a cooperating workload.
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

    // No trap: as namespace init the workload discards SIGTERM, so the grace
    // window must expire and SIGKILL the chain.
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
    // The provider configures the namespace, it does not route through it, so
    // it must not inherit `workload_env`.
    .env("HTTPS_PROXY", "http://127.0.0.1:3128")
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
        assert!(
            !env.contains("HTTPS_PROXY"),
            "workload_env leaked into the provider: {env}"
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
    // The shell itself adds PATH and bookkeeping vars like PWD/SHLVL/_,
    // depending on which shell backs /bin/sh.
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

    // The supervisor runs from an agent-writable cwd; the privileged provider
    // must not inherit it.
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
