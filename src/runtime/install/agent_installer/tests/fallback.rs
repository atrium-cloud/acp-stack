use super::super::execute::{exhausted_after_missing_prerequisites, missing_required_tools};
use super::super::*;
use super::support::*;
use crate::runtime::install::agent_registry::{ArchiveKind, ShellInstall};
use tempfile::TempDir;

#[test]
fn install_records_every_fallback_attempt_when_first_path_fails() {
    // Both attempts must land as `installer_runs` rows, proving the walk continued past the
    // failed shell path rather than bailing on the first failure.
    let tempdir = TempDir::new().expect("tempdir");
    write_fake_npm(
        tempdir.path(),
        r#"
set -eu
if [ "$1" = "view" ]; then
  printf '"1.2.3"\n'
  exit 0
fi
exit 9
"#,
    );

    let install = InstallSet {
        shell: Some(ShellInstall {
            script: "exit 1".to_owned(),
            creates: "fallback-agent".to_owned(),
            required_tools: Vec::new(),
            timeout_secs: None,
        }),
        npm: Some(crate::runtime::install::agent_registry::NpmInstall {
            package: "@acp-stack/definitely-not-published".to_owned(),
            creates: "fallback-agent".to_owned(),
        }),
        ..InstallSet::default()
    };
    let entry = native_entry(
        "fallback-agent",
        "Fallback Agent",
        Some("docs/agents/fallback-agent.md"),
        harness_spec("fallback-agent", install),
    );

    let result = install_resolved_capture(
        &agent_config("fallback-agent"),
        &entry,
        HashMap::new(),
        tempdir.path(),
        tempdir.path(),
        None,
        tempdir.path(),
    );

    match result
        .outcome
        .expect_err("every declared path is unreachable")
    {
        StackError::AgentInstallAllPathsFailed { summary } => {
            assert!(
                summary.contains("shell:") && summary.contains("npm:"),
                "terminal error must enumerate both attempted paths, got `{summary}`",
            );
        }
        other => panic!("expected the enumerated all-paths error, got {other:?}"),
    }
    assert!(
        result.rows.len() >= 2,
        "expected fallback chain to record both attempts, got {:?}",
        result
            .rows
            .iter()
            .map(|r| (r.status.as_str(), r.exit_status))
            .collect::<Vec<_>>(),
    );
    for (i, row) in result.rows.iter().enumerate() {
        assert_eq!(
            row.status, "failed",
            "attempt #{i} should be `failed`, got `{}`",
            row.status,
        );
    }
}

#[test]
fn github_backed_shell_failure_falls_back_to_npm_and_records_both_attempts() {
    // opencode's upstream installer resolves its release through the GitHub API, so an
    // unauthenticated host fails it once the hourly quota is gone and npm must take over.
    let tempdir = TempDir::new().expect("tempdir");
    let dest_dir = tempdir.path().join("bin");
    std::fs::create_dir(&dest_dir).expect("create bin dir");
    let installed = dest_dir.join("opencode");
    write_fake_npm(
        &dest_dir,
        r#"
set -eu
if [ "$1" = "view" ]; then
  test "$2" = "opencode-ai"
  test "$3" = "version"
  test "$4" = "--json"
  printf '"1.2.3"\n'
  exit 0
fi
if [ "$1" = "install" ]; then
  test "$2" = "-g"
  test "$3" = "--prefix"
  test "$5" = "--allow-scripts=opencode-ai"
  test "$6" = "opencode-ai@1.2.3"
  mkdir -p "$4/bin"
  printf '#!/bin/sh\n' > "$4/bin/opencode"
  chmod 755 "$4/bin/opencode"
  exit 0
fi
exit 1
"#,
    );

    let install = InstallSet {
        shell: Some(ShellInstall {
            script: "exit 1".to_owned(),
            creates: "opencode".to_owned(),
            required_tools: Vec::new(),
            timeout_secs: None,
        }),
        npm: Some(crate::runtime::install::agent_registry::NpmInstall {
            package: "opencode-ai".to_owned(),
            creates: "opencode".to_owned(),
        }),
        ..InstallSet::default()
    };

    let chain = install_one_with_fallback(
        "opencode",
        "harness.install",
        STEP_INSTALL,
        &install,
        None,
        None,
        &HashMap::new(),
        tempdir.path(),
        &dest_dir,
        false,
        None,
        tempdir.path(),
    );

    assert!(
        chain.terminal_error.is_none(),
        "npm fallback should carry the install, got {:?}",
        chain.terminal_error,
    );
    assert_eq!(
        chain
            .rows
            .iter()
            .map(|row| (row.method.as_deref(), row.status.as_str()))
            .collect::<Vec<_>>(),
        vec![
            (Some(INSTALL_METHOD_SHELL), "failed"),
            (Some(INSTALL_METHOD_NPM), "ran"),
        ],
    );
    assert_eq!(chain.rows[1].version.as_deref(), Some("1.2.3"));
    assert!(installed.is_file(), "npm fallback must produce the binary");
}

#[test]
fn shell_install_records_no_version() {
    let tempdir = TempDir::new().expect("tempdir");
    let binary_path = tempdir.path().join("shell-agent");
    let script = shell_string_for_write(&binary_path, "agent");
    let entry = native_entry(
        "shell-agent",
        "Shell Agent",
        Some("docs/agents/shell-agent.md"),
        harness_spec("shell-agent", shell_install_set(&script, "shell-agent")),
    );
    let result = install_resolved_capture(
        &agent_config("shell-agent"),
        &entry,
        HashMap::new(),
        tempdir.path(),
        tempdir.path(),
        None,
        tempdir.path(),
    );
    result.outcome.expect("install ok");
    assert_eq!(result.rows.len(), 1);
    assert!(
        result.rows[0].version.is_none(),
        "shell installs must leave version unset; got {:?}",
        result.rows[0].version
    );
}

#[test]
fn missing_shell_required_tool_fails_when_no_fallback_is_runnable() {
    let tempdir = TempDir::new().expect("tempdir");
    let install = InstallSet {
        shell: Some(ShellInstall {
            script: "missing-tool-command".to_owned(),
            creates: "agent".to_owned(),
            required_tools: vec!["definitely-missing-acp-stack-tool".to_owned()],
            timeout_secs: None,
        }),
        ..InstallSet::default()
    };

    let chain = install_one_with_fallback(
        "preflight-agent",
        "harness.install",
        STEP_INSTALL,
        &install,
        None,
        None,
        &HashMap::new(),
        tempdir.path(),
        tempdir.path(),
        false,
        None,
        tempdir.path(),
    );

    match chain.terminal_error.expect("missing prerequisite") {
        StackError::AgentInstallerPrerequisitesMissing {
            agent_id,
            step,
            tools,
        } => {
            assert_eq!(agent_id, "preflight-agent");
            assert_eq!(step, "harness.install");
            assert_eq!(tools, vec!["definitely-missing-acp-stack-tool"]);
        }
        other => panic!("expected prerequisite error, got {other:?}"),
    }
}

#[test]
fn missing_shell_required_tool_falls_back_to_runnable_npm_path() {
    let tempdir = TempDir::new().expect("tempdir");
    write_fake_npm(
        tempdir.path(),
        r#"
set -eu
if [ "$1" = "view" ]; then
  printf '"1.2.3"\n'
  exit 0
fi
exit 9
"#,
    );
    let install = InstallSet {
        shell: Some(ShellInstall {
            script: "missing-tool-command".to_owned(),
            creates: "agent".to_owned(),
            required_tools: vec!["definitely-missing-acp-stack-tool".to_owned()],
            timeout_secs: None,
        }),
        npm: Some(crate::runtime::install::agent_registry::NpmInstall {
            package: "@scope/agent".to_owned(),
            creates: "agent".to_owned(),
        }),
        ..InstallSet::default()
    };

    let chain = install_one_with_fallback(
        "preflight-agent",
        "harness.install",
        STEP_INSTALL,
        &install,
        None,
        None,
        &HashMap::new(),
        tempdir.path(),
        tempdir.path(),
        false,
        None,
        tempdir.path(),
    );

    match chain.terminal_error.expect("chain should fail") {
        StackError::AgentInstallAllPathsFailed { summary } => {
            assert!(
                summary
                    .contains("shell: skipped, missing tools: definitely-missing-acp-stack-tool"),
                "summary must record the skipped shell path, got `{summary}`",
            );
            assert!(
                summary.contains("npm: agent installer exited with status 9"),
                "summary must record the npm failure, got `{summary}`",
            );
        }
        other => panic!("expected the enumerated all-paths error, got {other:?}"),
    }
    assert_eq!(chain.rows.len(), 1);
}

#[test]
fn missing_fallback_prerequisite_does_not_mask_runnable_path_failure() {
    let attempts = vec![
        ("shell", "agent installer exited with status 7".to_owned()),
        ("npm", "skipped, missing tools: npm".to_owned()),
    ];
    let chain = exhausted_after_missing_prerequisites(
        "preflight-agent",
        "harness.install",
        STEP_INSTALL,
        vec![InstallerRowDraft::config_error(STEP_INSTALL)],
        &attempts,
        Some(StackError::AgentInstallerFailed {
            exit: Some(7),
            stderr_tail: "failed".to_owned(),
        }),
        BTreeSet::from(["npm".to_owned()]),
    );
    match chain.terminal_error.expect("chain should fail") {
        StackError::AgentInstallAllPathsFailed { summary } => {
            assert!(
                summary.contains("shell: agent installer exited with status 7"),
                "summary must keep the shell failure visible, got `{summary}`",
            );
            assert!(
                summary.contains("npm: skipped, missing tools: npm"),
                "summary must record the skipped npm path, got `{summary}`",
            );
        }
        other => panic!("expected the enumerated all-paths error, got {other:?}"),
    }
    assert_eq!(chain.rows.len(), 1);
}

#[test]
fn single_path_failure_keeps_its_typed_error() {
    let tempdir = TempDir::new().expect("tempdir");
    let install = InstallSet {
        shell: Some(ShellInstall {
            script: "exit 3".to_owned(),
            creates: "agent".to_owned(),
            required_tools: Vec::new(),
            timeout_secs: None,
        }),
        ..InstallSet::default()
    };

    let chain = install_one_with_fallback(
        "single-path-agent",
        "harness.install",
        STEP_INSTALL,
        &install,
        None,
        None,
        &HashMap::new(),
        tempdir.path(),
        tempdir.path(),
        false,
        None,
        tempdir.path(),
    );

    assert!(
        matches!(
            chain.terminal_error,
            Some(StackError::AgentInstallerFailed { exit: Some(3), .. })
        ),
        "a lone failed path must surface unwrapped, got {:?}",
        chain.terminal_error
    );
}

#[test]
fn github_release_install_path_has_no_host_tool_prerequisites() {
    let spec = ResolvedInstallSpec::GithubRelease {
        repo: "owner/repo".to_owned(),
        asset_pattern: "agent-linux-x86_64.tar.gz".to_owned(),
        archive: ArchiveKind::TarGz,
        archive_binary_name: None,
        binary_name: "agent".to_owned(),
        checksums_asset: None,
        version_pin: None,
    };
    let tempdir = TempDir::new().expect("tempdir");
    assert!(missing_required_tools(&spec, tempdir.path(), tempdir.path()).is_empty());
}
