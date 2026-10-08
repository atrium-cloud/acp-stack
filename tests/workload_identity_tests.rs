//! End-to-end workload identity guarantees: the wrapped workload runs as the declared user with no
//! capabilities, and the runtime stops its whole process tree under `unshare` and `off`. Requires
//! Linux, a provisioned user named by `ACPS_TEST_WORKLOAD_USER`, and a runtime holding
//! CAP_SETUID, CAP_SETGID and CAP_SETPCAP (plus CAP_SYS_ADMIN for `unshare` and a writable cgroup
//! v2 parent for `off`).

#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use acp_stack::config::{SandboxConfig, SandboxMode};
use acp_stack::runtime::sandbox::{SandboxProfile, WorkloadCgroup, WrappedCommand, wrap};

const WORKLOAD_USER_ENV: &str = "ACPS_TEST_WORKLOAD_USER";
const SETTLE: Duration = Duration::from_secs(1);
const REAP_DEADLINE: Duration = Duration::from_secs(5);

fn profile(mode: SandboxMode) -> SandboxProfile {
    let user = std::env::var(WORKLOAD_USER_ENV)
        .unwrap_or_else(|_| panic!("required test input missing: set {WORKLOAD_USER_ENV}"));
    SandboxProfile::resolve(&SandboxConfig {
        mode,
        workload_user: Some(user),
        ..SandboxConfig::default()
    })
    .expect("the workload user must resolve")
}

/// `wrap` names the running executable as the `__sandbox-exec` helper; under `cargo test` that is
/// the test binary, so point it at the built `acps` instead.
fn runnable(wrapped: WrappedCommand) -> Command {
    let test_binary = std::env::current_exe().expect("current exe");
    let acps = env!("CARGO_BIN_EXE_acps");
    let mut command = Command::new(&wrapped.program);
    for arg in wrapped.args {
        if Path::new(&arg) == test_binary {
            command.arg(acps);
        } else {
            command.arg(arg);
        }
    }
    command
}

fn wrapped_shell(profile: &SandboxProfile, script: &str) -> Command {
    let home = tempfile::tempdir().expect("runtime home");
    let wrapped = wrap(
        profile,
        None,
        Path::new("/bin/sh"),
        &["-c".to_owned(), script.to_owned()],
        home.path(),
        Path::new("/"),
    )
    .expect("wrap");
    runnable(wrapped)
}

fn status_field<'a>(output: &'a str, field: &str) -> &'a str {
    output
        .lines()
        .find_map(|line| line.strip_prefix(field))
        .map(str::trim)
        .unwrap_or_else(|| panic!("`{field}` missing from workload output: {output}"))
}

fn assert_runs_as_identity_without_capabilities(profile: &SandboxProfile) {
    let identity = profile.identity.clone().expect("identity");
    let output = wrapped_shell(
        profile,
        "echo uid: $(id -u); echo gid: $(id -g); \
         grep -E '^(Groups|CapEff|CapPrm|CapBnd|CapAmb|NoNewPrivs):' /proc/self/status",
    )
    .output()
    .expect("run the wrapped workload");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "workload failed: {stdout} {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(status_field(&stdout, "uid:"), identity.uid.to_string());
    assert_eq!(status_field(&stdout, "gid:"), identity.gid.to_string());
    assert_eq!(
        status_field(&stdout, "Groups:"),
        "",
        "no supplementary groups"
    );
    for set in ["CapEff:", "CapPrm:", "CapBnd:", "CapAmb:"] {
        assert_eq!(status_field(&stdout, set), "0000000000000000", "{set}");
    }
    assert_eq!(status_field(&stdout, "NoNewPrivs:"), "1");
}

/// Live processes owned by `uid` whose command line carries `marker`.
fn marked_processes(uid: u32, marker: &str) -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| {
            let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
            let owner = status
                .lines()
                .find_map(|line| line.strip_prefix("Uid:"))
                .and_then(|ids| ids.split_whitespace().next())
                .and_then(|real| real.parse::<u32>().ok());
            let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
            owner == Some(uid) && String::from_utf8_lossy(&cmdline).contains(marker)
        })
        .collect()
}

fn wait_until_gone(uid: u32, marker: &str) -> Vec<u32> {
    let deadline = Instant::now() + REAP_DEADLINE;
    loop {
        let alive = marked_processes(uid, marker);
        if alive.is_empty() || Instant::now() >= deadline {
            return alive;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A workload that forks a plain child and a `setsid` grandchild, each sleeping on `marker`.
fn forking_script(marker: &str) -> String {
    format!("sleep {marker} & setsid sh -c 'sleep {marker} & wait' & wait")
}

#[test]
#[ignore = "requires Linux workload identity capabilities and ACPS_TEST_WORKLOAD_USER"]
fn unshare_runs_the_workload_as_the_identity_without_capabilities() {
    assert_runs_as_identity_without_capabilities(&profile(SandboxMode::Unshare));
}

#[test]
#[ignore = "requires Linux workload identity capabilities and ACPS_TEST_WORKLOAD_USER"]
fn off_runs_the_workload_as_the_identity_without_capabilities() {
    assert_runs_as_identity_without_capabilities(&profile(SandboxMode::Off));
}

#[test]
#[ignore = "requires Linux workload identity capabilities and ACPS_TEST_WORKLOAD_USER"]
fn unshare_teardown_stops_a_setsid_grandchild() {
    let profile = profile(SandboxMode::Unshare);
    let uid = profile.identity.as_ref().expect("identity").uid;
    let marker = "3141.5";
    let mut child = wrapped_shell(&profile, &forking_script(marker))
        .spawn()
        .expect("spawn the wrapped workload");
    std::thread::sleep(SETTLE);
    assert!(
        !marked_processes(uid, marker).is_empty(),
        "the workload tree must be running before teardown"
    );
    // The direct child is `unshare`, owned by the runtime, so this is a same-uid signal; the
    // re-armed parent-death signal carries it into the namespace.
    child.kill().expect("kill unshare");
    child.wait().expect("reap unshare");
    let survivors = wait_until_gone(uid, marker);
    assert!(
        survivors.is_empty(),
        "workload processes survived: {survivors:?}"
    );
}

#[tokio::test]
#[ignore = "requires Linux workload identity capabilities and ACPS_TEST_WORKLOAD_USER"]
async fn off_cgroup_kill_stops_a_setsid_grandchild() {
    let profile = profile(SandboxMode::Off);
    let uid = profile.identity.as_ref().expect("identity").uid;
    let marker = "2718.2";
    let cgroup = WorkloadCgroup::for_profile(&profile)
        .expect("create the workload cgroup")
        .expect("off with an identity uses a cgroup");
    let std_command = wrapped_shell(&profile, &forking_script(marker));
    let mut command = tokio::process::Command::from(std_command);
    cgroup.enter_before_exec(&mut command);
    let mut child = command.spawn().expect("spawn the wrapped workload");
    tokio::time::sleep(SETTLE).await;
    assert!(
        !marked_processes(uid, marker).is_empty(),
        "the workload tree must be running before cgroup.kill"
    );
    cgroup.kill();
    child.wait().await.expect("reap the workload");
    let survivors = wait_until_gone(uid, marker);
    assert!(
        survivors.is_empty(),
        "workload processes survived: {survivors:?}"
    );
}

#[test]
#[ignore = "requires Linux workload identity capabilities and ACPS_TEST_WORKLOAD_USER"]
fn the_workload_path_drops_directories_the_identity_can_write() {
    let profile = profile(SandboxMode::Off);
    let shared = tempfile::tempdir().expect("shared dir");
    let writable = shared.path().join("writable-bin");
    std::fs::create_dir(&writable).expect("create");
    std::fs::set_permissions(
        &writable,
        std::os::unix::fs::PermissionsExt::from_mode(0o777),
    )
    .expect("make world-writable");
    let kept = profile.without_workload_writable(vec![PathBuf::from("/usr/bin"), writable.clone()]);
    assert_eq!(kept, vec![PathBuf::from("/usr/bin")]);
}
