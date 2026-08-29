use super::super::*;
use super::support::*;
use tempfile::TempDir;

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
        None,
        tempdir.path(),
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
    // Overlap is proven from start stamps; wall time includes the version probes.
    let harness_stamp = tempdir.path().join("harness-started");
    let adapter_stamp = tempdir.path().join("adapter-started");
    let harness_script = format!(
        ": > {stamp}; sleep 0.6; mkdir -p {bin}; printf '#!/bin/sh\\n' > {harness}; chmod 755 {harness}",
        stamp = shell_quote_path(&harness_stamp),
        bin = shell_quote_path(&dest_dir),
        harness = shell_quote_path(&harness_binary),
    );
    let adapter_script = format!(
        ": > {stamp}; sleep 0.6; mkdir -p {bin}; printf '#!/bin/sh\\n' > {adapter}; chmod 755 {adapter}",
        stamp = shell_quote_path(&adapter_stamp),
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

    let result = install_resolved_capture(
        &agent_config("adapter-agent"),
        &entry,
        HashMap::new(),
        tempdir.path(),
        &dest_dir,
        None,
        tempdir.path(),
    );

    result.outcome.expect("adapter should install");
    let started_at = |stamp: &Path| -> std::time::SystemTime {
        std::fs::metadata(stamp)
            .expect("recipe wrote its start stamp")
            .modified()
            .expect("stamp mtime")
    };
    let (harness_started, adapter_started) =
        (started_at(&harness_stamp), started_at(&adapter_stamp));
    let skew = harness_started
        .duration_since(adapter_started)
        .or_else(|_| adapter_started.duration_since(harness_started))
        .expect("stamps are ordered");
    assert!(
        skew < std::time::Duration::from_millis(600),
        "recipes started {skew:?} apart, expected concurrent steps"
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
        None,
        tempdir.path(),
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
        None,
        tempdir.path(),
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
