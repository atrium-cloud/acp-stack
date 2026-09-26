use super::super::step_runners::select_install_path;
use super::super::*;
use super::support::*;
use crate::runtime::install::agent_registry::{ArchMap, GithubInstall, NpmInstall};
use crate::state::INSTALLER_STATUS_KEPT;
use tempfile::TempDir;

fn npm_install_set() -> InstallSet {
    InstallSet {
        npm: Some(NpmInstall {
            package: "@scope/agent".to_owned(),
            creates: "agent".to_owned(),
        }),
        ..InstallSet::default()
    }
}

fn pinned_npm_spec(version: &str) -> (String, Option<String>) {
    match select_install_path(
        "test",
        "harness.install",
        &npm_install_set(),
        None,
        Some(version),
    )
    .expect("resolve")
    {
        ResolvedInstallSpec::Npm {
            package, version, ..
        } => (package, version),
        other => panic!("expected Npm variant, got {other:?}"),
    }
}

#[test]
fn a_pin_reaches_npm_without_a_leading_v() {
    assert_eq!(
        pinned_npm_spec("v1.2.3"),
        ("@scope/agent@1.2.3".to_owned(), Some("1.2.3".to_owned()))
    );
    assert_eq!(
        pinned_npm_spec("1.2.3"),
        ("@scope/agent@1.2.3".to_owned(), Some("1.2.3".to_owned()))
    );
    // Only a `v` before a digit is a tag prefix; anything else is the version itself.
    assert_eq!(
        pinned_npm_spec("vnext"),
        ("@scope/agent@vnext".to_owned(), Some("vnext".to_owned()))
    );
}

#[test]
fn a_pin_reaches_github_verbatim_as_the_release_tag() {
    let install = InstallSet {
        github: Some(GithubInstall {
            asset_pattern: "agent-linux.tar.gz".to_owned(),
            archive: ArchiveKind::TarGz,
            archive_binary_name: None,
            bundle_binary_path: None,
            binary_name: "agent".to_owned(),
            checksums_asset: None,
            arch: ArchMap::default(),
        }),
        ..npm_install_set()
    };
    for tag in ["v1.2.3", "1.2.3", "rust-v0.46.0"] {
        match select_install_path(
            "test",
            "harness.install",
            &install,
            Some("https://github.com/example/agent"),
            Some(tag),
        )
        .expect("resolve")
        {
            ResolvedInstallSpec::GithubRelease { version_pin, .. } => {
                assert_eq!(version_pin.as_deref(), Some(tag));
            }
            other => panic!("expected GithubRelease variant, got {other:?}"),
        }
    }
}

#[test]
fn a_pin_on_a_script_only_cli_is_a_typed_error() {
    let error = select_install_path(
        "test",
        "harness.install",
        &shell_install_set("true", "agent"),
        None,
        Some("1.2.3"),
    )
    .expect_err("a vendor script cannot fetch a chosen version");
    assert!(
        matches!(
            &error,
            StackError::AgentVersionUnsupported { agent_id, version, .. }
                if agent_id == "test" && version == "1.2.3"
        ),
        "{error:?}"
    );
    assert_eq!(error.error_code(), "agent.version_unsupported");
}

fn write_script(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
    std::fs::write(path, format!("#!/bin/sh\n{body}\n")).expect("script");
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o755))
        .expect("chmod");
}

#[test]
fn keeping_a_native_cli_skips_its_install_and_records_a_kept_row() {
    let tempdir = TempDir::new().expect("tempdir");
    let existing = tempdir.path().join("vendor/bin/kept-agent");
    write_script(&existing, "echo kept-agent 9.9.9");
    // The recipe would fail the run if it ever ran.
    let entry = native_entry(
        "kept-agent",
        "Kept Agent",
        Some("docs/agents/kept-agent.md"),
        harness_spec("kept-agent", shell_install_set("exit 42", "kept-agent")),
    );

    let result = install_resolved_capture(
        &agent_config("kept-agent"),
        &entry,
        &HarnessInstall::Keep(existing.clone()),
        HashMap::new(),
        tempdir.path(),
        tempdir.path(),
        None,
        tempdir.path(),
    );

    let outcome = result.outcome.expect("keeping a runnable binary succeeds");
    assert!(matches!(&outcome, InstallerOutcome::AlreadyPresent { path, .. } if *path == existing));
    assert_eq!(result.rows.len(), 1);
    let row = &result.rows[0];
    assert_eq!(row.status, INSTALLER_STATUS_KEPT);
    assert_eq!(row.step, STEP_INSTALL);
    assert_eq!(row.version.as_deref(), Some("9.9.9"));
    let artifact = row.artifact.as_ref().expect("kept row records its binary");
    assert_eq!(artifact.path, existing);
    assert_eq!(artifact.sha256, outcome.sha256());
}

#[test]
fn keeping_a_cli_that_cannot_spawn_fails_the_install() {
    let tempdir = TempDir::new().expect("tempdir");
    let existing = tempdir.path().join("vendor/bin/broken-agent");
    std::fs::create_dir_all(existing.parent().expect("parent")).expect("dir");
    std::fs::write(&existing, b"not an executable").expect("stub");
    let entry = native_entry(
        "broken-agent",
        "Broken Agent",
        Some("docs/agents/broken-agent.md"),
        harness_spec("broken-agent", shell_install_set("exit 42", "broken-agent")),
    );

    let result = install_resolved_capture(
        &agent_config("broken-agent"),
        &entry,
        &HarnessInstall::Keep(existing),
        HashMap::new(),
        tempdir.path(),
        tempdir.path(),
        None,
        tempdir.path(),
    );

    assert!(matches!(
        result.outcome,
        Err(StackError::AgentInstallerBinaryUnrunnable { .. })
    ));
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].status, "failed");
}

#[test]
fn keeping_an_adapter_kind_cli_still_installs_the_adapter() {
    let tempdir = TempDir::new().expect("tempdir");
    let existing = tempdir.path().join("vendor/bin/duo-agent");
    write_script(&existing, "echo duo-agent 1.0.0");
    let adapter_path = tempdir.path().join("duo-acp");
    let entry = adapter_entry(
        "duo",
        "Duo",
        Some("docs/agents/duo.md"),
        harness_spec("duo-agent", shell_install_set("exit 42", "duo-agent")),
        adapter_spec(
            "duo-acp",
            shell_install_set(
                &shell_string_for_write(&adapter_path, "adapter"),
                &adapter_path.display().to_string(),
            ),
        ),
    );

    let result = install_resolved_capture(
        &agent_config(&adapter_path.display().to_string()),
        &entry,
        &HarnessInstall::Keep(existing.clone()),
        HashMap::new(),
        tempdir.path(),
        tempdir.path(),
        None,
        tempdir.path(),
    );

    result
        .outcome
        .expect("the adapter installs beside a kept CLI");
    let harness_row = result
        .rows
        .iter()
        .find(|row| row.step == STEP_HARNESS)
        .expect("harness row");
    assert_eq!(harness_row.status, INSTALLER_STATUS_KEPT);
    assert_eq!(
        harness_row.artifact.as_ref().map(|artifact| &artifact.path),
        Some(&existing)
    );
    let adapter_row = result
        .rows
        .iter()
        .find(|row| row.step == STEP_ADAPTER)
        .expect("adapter row");
    assert_eq!(adapter_row.status, "ran");
    assert_eq!(
        adapter_row.artifact.as_ref().map(|artifact| &artifact.path),
        Some(&adapter_path)
    );
}

#[test]
fn an_installed_step_records_the_binary_it_left() {
    let tempdir = TempDir::new().expect("tempdir");
    let binary = tempdir.path().join("fresh-agent");
    let entry = native_entry(
        "fresh-agent",
        "Fresh Agent",
        Some("docs/agents/fresh-agent.md"),
        harness_spec(
            "fresh-agent",
            shell_install_set(
                &shell_string_for_write(&binary, "fresh"),
                &binary.display().to_string(),
            ),
        ),
    );

    let result = install_resolved_capture(
        &agent_config(&binary.display().to_string()),
        &entry,
        &HarnessInstall::Install,
        HashMap::new(),
        tempdir.path(),
        tempdir.path(),
        None,
        tempdir.path(),
    );

    let outcome = result.outcome.expect("install succeeds");
    let artifact = result.rows[0]
        .artifact
        .as_ref()
        .expect("a ran row records its binary");
    assert_eq!(artifact.path, binary);
    assert_eq!(artifact.sha256, outcome.sha256());
}
