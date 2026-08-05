use super::execute::{exhausted_after_missing_prerequisites, missing_required_tools};
use super::step_runners::select_install_path;
use super::*;
use crate::runtime::install::agent_registry::{
    AdapterSpec, ArchiveKind, HarnessSpec, InstallProvidedBy, ShellInstall,
};
use crate::state::StateStore;
use std::os::unix::fs::PermissionsExt;
use tempfile::TempDir;

fn open_store() -> (TempDir, StateStore) {
    let tempdir = TempDir::new().expect("tempdir");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("open");
    store.migrate().expect("migrate");
    (tempdir, store)
}

fn install_config(shell: &str, creates: &str) -> AgentInstallConfig {
    AgentInstallConfig {
        install_type: "shell".into(),
        creates: creates.into(),
        shell: Some(shell.into()),
    }
}

fn workspace_root() -> PathBuf {
    std::env::temp_dir()
}

#[test]
fn init_resume_creates_resolver_checks_local_bin_and_workspace_relative_paths() {
    let tempdir = TempDir::new().expect("tempdir");
    let workspace_root = tempdir.path().join("workspace");
    let local_bin = tempdir.path().join(".local/bin");
    std::fs::create_dir_all(workspace_root.join("bin")).expect("workspace bin");
    std::fs::create_dir_all(&local_bin).expect("local bin");
    let workspace_agent = workspace_root.join("bin/agent");
    let local_agent = local_bin.join("managed-agent");
    std::fs::write(&workspace_agent, b"#!/bin/sh\n").expect("workspace agent");
    std::fs::write(&local_agent, b"#!/bin/sh\n").expect("local agent");
    let executable = std::fs::Permissions::from_mode(0o755);
    std::fs::set_permissions(&workspace_agent, executable.clone()).expect("chmod workspace agent");
    std::fs::set_permissions(&local_agent, executable).expect("chmod local agent");

    assert_eq!(
        resolve_creates_for_init_resume("bin/agent", &workspace_root, &[&local_bin], None),
        Some(workspace_agent),
    );
    assert_eq!(
        resolve_creates_for_init_resume("managed-agent", &workspace_root, &[&local_bin], None),
        Some(local_agent),
    );
    assert_eq!(
        resolve_creates_for_init_resume("managed-agent", &workspace_root, &[], None),
        None,
        "custom [agent.install] verifier must not search managed local bin unless it is on PATH",
    );
}

fn agent_config(command: &str) -> AgentConfig {
    AgentConfig {
        id: "test-agent".to_owned(),
        name: "Test Agent".to_owned(),
        command: command.to_owned(),
        args: Vec::new(),
        cwd: None,
        env: Vec::new(),
        expected_sha256: None,
        restart: "on-crash".to_owned(),
        mode: None,
        model: None,
        harness_version: None,
        adapter: None,
        provider: None,
        providers: None,
        subagent: None,
        auto_update: None,
        install: None,
    }
}

fn shell_install_set(script: &str, creates: &str) -> InstallSet {
    InstallSet {
        shell: Some(ShellInstall {
            script: script.to_owned(),
            creates: creates.to_owned(),
            required_tools: Vec::new(),
        }),
        ..InstallSet::default()
    }
}

fn adapter_provided_install_set() -> InstallSet {
    InstallSet {
        provided_by: Some(InstallProvidedBy::Adapter),
        ..InstallSet::default()
    }
}

fn harness_spec(id: &str, install: InstallSet) -> HarnessSpec {
    HarnessSpec {
        id: id.to_owned(),
        install,
        update: Default::default(),
    }
}

fn adapter_spec(id: &str, install: InstallSet) -> AdapterSpec {
    AdapterSpec {
        id: id.to_owned(),
        sync_id: None,
        github: None,
        install,
        update: Default::default(),
    }
}

fn native_entry(
    id: &str,
    name: &str,
    support_doc: Option<&str>,
    harness: HarnessSpec,
) -> RegistryEntry {
    RegistryEntry {
        id: id.to_owned(),
        name: name.to_owned(),
        kind: RegistryKind::Native,
        headless_compatible: support_doc.is_some(),
        set_provider: false,
        multiple_active_providers: false,
        set_model: false,
        set_mode: false,
        supports_agent_skills: false,
        agent_skills_install_dir: None,
        agent_skills_link_dir: None,
        subagents: false,
        subagent_alias: None,
        subagent_free_models: Vec::new(),
        sync_exempt: false,
        allow_custom_provider: false,
        allow_custom_model: false,
        stdio_framing: Default::default(),
        website: None,
        github: None,
        support_doc: support_doc.map(str::to_owned),
        testflight_prompt: None,
        testflight_expect_fs: None,
        adapter: None,
        harness: Some(harness),
    }
}

fn adapter_entry(
    id: &str,
    name: &str,
    support_doc: Option<&str>,
    harness: HarnessSpec,
    adapter: AdapterSpec,
) -> RegistryEntry {
    RegistryEntry {
        id: id.to_owned(),
        name: name.to_owned(),
        kind: RegistryKind::Adapter,
        headless_compatible: support_doc.is_some(),
        set_provider: false,
        multiple_active_providers: false,
        set_model: false,
        set_mode: false,
        supports_agent_skills: false,
        agent_skills_install_dir: None,
        agent_skills_link_dir: None,
        subagents: false,
        subagent_alias: None,
        subagent_free_models: Vec::new(),
        sync_exempt: false,
        allow_custom_provider: false,
        allow_custom_model: false,
        stdio_framing: Default::default(),
        website: None,
        github: None,
        support_doc: support_doc.map(str::to_owned),
        testflight_prompt: None,
        testflight_expect_fs: None,
        adapter: Some(adapter),
        harness: Some(harness),
    }
}

// Fixture binaries carry a shebang so they pass the installer's spawn gate;
// `content` lands after it as a `#`-prefixed comment to keep files distinct
// without the probe executing it.
fn shell_string_for_write(path: &Path, content: &str) -> String {
    format!(
        "mkdir -p {bin} && printf '#!/bin/sh\\n# %s' {content} > {binary} && chmod 755 {binary}",
        bin = shell_quote_path(path.parent().expect("binary has parent")),
        content = shell_quote_literal(content),
        binary = shell_quote_path(path),
    )
}

fn shell_quote_literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn write_fake_npm(dest_dir: &Path, body: &str) {
    let npm_path = dest_dir.join("npm");
    std::fs::write(&npm_path, format!("#!/bin/sh\n{body}")).expect("write fake npm");
    let permissions = std::fs::Permissions::from_mode(0o755);
    std::fs::set_permissions(&npm_path, permissions).expect("chmod fake npm");
}

#[test]
fn select_install_path_captures_pinned_npm_version() {
    let install = InstallSet {
        npm: Some(crate::runtime::install::agent_registry::NpmInstall {
            package: "@scope/agent".to_owned(),
            creates: "agent".to_owned(),
        }),
        ..InstallSet::default()
    };
    let resolved = select_install_path("test", "harness.install", &install, None, Some("1.2.3"))
        .expect("resolve");
    match resolved {
        ResolvedInstallSpec::Npm {
            package,
            name,
            version,
            creates,
        } => {
            assert_eq!(package, "@scope/agent@1.2.3");
            assert_eq!(name, "@scope/agent");
            assert_eq!(version.as_deref(), Some("1.2.3"));
            assert_eq!(creates, "agent");
        }
        other => panic!("expected Npm variant, got {other:?}"),
    }
}

#[test]
fn select_install_path_unpinned_npm_has_no_version() {
    let install = InstallSet {
        npm: Some(crate::runtime::install::agent_registry::NpmInstall {
            package: "@scope/agent".to_owned(),
            creates: "agent".to_owned(),
        }),
        ..InstallSet::default()
    };
    let resolved =
        select_install_path("test", "harness.install", &install, None, None).expect("resolve");
    match resolved {
        ResolvedInstallSpec::Npm {
            package,
            name,
            version,
            creates,
        } => {
            assert_eq!(package, "@scope/agent");
            assert_eq!(name, "@scope/agent");
            assert!(version.is_none());
            assert_eq!(creates, "agent");
        }
        other => panic!("expected Npm variant, got {other:?}"),
    }
}

#[test]
fn unpinned_npm_install_records_resolved_version() {
    let tempdir = TempDir::new().expect("tempdir");
    let dest_dir = tempdir.path().join("bin");
    std::fs::create_dir(&dest_dir).expect("create bin dir");
    write_fake_npm(
        &dest_dir,
        r#"
set -eu
if [ "$1" = "view" ]; then
  test "$2" = "@scope/agent"
  test "$3" = "version"
  test "$4" = "--json"
  printf '"1.2.3"\n'
  exit 0
fi
if [ "$1" = "install" ]; then
  test "$2" = "-g"
  test "$3" = "--prefix"
  test "$5" = "--allow-scripts=@scope/agent"
  test "$6" = "@scope/agent@1.2.3"
  mkdir -p "$4/bin"
  printf '#!/bin/sh\n' > "$4/bin/agent"
  chmod 755 "$4/bin/agent"
  exit 0
fi
exit 99
"#,
    );
    let install = InstallSet {
        npm: Some(crate::runtime::install::agent_registry::NpmInstall {
            package: "@scope/agent".to_owned(),
            creates: "agent".to_owned(),
        }),
        ..InstallSet::default()
    };
    let entry = native_entry(
        "npm-agent",
        "Npm Agent",
        Some("docs/agents/npm-agent.md"),
        harness_spec("agent", install),
    );

    let result = install_resolved_capture(
        &agent_config("agent"),
        &entry,
        HashMap::new(),
        tempdir.path(),
        &dest_dir,
    );

    result.outcome.expect("npm install should pass");
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].status, "ran");
    assert_eq!(result.rows[0].version.as_deref(), Some("1.2.3"));
}

#[test]
fn unpinned_npm_install_accepts_array_version_output() {
    // Fresh hosts have been observed getting a JSON array from
    // `npm view <pkg> version --json` (e.g. `["1.18.7"]`); npm orders it
    // ascending, so the last element is the version to install.
    let tempdir = TempDir::new().expect("tempdir");
    let dest_dir = tempdir.path().join("bin");
    std::fs::create_dir(&dest_dir).expect("create bin dir");
    write_fake_npm(
        &dest_dir,
        r#"
set -eu
if [ "$1" = "view" ]; then
  printf '[\n  "1.0.0",\n  "1.18.7"\n]\n'
  exit 0
fi
if [ "$1" = "install" ]; then
  test "$5" = "--allow-scripts=@scope/agent"
  test "$6" = "@scope/agent@1.18.7"
  mkdir -p "$4/bin"
  printf '#!/bin/sh\n' > "$4/bin/agent"
  chmod 755 "$4/bin/agent"
  exit 0
fi
exit 99
"#,
    );
    let install = InstallSet {
        npm: Some(crate::runtime::install::agent_registry::NpmInstall {
            package: "@scope/agent".to_owned(),
            creates: "agent".to_owned(),
        }),
        ..InstallSet::default()
    };
    let entry = native_entry(
        "npm-agent",
        "Npm Agent",
        Some("docs/agents/npm-agent.md"),
        harness_spec("agent", install),
    );

    let result = install_resolved_capture(
        &agent_config("agent"),
        &entry,
        HashMap::new(),
        tempdir.path(),
        &dest_dir,
    );

    result.outcome.expect("array version output should pass");
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].status, "ran");
    assert_eq!(result.rows[0].version.as_deref(), Some("1.18.7"));
}

#[test]
fn npm_version_lookup_empty_array_fails_step() {
    let tempdir = TempDir::new().expect("tempdir");
    let dest_dir = tempdir.path().join("bin");
    std::fs::create_dir(&dest_dir).expect("create bin dir");
    write_fake_npm(
        &dest_dir,
        r#"
set -eu
if [ "$1" = "view" ]; then
  printf '[]\n'
  exit 0
fi
exit 99
"#,
    );
    let install = InstallSet {
        npm: Some(crate::runtime::install::agent_registry::NpmInstall {
            package: "@scope/agent".to_owned(),
            creates: "agent".to_owned(),
        }),
        ..InstallSet::default()
    };
    let entry = native_entry(
        "npm-agent",
        "Npm Agent",
        Some("docs/agents/npm-agent.md"),
        harness_spec("agent", install),
    );

    let result = install_resolved_capture(
        &agent_config("agent"),
        &entry,
        HashMap::new(),
        tempdir.path(),
        &dest_dir,
    );

    assert!(matches!(
        result.outcome.expect_err("empty array should fail"),
        StackError::AgentInitializeFailed { .. }
    ));
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].status, "failed");
    assert!(result.rows[0].stderr.contains("empty version"));
    assert!(result.rows[0].version.is_none());
}

#[test]
fn npm_version_lookup_failure_fails_step() {
    let tempdir = TempDir::new().expect("tempdir");
    let dest_dir = tempdir.path().join("bin");
    std::fs::create_dir(&dest_dir).expect("create bin dir");
    write_fake_npm(
        &dest_dir,
        r#"
set -eu
if [ "$1" = "view" ]; then
  printf 'registry down\n' >&2
  exit 7
fi
exit 99
"#,
    );
    let install = InstallSet {
        npm: Some(crate::runtime::install::agent_registry::NpmInstall {
            package: "@scope/agent".to_owned(),
            creates: "agent".to_owned(),
        }),
        ..InstallSet::default()
    };
    let entry = native_entry(
        "npm-agent",
        "Npm Agent",
        Some("docs/agents/npm-agent.md"),
        harness_spec("agent", install),
    );

    let result = install_resolved_capture(
        &agent_config("agent"),
        &entry,
        HashMap::new(),
        tempdir.path(),
        &dest_dir,
    );

    assert!(matches!(
        result.outcome.expect_err("npm view failure should fail"),
        StackError::AgentInstallerFailed { exit: Some(7), .. }
    ));
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].status, "failed");
    assert_eq!(result.rows[0].exit_status, Some(7));
    assert!(result.rows[0].version.is_none());
}

#[test]
fn npm_version_lookup_invalid_json_fails_step() {
    let tempdir = TempDir::new().expect("tempdir");
    let dest_dir = tempdir.path().join("bin");
    std::fs::create_dir(&dest_dir).expect("create bin dir");
    write_fake_npm(
        &dest_dir,
        r#"
set -eu
if [ "$1" = "view" ]; then
  printf 'not-json\n'
  exit 0
fi
exit 99
"#,
    );
    let install = InstallSet {
        npm: Some(crate::runtime::install::agent_registry::NpmInstall {
            package: "@scope/agent".to_owned(),
            creates: "agent".to_owned(),
        }),
        ..InstallSet::default()
    };
    let entry = native_entry(
        "npm-agent",
        "Npm Agent",
        Some("docs/agents/npm-agent.md"),
        harness_spec("agent", install),
    );

    let result = install_resolved_capture(
        &agent_config("agent"),
        &entry,
        HashMap::new(),
        tempdir.path(),
        &dest_dir,
    );

    assert!(matches!(
        result
            .outcome
            .expect_err("invalid npm view JSON should fail"),
        StackError::AgentInitializeFailed { .. }
    ));
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].status, "failed");
    assert!(result.rows[0].stderr.contains("unexpected JSON"));
    assert!(result.rows[0].version.is_none());
}

#[test]
fn install_records_every_fallback_attempt_when_first_path_fails() {
    // The first declared path is a shell recipe that exits 1
    // without producing `creates`. The second is an npm package that
    // npm/npx can't actually fetch in the test sandbox. The
    // important guarantee being asserted: BOTH attempts get recorded
    // as `installer_runs` rows (not just the first one). This proves
    // `install_one_with_fallback` walked past the failed shell path
    // and tried npm, rather than the pre-audit behavior of bailing
    // on the first failure.
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
    );

    // The chain exhausted both paths, so the overall outcome is Err.
    // But the rows must include BOTH attempts — proof that the
    // fallback walk actually happened.
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
    // Both rows must record the failure outcome — proves the runner
    // didn't skip the second attempt after the first failed.
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
    // opencode's upstream installer resolves its release through the GitHub
    // API, so unauthenticated hosts start failing it once the hourly quota is
    // gone. The npm path must then take over, and both attempts must land in
    // `installer_runs` under distinct methods so the fallback is visible.
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

#[test]
fn persist_step_logs_writes_files_and_sets_log_dir() {
    let tempdir = TempDir::new().expect("tempdir");
    let mut row = InstallerRowDraft {
        started_at: "2026-05-22T00:00:00.123456789Z".to_owned(),
        finished_at: Some("2026-05-22T00:00:01.000000000Z".to_owned()),
        status: "ran".into(),
        stdout: "hello stdout\n".into(),
        stderr: "hello stderr\n".into(),
        exit_status: Some(0),
        step: "harness".into(),
        method: Some(INSTALL_METHOD_GITHUB.to_owned()),
        version: Some("v1.0.0".into()),
        log_dir: None,
    };
    persist_step_logs_to_disk(&mut row, "test-agent", Some(tempdir.path()))
        .expect("logs should persist");
    let log_dir = row.log_dir.as_deref().expect("log_dir set on success");
    let stdout_path = std::path::Path::new(log_dir).join("stdout");
    let stderr_path = std::path::Path::new(log_dir).join("stderr");
    let stdout_body = std::fs::read_to_string(&stdout_path).expect("stdout written");
    let stderr_body = std::fs::read_to_string(&stderr_path).expect("stderr written");
    assert_eq!(stdout_body, "hello stdout\n");
    assert_eq!(stderr_body, "hello stderr\n");
}

#[test]
fn persist_step_logs_skips_when_streams_empty() {
    let tempdir = TempDir::new().expect("tempdir");
    let mut row = InstallerRowDraft {
        started_at: "2026-05-22T00:00:00.000000000Z".to_owned(),
        finished_at: Some("2026-05-22T00:00:00.000000000Z".to_owned()),
        status: "skipped".into(),
        stdout: String::new(),
        stderr: String::new(),
        exit_status: Some(0),
        step: "install".into(),
        method: Some(INSTALL_METHOD_SHELL.to_owned()),
        version: None,
        log_dir: None,
    };
    persist_step_logs_to_disk(&mut row, "test-agent", Some(tempdir.path()))
        .expect("empty streams should be a no-op");
    assert!(
        row.log_dir.is_none(),
        "log_dir must stay None when both streams are empty"
    );
}

#[test]
fn persist_step_logs_is_a_no_op_when_log_base_is_none() {
    let mut row = InstallerRowDraft {
        started_at: "2026-05-22T00:00:00.000000000Z".to_owned(),
        finished_at: None,
        status: "ran".into(),
        stdout: "anything".into(),
        stderr: String::new(),
        exit_status: Some(0),
        step: "harness".into(),
        method: Some(INSTALL_METHOD_SHELL.to_owned()),
        version: None,
        log_dir: None,
    };
    persist_step_logs_to_disk(&mut row, "test-agent", None)
        .expect("missing log base should be a no-op");
    assert!(row.log_dir.is_none());
}

#[test]
fn installer_log_persist_failure_prevents_history_row() {
    let tempdir = TempDir::new().expect("tempdir");
    let (_state_dir, store) = open_store();
    let log_base_file = tempdir.path().join("not-a-directory");
    std::fs::write(&log_base_file, b"file blocks log dir").expect("write blocker file");
    let install = install_config(
        "printf 'audit stdout\n'; mkdir -p bin; printf agent > bin/test-agent",
        "bin/test-agent",
    );

    let err = run_installer(
        "test-agent",
        &install,
        None,
        HashMap::new(),
        tempdir.path(),
        &store,
        Some(&log_base_file),
    )
    .expect_err("log persistence failure must fail install wrapper");

    assert!(matches!(err, StackError::AgentInstallerLogPersist { .. }));
    let runs = store.query_installer_runs(10).expect("query");
    assert!(
        runs.is_empty(),
        "installer history must not record a row without the audit log"
    );
}

#[test]
fn installer_env_is_non_interactive_and_reserved_names_resist_agent_env() {
    let (tempdir, store) = open_store();
    let capture = tempdir.path().join("env-capture");
    // The script records the env the installer actually ran with; `creates`
    // is left unresolvable so the outcome itself is irrelevant to the pin.
    let script = format!(
        "printf '%s:%s:%s' \"$CI\" \"$TERM\" \"$CUSTOM\" > {}",
        shell_quote_literal(&capture.display().to_string())
    );
    let install = install_config(&script, "definitely-not-a-real-binary-xyz123");
    let mut agent_env = HashMap::new();
    agent_env.insert("CI".to_owned(), "0".to_owned());
    agent_env.insert("TERM".to_owned(), "xterm-256color".to_owned());
    agent_env.insert("CUSTOM".to_owned(), "custom-value".to_owned());
    let _ = run_installer(
        "test-agent",
        &install,
        None,
        agent_env,
        &workspace_root(),
        &store,
        None,
    );
    let captured = std::fs::read_to_string(&capture).expect("script ran and captured env");
    assert_eq!(
        captured, "1:dumb:custom-value",
        "reserved non-interactive names must resist [agent].env; others pass through"
    );
}

#[test]
fn precheck_short_circuits_when_creates_resolves() {
    // `true` ships on every POSIX system; the installer should skip.
    let (_tempdir, store) = open_store();
    let install = install_config("false", "true");
    let outcome = run_installer(
        "test-agent",
        &install,
        None,
        HashMap::new(),
        &workspace_root(),
        &store,
        None,
    )
    .expect("ok");
    assert_eq!(outcome.label(), "already_present");
    let runs = store.query_installer_runs(10).expect("query");
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, "skipped");
    assert_eq!(runs[0].step, "install");
}

#[test]
fn missing_creates_after_run_returns_creates_missing() {
    let (_tempdir, store) = open_store();
    // A successful shell that does NOT actually produce the named binary.
    let install = install_config("true", "definitely-not-a-real-binary-xyz123");
    let err = run_installer(
        "test-agent",
        &install,
        None,
        HashMap::new(),
        &workspace_root(),
        &store,
        None,
    )
    .expect_err("must fail");
    assert!(matches!(
        err,
        StackError::AgentInstallerCreatesMissing { .. }
    ));
    let runs = store.query_installer_runs(10).expect("query");
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, "failed");
    assert_eq!(runs[0].step, "install");
}

#[test]
fn missing_workspace_root_returns_typed_installer_error() {
    let tempdir = TempDir::new().expect("tempdir");
    let missing_workspace = tempdir.path().join("missing-workspace");
    let install = install_config("true", "definitely-not-a-real-binary-xyz123");

    let result = run_installer_capture(&install, None, HashMap::new(), &missing_workspace);
    let err = result.outcome.expect_err("missing cwd must fail");

    assert!(matches!(
        err,
        StackError::AgentInstallerWorkingDirectoryMissing { path }
            if path == missing_workspace
    ));
    assert_eq!(result.row.status, "error");
    assert_eq!(result.row.step, "install");
}

#[test]
fn nonzero_exit_returns_installer_failed() {
    let (_tempdir, store) = open_store();
    let install = install_config("false", "definitely-not-a-real-binary-xyz123");
    let err = run_installer(
        "test-agent",
        &install,
        None,
        HashMap::new(),
        &workspace_root(),
        &store,
        None,
    )
    .expect_err("must fail");
    assert!(matches!(
        err,
        StackError::AgentInstallerFailed { exit: Some(1), .. }
    ));
    let runs = store.query_installer_runs(10).expect("query");
    assert_eq!(runs[0].status, "failed");
    assert_eq!(runs[0].exit_status, Some(1));
    assert_eq!(runs[0].step, "install");
}

#[test]
fn sha256_mismatch_returns_typed_error() {
    let (_tempdir, store) = open_store();
    let install = install_config("false", "true");
    let bogus = "0".repeat(64);
    let err = run_installer(
        "test-agent",
        &install,
        Some(&bogus),
        HashMap::new(),
        &workspace_root(),
        &store,
        None,
    )
    .expect_err("must fail");
    assert!(matches!(err, StackError::AgentSha256Mismatch { .. }));
}

#[test]
fn output_truncation_keeps_rows_bounded() {
    let (_tempdir, store) = open_store();
    // Emit ~200 KiB to stdout via printf inside the shell; the cap should
    // hold the resulting row well below twice the cap. `head -c` is
    // POSIX-portable enough for our test environments.
    let shell = format!(
        "head -c {} /dev/urandom | base64 | head -c {}",
        MAX_INSTALLER_STREAM_BYTES * 4,
        MAX_INSTALLER_STREAM_BYTES * 4
    );
    // Use a creates path that won't exist so we go through the "ran" path
    // and capture stdout. We don't care that this returns an error after
    // running; we only check the truncation guarantee on what was stored.
    let install = install_config(&shell, "definitely-not-a-real-binary-xyz123");
    let _ = run_installer(
        "test-agent",
        &install,
        None,
        HashMap::new(),
        &workspace_root(),
        &store,
        None,
    );
    let runs = store.query_installer_runs(10).expect("query");
    assert!(
        runs[0].stdout.len() <= MAX_INSTALLER_STREAM_BYTES + 128,
        "stdout grew to {} bytes",
        runs[0].stdout.len()
    );
}

#[test]
fn unsupported_registry_entry_fails_before_running_steps() {
    let entry = native_entry(
        "unsupported",
        "Unsupported Agent",
        None,
        harness_spec(
            "unsupported",
            shell_install_set("false", "definitely-should-not-run"),
        ),
    );
    let tempdir = TempDir::new().expect("tempdir");
    let result = install_resolved_capture(
        &agent_config("unsupported-agent"),
        &entry,
        HashMap::new(),
        tempdir.path(),
        tempdir.path(),
    );
    assert!(result.rows.is_empty());
    let err = result.outcome.expect_err("must reject unsupported agent");
    assert_eq!(
        err.public_message(),
        "Unsupported Agent is not currently supported. Please try a different agent."
    );
}

#[test]
fn final_verification_searches_managed_bin_dir() {
    let tempdir = TempDir::new().expect("tempdir");
    let dest_dir = tempdir.path().join("bin");
    std::fs::create_dir(&dest_dir).expect("create bin dir");
    let binary_path = dest_dir.join("managed-agent");
    std::fs::write(&binary_path, b"#!/bin/sh\n").expect("write fake binary");
    std::fs::set_permissions(&binary_path, std::fs::Permissions::from_mode(0o755))
        .expect("chmod fake binary");

    let entry = native_entry(
        "managed-agent",
        "Managed Agent",
        Some("docs/agents/managed-agent.md"),
        harness_spec("managed-agent", shell_install_set("true", "managed-agent")),
    );

    let result = install_resolved_capture(
        &agent_config("managed-agent"),
        &entry,
        HashMap::new(),
        tempdir.path(),
        &dest_dir,
    );
    let outcome = result.outcome.expect("managed binary should resolve");
    assert_eq!(outcome.path(), binary_path.as_path());
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].status, "ran");
}

#[test]
fn adapter_entry_installs_harness_then_adapter_and_verifies_adapter_command() {
    let tempdir = TempDir::new().expect("tempdir");
    let dest_dir = tempdir.path().join("bin");
    std::fs::create_dir(&dest_dir).expect("create bin dir");
    let adapter_binary = dest_dir.join("adapter-agent");
    let harness_binary = dest_dir.join("upstream-agent");
    let adapter_script = shell_string_for_write(&adapter_binary, "adapter");
    let harness_script = shell_string_for_write(&harness_binary, "harness");
    let entry = adapter_entry(
        "adapter-agent",
        "Adapter Agent",
        Some("docs/agents/adapter-agent.md"),
        harness_spec(
            "upstream-agent",
            shell_install_set(&harness_script, "upstream-agent"),
        ),
        adapter_spec(
            "adapter-agent",
            shell_install_set(&adapter_script, "adapter-agent"),
        ),
    );

    let result = install_resolved_capture(
        &agent_config("adapter-agent"),
        &entry,
        HashMap::new(),
        tempdir.path(),
        &dest_dir,
    );

    let outcome = result.outcome.expect("adapter should install");
    assert_eq!(outcome.path(), adapter_binary.as_path());
    assert_eq!(result.rows.len(), 2);
    assert_eq!(result.rows[0].step, "harness");
    assert_eq!(result.rows[0].status, "ran");
    assert_eq!(result.rows[1].step, "adapter");
    assert_eq!(result.rows[1].status, "ran");
}

#[test]
fn adapter_entry_runs_harness_and_adapter_install_steps_concurrently() {
    let tempdir = TempDir::new().expect("tempdir");
    let dest_dir = tempdir.path().join("bin");
    std::fs::create_dir(&dest_dir).expect("create bin dir");
    let harness_binary = dest_dir.join("upstream-agent");
    let adapter_binary = dest_dir.join("adapter-agent");
    let harness_script = format!(
        "sleep 0.6; mkdir -p {bin}; printf '#!/bin/sh\\n' > {harness}; chmod 755 {harness}",
        bin = shell_quote_path(&dest_dir),
        harness = shell_quote_path(&harness_binary),
    );
    let adapter_script = format!(
        "sleep 0.6; mkdir -p {bin}; printf '#!/bin/sh\\n' > {adapter}; chmod 755 {adapter}",
        bin = shell_quote_path(&dest_dir),
        adapter = shell_quote_path(&adapter_binary),
    );
    let entry = adapter_entry(
        "adapter-agent",
        "Adapter Agent",
        Some("docs/agents/adapter-agent.md"),
        harness_spec(
            "upstream-agent",
            shell_install_set(&harness_script, "upstream-agent"),
        ),
        adapter_spec(
            "adapter-agent",
            shell_install_set(&adapter_script, "adapter-agent"),
        ),
    );

    let started = std::time::Instant::now();
    let result = install_resolved_capture(
        &agent_config("adapter-agent"),
        &entry,
        HashMap::new(),
        tempdir.path(),
        &dest_dir,
    );
    let elapsed = started.elapsed();

    result.outcome.expect("adapter should install");
    assert!(
        elapsed < std::time::Duration::from_millis(1100),
        "adapter install took {elapsed:?}, expected concurrent steps"
    );
    assert_eq!(result.rows.len(), 2);
    assert_eq!(result.rows[0].step, "harness");
    assert_eq!(result.rows[1].step, "adapter");
}

#[test]
fn adapter_entry_skips_harness_step_when_harness_is_provided_by_adapter() {
    let tempdir = TempDir::new().expect("tempdir");
    let dest_dir = tempdir.path().join("bin");
    std::fs::create_dir(&dest_dir).expect("create bin dir");
    let adapter_binary = dest_dir.join("adapter-agent");
    let adapter_script = shell_string_for_write(&adapter_binary, "adapter");
    let entry = adapter_entry(
        "adapter-agent",
        "Adapter Agent",
        Some("docs/agents/adapter-agent.md"),
        harness_spec("adapter-agent-sdk", adapter_provided_install_set()),
        adapter_spec(
            "adapter-agent",
            shell_install_set(&adapter_script, "adapter-agent"),
        ),
    );

    let result = install_resolved_capture(
        &agent_config("adapter-agent"),
        &entry,
        HashMap::new(),
        tempdir.path(),
        &dest_dir,
    );

    let outcome = result.outcome.expect("adapter should install");
    assert_eq!(outcome.path(), adapter_binary.as_path());
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].step, "adapter");
    assert_eq!(result.rows[0].status, "ran");
}

#[test]
fn adapter_entry_runs_adapter_even_when_harness_fails() {
    let tempdir = TempDir::new().expect("tempdir");
    let dest_dir = tempdir.path().join("bin");
    std::fs::create_dir(&dest_dir).expect("create bin dir");
    let adapter_binary = dest_dir.join("adapter-agent");
    let adapter_script = shell_string_for_write(&adapter_binary, "adapter");
    let entry = adapter_entry(
        "adapter-agent",
        "Adapter Agent",
        Some("docs/agents/adapter-agent.md"),
        harness_spec(
            "upstream-agent",
            shell_install_set("false", "upstream-agent"),
        ),
        adapter_spec(
            "adapter-agent",
            shell_install_set(&adapter_script, "adapter-agent"),
        ),
    );

    let result = install_resolved_capture(
        &agent_config("adapter-agent"),
        &entry,
        HashMap::new(),
        tempdir.path(),
        &dest_dir,
    );

    assert!(matches!(
        result
            .outcome
            .expect_err("harness failure must fail install"),
        StackError::AgentInstallerFailed { .. }
    ));
    assert_eq!(result.rows.len(), 2);
    assert_eq!(result.rows[0].step, "harness");
    assert_eq!(result.rows[0].status, "failed");
    assert_eq!(result.rows[1].step, "adapter");
    assert_eq!(result.rows[1].status, "ran");
    assert!(adapter_binary.is_file());
}

#[test]
fn registry_installs_do_not_receive_agent_runtime_secrets() {
    let tempdir = TempDir::new().expect("tempdir");
    let binary_path = tempdir.path().join("secret-check-agent");
    let script = format!(
        "test -z \"$OPENCODE_API_KEY\" && printf '#!/bin/sh\\n' > {binary} && chmod 755 {binary}",
        binary = shell_quote_path(&binary_path),
    );
    let entry = native_entry(
        "secret-check-agent",
        "Secret Check Agent",
        Some("docs/agents/secret-check-agent.md"),
        harness_spec(
            "secret-check-agent",
            shell_install_set(&script, "secret-check-agent"),
        ),
    );
    let mut agent_env = HashMap::new();
    agent_env.insert("OPENCODE_API_KEY".to_owned(), "secret-value".to_owned());

    let result = install_resolved_capture(
        &agent_config("secret-check-agent"),
        &entry,
        agent_env,
        tempdir.path(),
        tempdir.path(),
    );

    let outcome = result
        .outcome
        .expect("registry installer should not see runtime secret");
    assert_eq!(outcome.path(), binary_path.as_path());
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].status, "ran");
}

#[test]
fn bootstrap_can_install_directly_into_managed_bin() {
    let tempdir = TempDir::new().expect("tempdir");
    let dest_dir = tempdir.path().join(".local").join("bin");
    let managed_opencode = dest_dir.join("opencode");
    let script = format!(
        "set -eu\n\
         managed_bin={dest_dir}\n\
         mkdir -p \"$managed_bin\"\n\
         printf '#!/bin/sh\\n' > \"$managed_bin/opencode\"\n\
         chmod 755 \"$managed_bin/opencode\"\n\
         test -x {managed_opencode}",
        dest_dir = shell_quote_path(&dest_dir),
        managed_opencode = shell_quote_path(&managed_opencode),
    );
    let entry = native_entry(
        "opencode",
        "OpenCode",
        Some("docs/agents/opencode.md"),
        harness_spec("opencode", shell_install_set(&script, "opencode")),
    );

    let result = install_resolved_capture(
        &agent_config("opencode"),
        &entry,
        HashMap::new(),
        tempdir.path(),
        &dest_dir,
    );

    let outcome = result.outcome.expect("managed opencode link should verify");
    assert_eq!(outcome.path(), managed_opencode.as_path());
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].status, "ran");
}

// Mirrors the blocked-postinstall stub npm 12 leaves behind: executable file,
// no shebang, so exec fails with ENOEXEC despite `creates` resolving.
fn shell_string_for_stub_write(path: &Path) -> String {
    format!(
        "mkdir -p {bin} && printf 'not a real binary' > {binary} && chmod 755 {binary}",
        bin = shell_quote_path(path.parent().expect("binary has parent")),
        binary = shell_quote_path(path),
    )
}

#[test]
fn spawn_gate_fails_step_on_unrunnable_binary() {
    let tempdir = TempDir::new().expect("tempdir");
    let dest_dir = tempdir.path().join("bin");
    std::fs::create_dir(&dest_dir).expect("create bin dir");
    let binary_path = dest_dir.join("stub-agent");
    let entry = native_entry(
        "stub-agent",
        "Stub Agent",
        Some("docs/agents/stub-agent.md"),
        harness_spec(
            "stub-agent",
            shell_install_set(&shell_string_for_stub_write(&binary_path), "stub-agent"),
        ),
    );

    let result = install_resolved_capture(
        &agent_config("stub-agent"),
        &entry,
        HashMap::new(),
        tempdir.path(),
        &dest_dir,
    );

    let err = result
        .outcome
        .expect_err("shebang-less stub must fail the spawn gate");
    assert!(
        matches!(err, StackError::AgentInstallerBinaryUnrunnable { .. }),
        "expected AgentInstallerBinaryUnrunnable, got {err:?}",
    );
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].status, "failed");
}

#[test]
fn spawn_gate_failure_advances_fallback_chain_to_npm() {
    let tempdir = TempDir::new().expect("tempdir");
    let dest_dir = tempdir.path().join("bin");
    std::fs::create_dir(&dest_dir).expect("create bin dir");
    // Shell path drops a stub for a different name than npm's `creates` so the
    // npm step's postcheck cannot accidentally resolve the shell leftovers.
    let stub_path = dest_dir.join("chain-agent-stub");
    let shell_script = format!(
        "{write} && cp {stub} {creates_path}",
        write = shell_string_for_stub_write(&stub_path),
        stub = shell_quote_path(&stub_path),
        creates_path = shell_quote_path(&dest_dir.join("chain-agent")),
    );
    write_fake_npm(
        &dest_dir,
        r#"
set -eu
if [ "$1" = "view" ]; then
  printf '"1.2.3"\n'
  exit 0
fi
if [ "$1" = "install" ]; then
  mkdir -p "$4/bin"
  printf '#!/bin/sh\n' > "$4/bin/chain-agent"
  chmod 755 "$4/bin/chain-agent"
  exit 0
fi
exit 99
"#,
    );
    let install = InstallSet {
        shell: Some(ShellInstall {
            script: shell_script,
            creates: "chain-agent".to_owned(),
            required_tools: Vec::new(),
        }),
        npm: Some(crate::runtime::install::agent_registry::NpmInstall {
            package: "chain-agent".to_owned(),
            creates: "chain-agent".to_owned(),
        }),
        ..InstallSet::default()
    };
    let entry = native_entry(
        "chain-agent",
        "Chain Agent",
        Some("docs/agents/chain-agent.md"),
        harness_spec("chain-agent", install),
    );

    let result = install_resolved_capture(
        &agent_config("chain-agent"),
        &entry,
        HashMap::new(),
        tempdir.path(),
        &dest_dir,
    );

    result
        .outcome
        .expect("npm fallback should replace the stub the shell path produced");
    assert_eq!(result.rows.len(), 2);
    assert_eq!(result.rows[0].status, "failed");
    assert_eq!(result.rows[0].method.as_deref(), Some("shell"));
    assert_eq!(result.rows[1].status, "ran");
    assert_eq!(result.rows[1].method.as_deref(), Some("npm"));
}

#[test]
fn executable_header_check_accepts_known_formats_and_rejects_text() {
    let tempdir = TempDir::new().expect("tempdir");
    let case = |name: &str, bytes: &[u8]| {
        let path = tempdir.path().join(name);
        std::fs::write(&path, bytes).expect("write header fixture");
        verify_executable_header(&path)
    };
    assert!(case("elf", b"\x7fELF\x02\x01\x01\x00").is_ok());
    assert!(case("shebang", b"#!/bin/sh\n").is_ok());
    assert!(case("macho", &[0xcf, 0xfa, 0xed, 0xfe, 0x00, 0x00]).is_ok());
    assert!(case("fat-macho", &[0xca, 0xfe, 0xba, 0xbe, 0x00, 0x00]).is_ok());
    assert!(case("fat-macho-64", &[0xca, 0xfe, 0xba, 0xbf, 0x00, 0x00]).is_ok());
    assert!(case("fat-macho-64-cigam", &[0xbf, 0xba, 0xfe, 0xca, 0x00, 0x00]).is_ok());
    assert!(case("empty", b"").is_err());
    assert!(case("short-text", b"ok").is_err());
    assert!(case("stub", b"echo \"Error: postinstall was not run.\"").is_err());
}

#[test]
fn escape_hatch_reinstalls_over_unrunnable_existing_binary() {
    let tempdir = TempDir::new().expect("tempdir");
    let binary = tempdir.path().join("hatch-agent");
    std::fs::write(&binary, b"not a real binary").expect("write stub");
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).expect("chmod stub");
    let script = format!(
        "printf '#!/bin/sh\\n' > {binary} && chmod 755 {binary}",
        binary = shell_quote_path(&binary),
    );
    let install = install_config(&script, binary.to_str().expect("utf8 tempdir path"));

    let result = run_installer_capture(&install, None, HashMap::new(), tempdir.path());

    match result
        .outcome
        .expect("recipe should replace the unrunnable pre-existing binary")
    {
        InstallerOutcome::Installed { path, .. } => assert_eq!(path, binary),
        other => panic!("expected Installed after reinstall, got {other:?}"),
    }
}

#[test]
fn init_resume_verifier_rejects_unrunnable_binary() {
    let tempdir = TempDir::new().expect("tempdir");
    let workspace_root = tempdir.path().join("workspace");
    std::fs::create_dir_all(workspace_root.join("bin")).expect("workspace bin");
    let stub = workspace_root.join("bin/stub-agent");
    std::fs::write(&stub, b"not a real binary").expect("write stub");
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).expect("chmod stub");

    assert_eq!(
        resolve_creates_for_init_resume("bin/stub-agent", &workspace_root, &[], None),
        None,
        "a resolvable but unspawnable binary must read as absent so resume re-installs",
    );
}

#[test]
fn init_resume_verifier_enforces_pin_before_probing() {
    let tempdir = TempDir::new().expect("tempdir");
    let workspace_root = tempdir.path().join("workspace");
    std::fs::create_dir_all(workspace_root.join("bin")).expect("workspace bin");
    let binary = workspace_root.join("bin/pinned-agent");
    std::fs::write(&binary, b"#!/bin/sh\n").expect("write binary");
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    assert_eq!(
        resolve_creates_for_init_resume("bin/pinned-agent", &workspace_root, &[], Some("deadbeef"),),
        None,
        "a binary failing the operator's pin must read as absent so resume re-installs",
    );
    let sha256 = sha256_of_file(&binary).expect("hash binary");
    assert_eq!(
        resolve_creates_for_init_resume("bin/pinned-agent", &workspace_root, &[], Some(&sha256)),
        Some(binary),
        "a binary matching its pin is probed and accepted",
    );
}

#[test]
fn declared_pin_keeps_step_gate_from_executing_binary() {
    let tempdir = TempDir::new().expect("tempdir");
    let dest_dir = tempdir.path().join("bin");
    let binary_path = dest_dir.join("pin-agent");
    // The installed script passes the header check (`#!`) but a spawn probe
    // would deterministically fail: the interpreter does not exist. So if the
    // step-level gate ever regressed to probing under a declared pin, the
    // outcome would be AgentInstallerBinaryUnrunnable instead of the pin
    // mismatch — the assertion below proves the probe never ran.
    let script = format!(
        "mkdir -p {bin} && printf '#!/nonexistent/acp-stack-test-interpreter\\nexit 0\\n' > {binary} && chmod 755 {binary}",
        bin = shell_quote_path(&dest_dir),
        binary = shell_quote_path(&binary_path),
    );
    let entry = native_entry(
        "pin-agent",
        "Pin Agent",
        Some("docs/agents/pin-agent.md"),
        harness_spec("pin-agent", shell_install_set(&script, "pin-agent")),
    );
    let mut agent = agent_config("pin-agent");
    agent.expected_sha256 = Some("deadbeef".to_owned());

    let result =
        install_resolved_capture(&agent, &entry, HashMap::new(), tempdir.path(), &dest_dir);

    let err = result
        .outcome
        .expect_err("a mismatched pin must fail final verification");
    assert!(
        matches!(err, StackError::AgentSha256Mismatch { .. }),
        "expected AgentSha256Mismatch (pin checked before any probe), got {err:?}",
    );
}

#[test]
fn declared_pin_step_gate_still_rejects_shebang_less_stub() {
    let tempdir = TempDir::new().expect("tempdir");
    let dest_dir = tempdir.path().join("bin");
    std::fs::create_dir(&dest_dir).expect("create bin dir");
    let binary_path = dest_dir.join("pin-stub-agent");
    let entry = native_entry(
        "pin-stub-agent",
        "Pin Stub Agent",
        Some("docs/agents/pin-stub-agent.md"),
        harness_spec(
            "pin-stub-agent",
            shell_install_set(&shell_string_for_stub_write(&binary_path), "pin-stub-agent"),
        ),
    );
    let mut agent = agent_config("pin-stub-agent");
    agent.expected_sha256 = Some("deadbeef".to_owned());

    let result =
        install_resolved_capture(&agent, &entry, HashMap::new(), tempdir.path(), &dest_dir);

    let err = result
        .outcome
        .expect_err("the header-only step gate must still reject a stub under a declared pin");
    assert!(
        matches!(err, StackError::AgentInstallerBinaryUnrunnable { .. }),
        "expected AgentInstallerBinaryUnrunnable, got {err:?}",
    );
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].status, "failed");
}

#[test]
fn spawn_gate_probe_fails_on_missing_interpreter() {
    let tempdir = TempDir::new().expect("tempdir");
    let binary = tempdir.path().join("bad-interp-agent");
    std::fs::write(
        &binary,
        b"#!/nonexistent/acp-stack-test-interpreter\nexit 0\n",
    )
    .expect("write bad-interpreter script");
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755))
        .expect("chmod bad-interpreter script");

    let err = verify_binary_spawns(&binary, tempdir.path(), &[])
        .expect_err("a script whose interpreter is missing cannot spawn");
    assert!(
        matches!(err, StackError::AgentInstallerBinaryUnrunnable { .. }),
        "expected AgentInstallerBinaryUnrunnable, got {err:?}",
    );
}

#[test]
fn spawn_gate_probe_runs_exec_only_binary_when_header_read_is_denied() {
    let tempdir = TempDir::new().expect("tempdir");
    let binary = tempdir.path().join("exec-only-agent");
    std::fs::write(&binary, b"#!/bin/sh\n").expect("write exec-only script");
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o111))
        .expect("chmod exec-only script");
    if std::fs::File::open(&binary).is_ok() {
        // Root can read a mode-0111 file, so there is no denied read for the
        // header check to skip on; the scenario this test covers is absent.
        return;
    }

    verify_binary_spawns(&binary, tempdir.path(), &[])
        .expect("an unreadable-but-executable script must pass via the spawn probe");
}

fn shell_quote_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    format!("'{}'", text.replace('\'', "'\\''"))
}
