use crate::common::cli::*;
use crate::support::*;
use predicates::prelude::PredicateBooleanExt as _;
use serde_json::Value;

#[test]
fn secrets_set_only_captures_first_line_of_stdin() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let (_, admin_key) = run_init_with_home(tempdir.path());

    acps_command(tempdir.path())
        .args([
            "secrets",
            "set",
            "MULTILINE_TEST",
            "--admin-key",
            admin_key.as_str(),
        ])
        .write_stdin("first-line\nsecond-line\n")
        .assert()
        .success();

    let store = acp_stack::secrets::SecretStore::open(tempdir.path()).expect("open store");
    assert_eq!(store.get("MULTILINE_TEST").expect("get"), "first-line");
}

#[test]
fn secrets_set_requires_admin_key() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    acps_command(tempdir.path())
        .args(["secrets", "set", "OPENCODE_API_KEY"])
        .write_stdin("attacker-supplied")
        .assert()
        .failure()
        .stderr(predicates::str::contains("--admin-key"));
}

#[test]
fn secrets_set_allows_old_auth_ref_names_with_admin_key() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let (_, admin_key) = run_init_with_home(tempdir.path());

    acps_command(tempdir.path())
        .args([
            "secrets",
            "set",
            "ACP_STACK_SESSION_KEY",
            "--admin-key",
            admin_key.as_str(),
        ])
        .write_stdin("ordinary-secret")
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "set secret: ACP_STACK_SESSION_KEY",
        ));
}

#[test]
fn secrets_delete_requires_admin_key() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let (_, admin_key) = run_init_with_home(tempdir.path());

    acps_command(tempdir.path())
        .args([
            "secrets",
            "set",
            "TEMP_VALUE",
            "--admin-key",
            admin_key.as_str(),
        ])
        .write_stdin("abc")
        .assert()
        .success();

    acps_command(tempdir.path())
        .args(["secrets", "delete", "TEMP_VALUE"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--admin-key"));
}

#[test]
fn secrets_list_shows_session_and_admin_names_only_after_init() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    acps_command(tempdir.path())
        .args(["secrets", "list"])
        .assert()
        .success()
        .stdout(predicates::str::contains("ACP_STACK_ADMIN_KEY").not())
        .stdout(predicates::str::contains("ACP_STACK_SESSION_KEY").not())
        .stdout(predicates::str::contains("acps_").not());
}

#[test]
fn secrets_commands_format_json_never_print_values() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let (_, admin_key) = run_init_with_home(tempdir.path());

    let set_output = acps_command(tempdir.path())
        .args([
            "secrets",
            "set",
            "OPENCODE_API_KEY",
            "--format",
            "json",
            "--admin-key",
            admin_key.as_str(),
        ])
        .write_stdin("super-secret-value\n")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let set_body: Value = serde_json::from_slice(&set_output).expect("set json parses");
    assert_eq!(set_body["action"], "set");
    assert_eq!(set_body["name"], "OPENCODE_API_KEY");
    assert!(!String::from_utf8_lossy(&set_output).contains("super-secret-value"));

    let list_output = acps_command(tempdir.path())
        .args(["secrets", "list", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let list_body: Value = serde_json::from_slice(&list_output).expect("list json parses");
    let names = list_body["secrets"]
        .as_array()
        .expect("secrets should be an array");
    assert!(names.iter().any(|name| name == "OPENCODE_API_KEY"));
    assert!(!String::from_utf8_lossy(&list_output).contains("super-secret-value"));

    let delete_output = acps_command(tempdir.path())
        .args([
            "secrets",
            "delete",
            "OPENCODE_API_KEY",
            "--format",
            "json",
            "--admin-key",
            admin_key.as_str(),
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let delete_body: Value = serde_json::from_slice(&delete_output).expect("delete json parses");
    assert_eq!(delete_body["action"], "delete");
    assert_eq!(delete_body["name"], "OPENCODE_API_KEY");
}

#[test]
fn secrets_set_reads_value_from_stdin() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let (_, admin_key) = run_init_with_home(tempdir.path());

    acps_command(tempdir.path())
        .args([
            "secrets",
            "set",
            "OPENCODE_API_KEY",
            "--admin-key",
            admin_key.as_str(),
        ])
        .write_stdin("super-secret-value\n")
        .assert()
        .success()
        .stdout(predicates::str::contains("set secret: OPENCODE_API_KEY"));

    acps_command(tempdir.path())
        .args(["secrets", "list"])
        .assert()
        .success()
        .stdout(predicates::str::contains("OPENCODE_API_KEY"));
}

#[test]
fn secrets_set_accepts_name_and_value_flags() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let (_, admin_key) = run_init_with_home(tempdir.path());

    let output = acps_command(tempdir.path())
        .args([
            "secrets",
            "set",
            "--name",
            "MOONSHOT_API_KEY",
            "--value",
            "super-secret-value",
            "--admin-key",
            admin_key.as_str(),
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("set secret: MOONSHOT_API_KEY"))
        .get_output()
        .stdout
        .clone();
    assert!(!String::from_utf8_lossy(&output).contains("super-secret-value"));

    acps_command(tempdir.path())
        .args(["secrets", "list"])
        .assert()
        .success()
        .stdout(predicates::str::contains("MOONSHOT_API_KEY"));
}

#[test]
fn secrets_set_rejects_positional_name_with_name_flag() {
    let tempdir = tempfile::tempdir().expect("tempdir");

    acps_command(tempdir.path())
        .args([
            "secrets",
            "set",
            "OPENCODE_API_KEY",
            "--name",
            "MOONSHOT_API_KEY",
            "--value",
            "super-secret-value",
            "--admin-key",
            "unused",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "pass the secret name either positionally or with --name, not both",
        ));
}

#[test]
fn secrets_delete_removes_named_secret() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let (_, admin_key) = run_init_with_home(tempdir.path());

    acps_command(tempdir.path())
        .args([
            "secrets",
            "set",
            "TEMP_VALUE",
            "--admin-key",
            admin_key.as_str(),
        ])
        .write_stdin("abc")
        .assert()
        .success();

    acps_command(tempdir.path())
        .args([
            "secrets",
            "delete",
            "TEMP_VALUE",
            "--admin-key",
            admin_key.as_str(),
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("deleted secret: TEMP_VALUE"));

    acps_command(tempdir.path())
        .args([
            "secrets",
            "delete",
            "TEMP_VALUE",
            "--admin-key",
            admin_key.as_str(),
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("was not found"));
}

#[test]
fn auth_regenerate_session_key_requires_admin_key() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    acps_command(tempdir.path())
        .args(["auth", "regenerate-session-key"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--admin-key"));
}

#[test]
fn reset_without_yes_lists_targets_and_keeps_files() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    acps_command(tempdir.path())
        .arg("reset")
        .assert()
        .failure()
        .stdout(predicates::str::contains("acps reset would delete:"))
        .stdout(predicates::str::contains("acps-config.toml"))
        .stdout(predicates::str::contains("state.sqlite"))
        .stdout(predicates::str::contains("age.key"))
        .stdout(predicates::str::contains("secrets.age"))
        .stdout(predicates::str::contains("re-run with --yes"));

    assert!(
        tempdir
            .path()
            .join(".config/acp-stack/acps-config.toml")
            .exists(),
        "dry-run must NOT remove files",
    );
}

#[test]
fn reset_dry_run_does_not_write_cli_error_event() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    acps_command(tempdir.path()).arg("reset").assert().failure();

    // A dry run must not touch the filesystem, and a `cli.error` row would
    // write to state.sqlite.
    acps_command(tempdir.path())
        .args(["logs", "query", "--level", "error"])
        .assert()
        .success()
        .stdout("");
}

#[test]
fn reset_with_yes_wipes_config_state_age_key_and_secret_store() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    acps_command(tempdir.path())
        .args(["reset", "--yes"])
        .assert()
        .success()
        .stdout(predicates::str::contains("reset acp-stack"));

    assert!(
        !tempdir
            .path()
            .join(".config/acp-stack/acps-config.toml")
            .exists()
    );
    assert!(!tempdir.path().join(".config/acp-stack/age.key").exists());
    assert!(
        !tempdir
            .path()
            .join(".local/share/acp-stack/state.sqlite")
            .exists()
    );
    assert!(
        !tempdir
            .path()
            .join(".local/share/acp-stack/secrets.age")
            .exists()
    );

    acps_command(tempdir.path())
        .args(["reset", "--yes"])
        .assert()
        .success();

    let init_after = acps_command(tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(init_after).expect("utf8");
    assert!(stdout.contains("admin key: acps_"));
}
