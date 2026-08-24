use super::super::step_runners::select_install_path;
use super::super::*;
use super::support::*;
use tempfile::TempDir;

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
        None,
    );

    result.outcome.expect("npm install should pass");
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].status, "ran");
    assert_eq!(result.rows[0].version.as_deref(), Some("1.2.3"));
}

#[test]
fn unpinned_npm_install_accepts_array_version_output() {
    // `npm view <pkg> version --json` returns a JSON array on some hosts, ordered ascending, so the
    // last element is the version to install.
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
        None,
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
        None,
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
        None,
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
        None,
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
