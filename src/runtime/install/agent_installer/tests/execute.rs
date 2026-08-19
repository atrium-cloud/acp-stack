use super::super::*;
use super::support::*;
use std::os::unix::fs::PermissionsExt;
use tempfile::TempDir;

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

    let result = run_installer_capture(&install, None, HashMap::new(), &missing_workspace, None);
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
        None,
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
        None,
    );
    let outcome = result.outcome.expect("managed binary should resolve");
    assert_eq!(outcome.path(), binary_path.as_path());
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].status, "ran");
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
        None,
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
        None,
    );

    let outcome = result.outcome.expect("managed opencode link should verify");
    assert_eq!(outcome.path(), managed_opencode.as_path());
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].status, "ran");
}

#[test]
fn running_row_is_visible_while_step_executes() {
    let (tempdir, store) = open_store();
    let workspace_root = workspace_root();
    // The script blocks until the test releases it, so the test can observe
    // the `running` row while the step is genuinely in flight.
    let proceed = tempdir.path().join("proceed");
    let script = format!(
        "for i in $(seq 1 200); do [ -f {proceed} ] && break; sleep 0.05; done",
        proceed = shell_quote_path(&proceed),
    );
    let install = install_config(&script, "definitely-not-a-real-binary-xyz123");
    let state_path = store.path().to_path_buf();
    let worker = std::thread::spawn(move || {
        let worker_store = StateStore::open(&state_path).expect("worker store");
        run_installer(
            "test-agent",
            &install,
            None,
            HashMap::new(),
            &workspace_root,
            &worker_store,
            None,
        )
    });

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let active = loop {
        let active = store
            .query_active_installer_runs(Some("test-agent"))
            .expect("active query");
        if !active.is_empty() {
            break active;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "running row never appeared while the step was blocked"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].step, "install");
    assert_eq!(active[0].status, crate::state::INSTALLER_STATUS_RUNNING);
    assert!(active[0].finished_at.is_none());

    std::fs::write(&proceed, b"go").expect("release installer");
    // The script produces no `creates` binary, so the step finalizes as
    // failed; the point is the running row was updated in place, not
    // duplicated by a second insert.
    let outcome = worker.join().expect("worker join");
    outcome.expect_err("no creates binary produced");
    let runs = store.query_installer_runs(10).expect("history");
    assert_eq!(
        runs.len(),
        1,
        "the running row must be finalized in place, not duplicated"
    );
    assert_eq!(runs[0].id, active[0].id);
    assert_eq!(runs[0].status, "failed");
    assert!(runs[0].finished_at.is_some());
    assert!(
        store
            .query_active_installer_runs(None)
            .expect("active query")
            .is_empty()
    );
}

#[test]
fn panicking_step_finalizes_its_running_row_before_unwinding() {
    let (_tempdir, store) = open_store();
    let sink = ReconnectingInstallerSink::new(store.path().to_path_buf());
    let progress = InstallProgress {
        sink: &sink,
        agent_id: "test-agent",
        operation: INSTALLER_OPERATION_INSTALL,
        log_base: None,
    };

    let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        super::super::execute::run_guarded_install_step(
            STEP_INSTALL,
            INSTALL_METHOD_SHELL,
            Some(&progress),
            || {
                panic!("simulated worker panic");
            },
        );
    }))
    .expect_err("the panic must keep unwinding to the thread-join fallback");
    drop(payload);

    assert!(
        store
            .query_active_installer_runs(None)
            .expect("active query")
            .is_empty(),
        "a panicked step must not stay active forever"
    );
    let runs = store.query_installer_runs(10).expect("history");
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, "error");
    assert_eq!(runs[0].step, "install");
    assert!(runs[0].finished_at.is_some());
    assert!(runs[0].stderr.contains("panicked"), "{:?}", runs[0].stderr);
}
