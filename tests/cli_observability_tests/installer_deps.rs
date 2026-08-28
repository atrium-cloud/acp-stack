use acp_stack::state::{
    INSTALLER_METHOD_GITHUB, INSTALLER_OPERATION_INSTALL, InstallerRunInput, StateStore,
    default_state_path,
};
use predicates::prelude::PredicateBooleanExt as _;
use serde_json::Value;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use crate::common::cli::*;
use crate::support::*;

#[test]
fn agent_check_reports_no_runs_when_state_is_empty() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");

    acps_command_without_placebo(tempdir.path())
        .args(["agent", "check"])
        .assert()
        .failure()
        .stdout(predicates::str::contains("install: not installed"));
}

#[test]
fn agent_check_format_json_reports_steps() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");

    let output = acps_command(tempdir.path())
        .args(["agent", "check", "--format", "json"])
        .assert()
        .failure()
        .get_output()
        .stdout
        .clone();
    let body: Value = serde_json::from_slice(&output).expect("agent check json parses");
    assert_eq!(body["agent"], "opencode");
    assert_eq!(body["ok"], false);
    assert_eq!(body["steps"][0]["step"], "install");
    assert_eq!(body["steps"][0]["result"]["status"], "not_installed");
}

#[test]
fn agent_check_reports_missing_adapter_step() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), amp_config()).expect("config should be written");

    let state_path = default_state_path(tempdir.path());
    fs::create_dir_all(state_path.parent().expect("state parent dir"))
        .expect("state dir should be created");
    let store = StateStore::open(&state_path).expect("state should open");
    store.migrate().expect("migration should pass");
    store
        .append_installer_run(InstallerRunInput {
            agent_id: "amp",
            started_at: "2026-05-22T00:00:00.000000000Z",
            finished_at: Some("2026-05-22T00:00:01.000000000Z"),
            status: "ran",
            stdout: "",
            stderr: "",
            exit_status: Some(0),
            step: "harness",
            version: None,
            operation: INSTALLER_OPERATION_INSTALL,
            method: None,
            log_dir: None,
            apply_run_id: None,
        })
        .expect("seed harness row");
    drop(store);

    acps_command_without_placebo(tempdir.path())
        .args(["agent", "check"])
        .assert()
        .failure()
        .stdout(predicates::str::contains("harness: unknown"))
        .stdout(predicates::str::contains("adapter: not installed"));
}

#[test]
fn installer_history_reports_empty_state_when_nothing_recorded() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    fs::create_dir_all(tempdir.path().join(".config/acp-stack"))
        .expect("config dir should be created");
    fs::write(
        tempdir.path().join(".config/acp-stack/acps-config.toml"),
        VALID_CONFIG,
    )
    .expect("config should be written");

    acps_command(tempdir.path())
        .args(["installer", "history"])
        .assert()
        .success()
        .stdout(predicates::str::contains("no installer runs recorded"));
}

#[test]
fn installer_history_renders_rows_with_filter() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    fs::create_dir_all(tempdir.path().join(".config/acp-stack"))
        .expect("config dir should be created");
    fs::write(
        tempdir.path().join(".config/acp-stack/acps-config.toml"),
        VALID_CONFIG,
    )
    .expect("config should be written");

    let state_path = default_state_path(tempdir.path());
    fs::create_dir_all(state_path.parent().expect("state parent dir"))
        .expect("state dir should be created");
    let store = StateStore::open(&state_path).expect("state should open");
    store.migrate().expect("migration should pass");
    store
        .append_installer_run(InstallerRunInput {
            agent_id: "opencode",
            started_at: "2026-05-22T00:00:00.000000000Z",
            finished_at: Some("2026-05-22T00:00:00.250000000Z"),
            status: "ran",
            stdout: "",
            stderr: "",
            exit_status: Some(0),
            step: "harness",
            version: Some("v1.0.0"),
            operation: INSTALLER_OPERATION_INSTALL,
            method: Some(INSTALLER_METHOD_GITHUB),
            log_dir: None,
            apply_run_id: None,
        })
        .expect("seed harness row");
    store
        .append_installer_run(InstallerRunInput {
            agent_id: "codex",
            started_at: "2026-05-22T00:00:01.000000000Z",
            finished_at: Some("2026-05-22T00:00:02.000000000Z"),
            status: "failed",
            stdout: "",
            stderr: "boom",
            exit_status: Some(2),
            step: "adapter",
            version: None,
            operation: INSTALLER_OPERATION_INSTALL,
            method: Some(INSTALLER_METHOD_GITHUB),
            log_dir: None,
            apply_run_id: None,
        })
        .expect("seed adapter row");
    drop(store);

    acps_command(tempdir.path())
        .args(["installer", "history"])
        .assert()
        .success()
        .stdout(predicates::str::contains("started_at"))
        .stdout(predicates::str::contains("codex"))
        .stdout(predicates::str::contains("opencode"))
        .stdout(predicates::str::contains("v1.0.0"))
        .stdout(predicates::str::contains("failed"));

    acps_command(tempdir.path())
        .args(["installer", "history", "--agent", "opencode"])
        .assert()
        .success()
        .stdout(predicates::str::contains("opencode"))
        .stdout(predicates::str::contains("v1.0.0"))
        .stdout(predicates::str::contains("codex").not());
}

#[test]
fn installer_history_format_json_renders_runs() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    fs::create_dir_all(tempdir.path().join(".config/acp-stack"))
        .expect("config dir should be created");
    fs::write(
        tempdir.path().join(".config/acp-stack/acps-config.toml"),
        VALID_CONFIG,
    )
    .expect("config should be written");

    let state_path = default_state_path(tempdir.path());
    fs::create_dir_all(state_path.parent().expect("state parent dir"))
        .expect("state dir should be created");
    let store = StateStore::open(&state_path).expect("state should open");
    store.migrate().expect("migration should pass");
    store
        .append_installer_run(InstallerRunInput {
            agent_id: "opencode",
            started_at: "2026-05-22T01:00:00.000000000Z",
            finished_at: Some("2026-05-22T01:00:01.000000000Z"),
            status: "ran",
            stdout: "hi",
            stderr: "",
            exit_status: Some(0),
            step: "harness",
            version: Some("v1.0.0"),
            operation: INSTALLER_OPERATION_INSTALL,
            method: Some(INSTALLER_METHOD_GITHUB),
            log_dir: Some("/tmp/installer-logs/opencode/harness"),
            apply_run_id: None,
        })
        .expect("seed row");
    drop(store);

    let output = acps_command(tempdir.path())
        .args(["installer", "history", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let body: Value = serde_json::from_slice(&output).expect("installer json parses");
    let runs = body["runs"].as_array().expect("runs should be an array");
    assert_eq!(runs.len(), 1, "{body}");
    assert_eq!(runs[0]["agent_id"], "opencode");
    assert_eq!(runs[0]["duration_ms"], 1_000);
}

#[test]
fn installer_history_renders_log_dir_continuation_line() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    fs::create_dir_all(tempdir.path().join(".config/acp-stack"))
        .expect("config dir should be created");
    fs::write(
        tempdir.path().join(".config/acp-stack/acps-config.toml"),
        VALID_CONFIG,
    )
    .expect("config should be written");

    let state_path = default_state_path(tempdir.path());
    fs::create_dir_all(state_path.parent().expect("state parent dir"))
        .expect("state dir should be created");
    let store = StateStore::open(&state_path).expect("state should open");
    store.migrate().expect("migration should pass");
    store
        .append_installer_run(InstallerRunInput {
            agent_id: "opencode",
            started_at: "2026-05-22T01:00:00.000000000Z",
            finished_at: Some("2026-05-22T01:00:01.000000000Z"),
            status: "ran",
            stdout: "hi",
            stderr: "",
            exit_status: Some(0),
            step: "harness",
            version: Some("v1.0.0"),
            operation: INSTALLER_OPERATION_INSTALL,
            method: Some(INSTALLER_METHOD_GITHUB),
            log_dir: Some("/tmp/installer-logs/opencode/2026-05-22T01:00:00.000000000Z/harness"),
            apply_run_id: None,
        })
        .expect("seed row with log_dir");
    drop(store);

    acps_command(tempdir.path())
        .args(["installer", "history"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "log_dir: /tmp/installer-logs/opencode/2026-05-22T01:00:00.000000000Z/harness",
        ));
}

#[test]
fn installer_history_rejects_zero_limit() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    fs::create_dir_all(tempdir.path().join(".config/acp-stack"))
        .expect("config dir should be created");
    fs::write(
        tempdir.path().join(".config/acp-stack/acps-config.toml"),
        VALID_CONFIG,
    )
    .expect("config should be written");

    acps_command(tempdir.path())
        .args(["installer", "history", "--limit", "0"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("limit must be"));
}

#[test]
fn deps_apply_prints_before_and_after_status() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");

    let dependency_one_name = "deps-apply-before-after-marker-one";
    let dependency_two_name = "deps-apply-before-after-marker-two";
    let feature = "deps-apply-before-after";
    let marker_one = tempdir.path().join("deps-apply-marker-one");
    let marker_two = tempdir.path().join("deps-apply-marker-two");
    let shell_one = format!(
        "printf '#!/bin/sh\\nexit 0\\n' > {marker} && chmod 755 {marker}",
        marker = shell_quote_path(&marker_one),
    );
    let shell_two = format!(
        "printf '#!/bin/sh\\nexit 0\\n' > {marker} && chmod 755 {marker}",
        marker = shell_quote_path(&marker_two),
    );
    let config = VALID_CONFIG.replace(
        "[agent]",
        &format!(
            r#"[[dependencies.commands]]
	name = "{dependency_one_name}"
	required = true
	feature = "{feature}"

	[dependencies.commands.install]
	shell = {}
	creates = {}

[[dependencies.commands]]
	name = "{dependency_two_name}"
	required = true
	feature = "{feature}"

	[dependencies.commands.install]
	shell = {}
	creates = {}

	[agent]"#,
            toml_string(&shell_one),
            toml_string(&marker_one.to_string_lossy()),
            toml_string(&shell_two),
            toml_string(&marker_two.to_string_lossy()),
        ),
    );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    let state_path = default_state_path(tempdir.path());
    fs::create_dir_all(state_path.parent().expect("state parent dir"))
        .expect("state dir should be created");
    let store = StateStore::open(&state_path).expect("state should open");
    store.migrate().expect("migration should pass");
    drop(store);
    seed_auth_verifiers(tempdir.path(), SESSION_KEY, ADMIN_KEY);

    let output = acps_command(tempdir.path())
        .args([
            "deps",
            "apply",
            "--yes",
            "--feature",
            feature,
            "--admin-key",
            ADMIN_KEY,
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("stdout should be utf8");

    let before_index = stdout.find("before:\n").expect("before section");
    let progress_one_index = stdout
        .find(&format!(
            "progress: applying dependency 1/2: {dependency_one_name}\n"
        ))
        .expect("first progress line");
    let progress_two_index = stdout
        .find(&format!(
            "progress: applying dependency 2/2: {dependency_two_name}\n"
        ))
        .expect("second progress line");
    let results_index = stdout.find("results:\n").expect("results section");
    let after_index = stdout.find("after:\n").expect("after section");
    let audit_index = stdout.find("audit run: dap_").expect("audit run line");
    assert!(
        progress_one_index < progress_two_index
            && progress_two_index < before_index
            && before_index < results_index
            && results_index < after_index
            && after_index < audit_index,
        "expected before/results/after ordering, got:\n{stdout}",
    );
    assert!(
        stdout[before_index..results_index].contains(&format!("  MISS {dependency_one_name}")),
        "before section must report missing dependency, got:\n{stdout}",
    );
    assert!(
        stdout[before_index..results_index].contains(&format!("  MISS {dependency_two_name}")),
        "before section must report missing dependency, got:\n{stdout}",
    );
    assert!(
        stdout[after_index..].contains(&format!("  OK   {dependency_one_name}")),
        "after section must report available dependency, got:\n{stdout}",
    );
    assert!(
        stdout[after_index..].contains(&format!("  OK   {dependency_two_name}")),
        "after section must report available dependency, got:\n{stdout}",
    );
}

#[test]
fn deps_apply_exits_nonzero_and_prints_manual_commands_on_privilege_skip() {
    // SAFETY: `geteuid()` is always safe — no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        // As root the escalation probe short-circuits to "run directly"
        // and the skip path under test is unreachable.
        return;
    }
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");

    // A fake sudo that exits 1, first on PATH, collapses the escalation probe
    // to Unavailable deterministically.
    let fake_bin = tempdir.path().join("fake-bin");
    fs::create_dir_all(&fake_bin).expect("fake bin dir");
    let fake_sudo = fake_bin.join("sudo");
    fs::write(&fake_sudo, "#!/bin/sh\nexit 1\n").expect("fake sudo");
    fs::set_permissions(&fake_sudo, fs::Permissions::from_mode(0o755)).expect("chmod fake sudo");
    let host_path = std::env::var("PATH").expect("PATH should be set");
    let path_with_fake_sudo = format!("{}:{host_path}", fake_bin.to_string_lossy());

    let dependency_name = "deps-apply-privilege-skip-marker";
    let config = VALID_CONFIG.replace(
        "[agent]",
        &format!(
            r#"[[dependencies.commands]]
	name = "{dependency_name}"
	required = true

	[dependencies.commands.install]
	shell = "exit 0"
	creates = "{dependency_name}"
	scope = "system"

	[agent]"#,
        ),
    );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    let state_path = default_state_path(tempdir.path());
    fs::create_dir_all(state_path.parent().expect("state parent dir"))
        .expect("state dir should be created");
    let store = StateStore::open(&state_path).expect("state should open");
    store.migrate().expect("migration should pass");
    drop(store);
    seed_auth_verifiers(tempdir.path(), SESSION_KEY, ADMIN_KEY);

    // Unlike init (which skips and continues), the explicit imperative
    // command must exit non-zero and hand the operator the manual commands.
    acps_command(tempdir.path())
        .env("PATH", path_with_fake_sudo)
        .args(["deps", "apply", "--yes", "--admin-key", ADMIN_KEY])
        .assert()
        .failure()
        .stdout(predicates::str::contains(
            "no passwordless sudo; they will be skipped and recorded as privilege_required",
        ))
        .stdout(predicates::str::contains(format!(
            "privreq     {dependency_name}"
        )))
        .stdout(predicates::str::contains("sudo /bin/bash -c 'exit 0'"))
        .stderr(predicates::str::contains("need root privilege"));

    let store = StateStore::open(&state_path).expect("state should reopen");
    let rows = store
        .query_installer_runs_filtered(Some("deps_apply"), 10)
        .expect("installer history should query");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].status, "privilege_required");
}

#[test]
fn deps_apply_requires_admin_key() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command(tempdir.path())
        .args(["deps", "apply", "--yes"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--admin-key"));
}

#[test]
fn deps_check_format_json_reports_dependency_shape() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");

    let config = VALID_CONFIG.replace(
        "[agent]",
        r#"[[dependencies.commands]]
name = "deps-check-json"
required = true

[agent]"#,
    );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    let output = acps_command(tempdir.path())
        .args(["deps", "check", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let body: Value = serde_json::from_slice(&output).expect("deps check json parses");
    let deps = body["dependencies"]
        .as_array()
        .expect("dependencies should be an array");
    assert_eq!(deps[0]["name"], "deps-check-json");
    assert_eq!(deps[0]["available"], false);
}

#[test]
fn deps_apply_format_json_omits_stderr_tail() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");

    let marker = tempdir.path().join("deps-apply-failed-marker");
    let config = VALID_CONFIG.replace(
        "[agent]",
        &format!(
            r#"[[dependencies.commands]]
name = "deps-apply-json-failure"
required = true

[dependencies.commands.install]
shell = "printf 'token sk-test-secret' >&2; exit 7"
creates = {}

[agent]"#,
            toml_string(&marker.to_string_lossy()),
        ),
    );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    let state_path = default_state_path(tempdir.path());
    fs::create_dir_all(state_path.parent().expect("state parent dir"))
        .expect("state dir should be created");
    let store = StateStore::open(&state_path).expect("state should open");
    store.migrate().expect("migration should pass");
    drop(store);
    seed_auth_verifiers(tempdir.path(), SESSION_KEY, ADMIN_KEY);

    let output = acps_command(tempdir.path())
        .args([
            "deps",
            "apply",
            "--yes",
            "--format",
            "json",
            "--admin-key",
            ADMIN_KEY,
        ])
        .assert()
        .failure()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8_lossy(&output);
    assert!(!stdout.contains("running dependency install actions"));
    assert!(!stdout.contains("progress: applying dependency"));
    assert!(!stdout.contains("sk-test-secret"));
    let body: Value = serde_json::from_slice(&output).expect("deps apply json parses");
    assert!(
        body["apply_run_id"]
            .as_str()
            .is_some_and(|value| value.starts_with("dap_")),
        "{body}",
    );
    let outcome = &body["results"][0]["outcome"];
    assert_eq!(outcome["kind"], "failed");
    assert_eq!(outcome["exit_code"], 7);
    assert_eq!(outcome["stderr_tail_omitted"], true);
    assert!(outcome.get("stderr_tail").is_none(), "{body}");
}

#[test]
fn deps_apply_persists_one_apply_run_id_for_all_rows() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");

    let installed_marker = tempdir.path().join("deps-apply-installed-marker");
    let skipped_marker = tempdir.path().join("deps-apply-skipped-marker");
    fs::write(&skipped_marker, "#!/bin/sh\nexit 0\n").expect("skipped marker should be written");
    #[cfg(unix)]
    fs::set_permissions(&skipped_marker, fs::Permissions::from_mode(0o755))
        .expect("skipped marker should be executable");
    let shell = format!(
        "printf '#!/bin/sh\\nexit 0\\n' > {marker} && chmod 755 {marker}",
        marker = shell_quote_path(&installed_marker),
    );
    let config = VALID_CONFIG.replace(
        "[agent]",
        &format!(
            r#"[[dependencies.commands]]
name = "deps-apply-installed"
required = true

[dependencies.commands.install]
shell = {}
creates = {}

[[dependencies.commands]]
name = "deps-apply-skipped"
required = true

[dependencies.commands.install]
shell = "exit 99"
creates = {}

[agent]"#,
            toml_string(&shell),
            toml_string(&installed_marker.to_string_lossy()),
            toml_string(&skipped_marker.to_string_lossy()),
        ),
    );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    let state_path = default_state_path(tempdir.path());
    fs::create_dir_all(state_path.parent().expect("state parent dir"))
        .expect("state dir should be created");
    let store = StateStore::open(&state_path).expect("state should open");
    store.migrate().expect("migration should pass");
    drop(store);
    seed_auth_verifiers(tempdir.path(), SESSION_KEY, ADMIN_KEY);

    acps_command(tempdir.path())
        .args(["deps", "apply", "--yes", "--admin-key", ADMIN_KEY])
        .assert()
        .success();

    let store = StateStore::open(&state_path).expect("state should open");
    let rows = store
        .query_installer_runs_filtered(Some("deps_apply"), 10)
        .expect("deps rows should query");
    assert_eq!(
        rows.len(),
        2,
        "expected one row per declared install action"
    );
    let apply_run_id = rows[0]
        .apply_run_id
        .as_deref()
        .expect("apply_run_id should be present");
    assert!(
        apply_run_id.starts_with("dap_"),
        "apply_run_id should use the deps apply prefix, got {apply_run_id}"
    );
    assert!(
        rows.iter()
            .all(|row| row.apply_run_id.as_deref() == Some(apply_run_id)),
        "all rows from one invocation must share apply_run_id, got {rows:?}"
    );
    assert!(rows.iter().any(|row| row.status == "installed"));
    assert!(rows.iter().any(|row| row.status == "skipped"));
}
