use super::super::*;
use super::support::*;
use crate::runtime::process_runner::{
    resolved_python_interpreter, resolved_python_interpreter_with_timeout,
};
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;
use tempfile::TempDir;

/// node-gyp reads `npm_config_python` to decide what to spawn, so the variable must reach the
/// installer subprocess itself, not merely be computed.
#[test]
fn an_install_step_sees_a_resolved_python_interpreter() {
    // A host without `python3` leaves the variable unset, so there is nothing to assert.
    if resolved_python_interpreter(None).is_none() {
        return;
    }
    let tempdir = TempDir::new().expect("tempdir");
    let install = shell_install_set(
        "printf 'interpreter=%s\\n' \"${npm_config_python:-UNSET}\"; \
         test -x \"${npm_config_python:-}\" && printf 'executable=yes\\n'",
        "python-env-agent",
    );
    let entry = native_entry(
        "python-env-agent",
        "Python Env Agent",
        Some("docs/agents/python-env-agent.md"),
        harness_spec("python-env-agent", install),
    );

    let result = install_resolved_capture(
        &agent_config("python-env-agent"),
        &entry,
        HashMap::new(),
        tempdir.path(),
        tempdir.path(),
        None,
        tempdir.path(),
    );

    let stdout = &result.rows[0].stdout;
    assert!(
        !stdout.contains("interpreter=UNSET"),
        "the installer must inherit npm_config_python, got `{stdout}`"
    );
    assert!(
        stdout.contains("interpreter=/"),
        "the value must be an absolute interpreter path, got `{stdout}`"
    );
    assert!(
        stdout.contains("executable=yes"),
        "the value must point at an executable file, got `{stdout}`"
    );
}

/// Resolution is skipped rather than guessed when the child's PATH has no `python3`; a bad
/// override turns a clear "python not found" into a confusing spawn failure.
#[test]
fn an_empty_path_resolves_no_interpreter() {
    let empty = TempDir::new().expect("tempdir");
    let path = std::ffi::OsString::from(empty.path());
    assert_eq!(resolved_python_interpreter(Some(&path)), None);
}

/// A hung version-manager shim must degrade to "no override": the probe runs before the step
/// deadline is computed, so an unbounded wait would sit outside every budget.
#[test]
fn a_hung_python_shim_resolves_no_interpreter_promptly() {
    let tempdir = TempDir::new().expect("tempdir");
    let shim = tempdir.path().join("python3");
    std::fs::write(&shim, "#!/bin/sh\nsleep 60\n").expect("write shim");
    let permissions = std::fs::Permissions::from_mode(0o755);
    std::fs::set_permissions(&shim, permissions).expect("chmod shim");
    let path = std::ffi::OsString::from(tempdir.path());

    let started = std::time::Instant::now();
    let resolved =
        resolved_python_interpreter_with_timeout(Some(&path), Duration::from_millis(200));

    assert_eq!(resolved, None);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the probe must give up on a hung shim promptly, took {:?}",
        started.elapsed()
    );
}
