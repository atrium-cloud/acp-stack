use crate::common::cli::*;
use acp_stack::config::{DependencyInstallScope, load_config_from_str};
use acp_stack::state::{StateStore, default_state_path};
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[test]
fn init_dep_flag_writes_user_scope_dependency() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--dep",
            "ripgrep=apt-get install -y ripgrep",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success();

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    let config = load_config_from_str(&written).expect("config should validate");
    let entry = config
        .dependencies
        .commands
        .iter()
        .find(|entry| entry.name == "ripgrep")
        .expect("ripgrep dependency should be declared");
    let install = entry
        .install
        .as_ref()
        .expect("dep should have install action");
    assert_eq!(install.shell, "apt-get install -y ripgrep");
    assert_eq!(install.scope, DependencyInstallScope::User);
}

#[test]
fn init_dep_system_flag_writes_system_scope() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--dep-system",
            "nginx=apt-get install -y nginx",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success();

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    let config = load_config_from_str(&written).expect("config should validate");
    let install = config
        .dependencies
        .commands
        .iter()
        .find(|entry| entry.name == "nginx")
        .and_then(|entry| entry.install.as_ref())
        .expect("nginx dependency should be declared with an install action");
    assert_eq!(install.scope, DependencyInstallScope::System);
}

#[test]
fn init_deps_apply_requires_yes_noninteractive() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--dep",
            "acpstack-absent-tool=true",
            "--deps-apply",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--deps-apply-yes"));
}

#[test]
fn init_deps_apply_runs_pending_action_and_surfaces_failure() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    // The tool is not on PATH, so the apply step runs its shell, which exits
    // non-zero.
    let output = acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--dep",
            "acpstack-failtool=exit 3",
            "--deps-apply",
            "--deps-apply-yes",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let stderr = String::from_utf8(output).expect("stderr should be utf8");
    assert!(
        stderr.contains("dependency apply produced failing actions"),
        "{stderr}"
    );
    assert!(
        stderr.contains("acpstack-failtool failed (exit=3)"),
        "{stderr}"
    );
}

#[test]
fn init_deps_apply_skips_system_scope_without_sudo_and_continues() {
    // SAFETY: `geteuid()` is always safe — no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        // As root the escalation probe runs directly, so the skip path under
        // test is unreachable.
        return;
    }
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    // A fake sudo that always exits 1 makes the escalation probe collapse to
    // Unavailable regardless of the host's real sudoers state.
    let fake_bin = tempdir.path().join("fake-bin");
    fs::create_dir_all(&fake_bin).expect("fake bin dir");
    let fake_sudo = fake_bin.join("sudo");
    fs::write(&fake_sudo, "#!/bin/sh\nexit 1\n").expect("fake sudo");
    #[cfg(unix)]
    fs::set_permissions(&fake_sudo, fs::Permissions::from_mode(0o755)).expect("chmod fake sudo");
    let host_path = std::env::var("PATH").expect("PATH should be set");
    let path_with_fake_sudo = format!("{}:{host_path}", fake_bin.to_string_lossy());

    // The system-scope action would succeed if it ran; it must instead be
    // skipped on privilege while init still completes.
    acps_command()
        .env("HOME", tempdir.path())
        .env("PATH", path_with_fake_sudo)
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--dep-system",
            "acpstack-absent-system-tool=exit 0",
            "--deps-apply",
            "--deps-apply-yes",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "no passwordless sudo; they will be skipped and recorded as privilege_required",
        ))
        .stdout(predicates::str::contains("sudo /bin/bash -c 'exit 0'"))
        .stdout(predicates::str::contains(
            "need root and were skipped (uid=",
        ))
        .stdout(predicates::str::contains(
            "resume with: acps init --resume --deps-apply --deps-apply-yes",
        ))
        .stdout(predicates::str::contains("initialized acp-stack"));

    let store =
        StateStore::open(default_state_path(tempdir.path())).expect("state store should open");
    let rows = store
        .query_installer_runs_filtered(Some("deps_apply"), 10)
        .expect("installer history should query");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].status, "privilege_required");
}
