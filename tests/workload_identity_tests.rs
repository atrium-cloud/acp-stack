//! End-to-end workload identity guarantees: the wrapped workload runs as the declared user with no
//! capabilities, the runtime stops its whole process tree under `unshare` and `off`, and workload
//! file I/O follows links with the identity's own credentials deciding what they reach. Requires
//! Linux, a provisioned user named by `ACPS_TEST_WORKLOAD_USER`, and a runtime holding
//! CAP_SETUID, CAP_SETGID and CAP_SETPCAP (plus CAP_SYS_ADMIN for `unshare` and a writable cgroup
//! v2 parent for `off`). The runtime must not hold CAP_DAC_OVERRIDE, or the permission checks below
//! pass for the wrong reason.

#![cfg(target_os = "linux")]

use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use acp_stack::config::{SandboxConfig, SandboxMode};
use acp_stack::error::{Result, StackError};
use acp_stack::runtime::agent::config_io::WorkloadHome;
use acp_stack::runtime::sandbox::{SandboxProfile, WorkloadCgroup, WrappedCommand, wrap};
use acp_stack::workload_fs::{self, Anchor, DEFAULT_JOB_TIMEOUT};
use acp_stack::workspace;

const WORKLOAD_USER_ENV: &str = "ACPS_TEST_WORKLOAD_USER";
const SETTLE: Duration = Duration::from_secs(1);
const REAP_DEADLINE: Duration = Duration::from_secs(5);
const MAX_READ: u64 = 1024 * 1024;
const PROTECTED_HARDLINKS: &str = "/proc/sys/fs/protected_hardlinks";

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

/// A runtime-owned workspace root the identity can write, as a deployment provisions it.
fn shared_root() -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("workspace root");
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o777))
        .expect("open the root to the workload user");
    root
}

/// Run `job` on the workspace anchor at `root` with the identity's credentials, as the workspace
/// API and ACP fs handlers do.
fn as_workload<T: Send + 'static>(
    profile: &SandboxProfile,
    root: &Path,
    job: impl FnOnce(&Anchor) -> Result<T> + Send + 'static,
) -> Result<T> {
    let root = root.to_path_buf();
    profile.executor().run(DEFAULT_JOB_TIMEOUT, move || {
        let anchor = workspace::open_root(&root, ".")?;
        job(&anchor)
    })
}

fn read_as_workload(profile: &SandboxProfile, root: &Path, path: &'static str) -> Result<Vec<u8>> {
    as_workload(profile, root, move |anchor| {
        workspace::read_file(anchor, Path::new(path), path, MAX_READ).map(|read| read.content)
    })
}

fn write_as_workload(
    profile: &SandboxProfile,
    root: &Path,
    path: &'static str,
    content: &'static [u8],
) -> Result<()> {
    let options = workspace::workload_write_options(profile);
    as_workload(profile, root, move |anchor| {
        workspace::write_file(anchor, Path::new(path), path, content, &options).map(drop)
    })
}

fn assert_permission_denied<T: std::fmt::Debug>(outcome: Result<T>, context: &str) {
    assert!(
        matches!(outcome, Err(StackError::WorkspacePermissionDenied { .. })),
        "{context}: expected a permission refusal, got {outcome:?}"
    );
}

#[test]
#[ignore = "requires Linux workload identity capabilities and ACPS_TEST_WORKLOAD_USER"]
fn workspace_links_resolve_and_write_through_as_the_identity() {
    let profile = profile(SandboxMode::Off);
    let uid = profile.identity.as_ref().expect("identity").uid;
    let root = shared_root();
    as_workload(&profile, root.path(), |anchor| {
        workload_fs::create_dir_all(anchor, Path::new("real"), workload_fs::UMASK_DIR_MODE)
    })
    .expect("seed a directory as the identity");
    write_as_workload(&profile, root.path(), "AGENTS.md", b"rules").expect("seed a file");
    symlink("AGENTS.md", root.path().join("CLAUDE.md")).expect("file link");
    symlink(root.path().join("real"), root.path().join("linked")).expect("dir link");

    assert_eq!(
        read_as_workload(&profile, root.path(), "CLAUDE.md").expect("read through the link"),
        b"rules"
    );
    write_as_workload(&profile, root.path(), "CLAUDE.md", b"updated").expect("write through");
    write_as_workload(&profile, root.path(), "linked/new.md", b"new").expect("write below");
    as_workload(&profile, root.path(), |anchor| {
        workspace::delete_file(anchor, Path::new("CLAUDE.md"), "CLAUDE.md")
    })
    .expect("delete the link");

    assert_eq!(
        std::fs::read(root.path().join("AGENTS.md")).expect("target"),
        b"updated"
    );
    let created = std::fs::metadata(root.path().join("real/new.md")).expect("created");
    assert_eq!(created.uid(), uid);
    assert!(std::fs::symlink_metadata(root.path().join("CLAUDE.md")).is_err());
}

#[test]
#[ignore = "requires Linux workload identity capabilities and ACPS_TEST_WORKLOAD_USER"]
fn links_into_runtime_owned_paths_fail_with_a_permission_error() {
    let profile = profile(SandboxMode::Off);
    let root = shared_root();
    let private = root.path().join("private");
    std::fs::create_dir(&private).expect("private dir");
    std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700))
        .expect("runtime-only dir");
    let secret = private.join("secret");
    std::fs::write(&secret, b"runtime").expect("secret");
    let readable = root.path().join("runtime.txt");
    std::fs::write(&readable, b"runtime").expect("readable");
    std::fs::set_permissions(&readable, std::fs::Permissions::from_mode(0o644))
        .expect("world-readable");
    let outside = tempfile::tempdir().expect("outside");
    symlink(&secret, root.path().join("target-link")).expect("target link");
    symlink(&private, root.path().join("parent-link")).expect("parent link");
    symlink(&readable, root.path().join("owned-link")).expect("owned link");
    symlink(outside.path(), root.path().join("escape")).expect("escape link");

    for path in ["target-link", "parent-link/secret"] {
        assert_permission_denied(
            read_as_workload(&profile, root.path(), path),
            &format!("read {path}"),
        );
        assert_permission_denied(
            write_as_workload(&profile, root.path(), path, b"planted"),
            &format!("write {path}"),
        );
    }
    let owned = write_as_workload(&profile, root.path(), "owned-link", b"planted");
    assert!(
        matches!(&owned, Err(StackError::WorkspacePathInvalid { reason, .. }) if reason.contains("owned by another user")),
        "{owned:?}"
    );
    let escaped = write_as_workload(&profile, root.path(), "escape/new", b"planted");
    assert!(
        matches!(escaped, Err(StackError::WorkspaceSymlinkEscape { .. })),
        "{escaped:?}"
    );

    assert_eq!(std::fs::read(&secret).expect("secret"), b"runtime");
    assert_eq!(std::fs::read(&readable).expect("readable"), b"runtime");
    assert_eq!(
        std::fs::read_dir(outside.path()).expect("outside").count(),
        0
    );
}

#[test]
#[ignore = "requires Linux workload identity capabilities and ACPS_TEST_WORKLOAD_USER"]
fn hard_links_to_runtime_files_grant_the_identity_nothing() {
    let protected = std::fs::read_to_string(PROTECTED_HARDLINKS).expect("read protected_hardlinks");
    assert_eq!(
        protected.trim(),
        "1",
        "this host must set fs.protected_hardlinks=1"
    );
    let profile = profile(SandboxMode::Off);
    let root = shared_root();
    let secret = root.path().join("runtime-secret");
    std::fs::write(&secret, b"runtime").expect("secret");
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600))
        .expect("runtime-only file");

    let source = secret.clone();
    let planted = root.path().join("planted");
    let attempt = profile
        .executor()
        .run(DEFAULT_JOB_TIMEOUT, move || {
            Ok(std::fs::hard_link(&source, &planted))
        })
        .expect("the job ran");
    assert_eq!(
        attempt
            .expect_err("protected_hardlinks refuses the link")
            .kind(),
        std::io::ErrorKind::PermissionDenied
    );

    std::fs::hard_link(&secret, root.path().join("linked")).expect("runtime-made hard link");
    assert_permission_denied(
        read_as_workload(&profile, root.path(), "linked"),
        "read a hard link",
    );
    let written = write_as_workload(&profile, root.path(), "linked", b"planted");
    assert!(
        matches!(&written, Err(StackError::WorkspacePathInvalid { reason, .. }) if reason.contains("owned by another user")),
        "{written:?}"
    );
    assert_eq!(std::fs::read(&secret).expect("secret"), b"runtime");
}

/// Removes a scratch tree below the workload home when the test ends.
struct HomeScratch<'a> {
    workload: &'a WorkloadHome,
    path: PathBuf,
}

impl Drop for HomeScratch<'_> {
    fn drop(&mut self) {
        if let Err(error) = self.workload.remove_tree(&self.path) {
            eprintln!("failed to remove {}: {error}", self.path.display());
        }
    }
}

#[test]
#[ignore = "requires Linux workload identity capabilities and ACPS_TEST_WORKLOAD_USER"]
fn native_config_under_the_workload_home_reads_and_writes_through_links() {
    let profile = profile(SandboxMode::Off);
    let runtime_home = tempfile::tempdir().expect("runtime home");
    let workload = WorkloadHome::for_profile(&profile, runtime_home.path());
    let scratch = HomeScratch {
        workload: &workload,
        path: workload
            .home()
            .join(format!(".acps-link-test-{}", std::process::id())),
    };
    let dotfiles = scratch.path.join("dotfiles");
    let linked_dir = scratch.path.join(".codex");
    let config = linked_dir.join("config.toml");
    workload.create_dir_all(&dotfiles).expect("dotfiles");
    workload
        .write_atomic(&dotfiles.join("config.toml"), b"model = \"a\"\n".to_vec())
        .expect("seed the dotfile");
    workload.symlink(&linked_dir, &dotfiles).expect("dir link");

    workload
        .prepare_owned_file(&config)
        .expect("prepare through the link");
    assert_eq!(
        workload.read(&config).expect("read"),
        Some(b"model = \"a\"\n".to_vec())
    );
    workload
        .write_atomic(&config, b"model = \"b\"\n".to_vec())
        .expect("write through the link");
    assert_eq!(
        workload.read(&dotfiles.join("config.toml")).expect("read"),
        Some(b"model = \"b\"\n".to_vec())
    );
    assert_eq!(
        workload
            .stat(&linked_dir)
            .expect("stat")
            .map(|info| info.kind),
        Some(workload_fs::EntryKind::Symlink)
    );
}
