#[cfg(not(feature = "dev-tools"))]
use assert_cmd::Command;
#[cfg(not(feature = "dev-tools"))]
use predicates::prelude::*;

#[cfg(not(feature = "dev-tools"))]
#[test]
fn production_help_hides_dev_command() {
    let mut cmd = Command::cargo_bin("acps").expect("acps binary");
    cmd.arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains(" dev ").not())
        .stdout(predicate::str::contains("Run development-only workflows").not());
}

#[cfg(not(feature = "dev-tools"))]
#[test]
fn production_dev_command_is_unknown() {
    let mut cmd = Command::cargo_bin("acps").expect("acps binary");
    cmd.arg("dev")
        .assert()
        .failure()
        .stderr(predicate::str::contains("unrecognized subcommand 'dev'"));
}

#[cfg(not(feature = "test-fixtures"))]
#[test]
fn production_build_does_not_expose_placebo_binary() {
    let manifest = std::fs::read_to_string("Cargo.toml").expect("read manifest");
    let value: toml::Value = toml::from_str(&manifest).expect("parse manifest");
    let bins = value
        .get("bin")
        .and_then(toml::Value::as_array)
        .expect("bin array");
    let placebo = bins
        .iter()
        .find(|bin| bin.get("name").and_then(toml::Value::as_str) == Some("placebo-agent"))
        .expect("placebo binary target");
    assert_eq!(
        placebo.get("required-features"),
        Some(&toml::Value::Array(vec![toml::Value::String(
            "test-fixtures".to_owned()
        )]))
    );
}
