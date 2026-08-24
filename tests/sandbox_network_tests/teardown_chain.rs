use std::os::unix::process::ExitStatusExt;
use std::time::{Duration, Instant};

use acp_stack::runtime::sandbox::supervise::SETUP_FAILED_EXIT;

use crate::support::*;

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

    // SIGKILL bypasses every supervisor cleanup path, so PR_SET_PDEATHSIG on the chain is all
    // that prevents an orphaned workload.
    // SAFETY: supervisor_pid is our direct, still-running child.
    let rc = unsafe { libc::kill(supervisor_pid, libc::SIGKILL) };
    assert_eq!(rc, 0, "SIGKILL to the supervisor failed");
    let status = child.wait().expect("wait supervise");
    assert_eq!(status.signal(), Some(libc::SIGKILL));

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        // A failed or empty cmdline read means the process is gone; matching content guards
        // against pid reuse.
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

    // Teardown is cleanup: a shutdown signal cutting it short would guarantee a host-side leak.
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
