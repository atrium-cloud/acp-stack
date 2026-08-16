use std::time::{Duration, Instant};

use acp_stack::runtime::sandbox::supervise::{SETUP_FAILED_EXIT, TEARDOWN_FAILED_EXIT};

use crate::support::*;

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
