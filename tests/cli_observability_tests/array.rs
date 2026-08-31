use std::fs;

use crate::common::cli::*;

#[test]
fn array_add_uses_canonical_agent_id_as_target() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config_path = config_dir.join("acps-config.toml");
    fs::write(&config_path, VALID_CONFIG).expect("config should be written");

    acps_command(tempdir.path())
        .args(["array", "add", "codex"])
        .assert()
        .success()
        .stdout(predicates::str::contains("array target added: codex"));

    let config: toml::Value = toml::from_str(
        &fs::read_to_string(config_path).expect("updated config should be readable"),
    )
    .expect("config should parse");
    assert_eq!(config["array"]["primary_target"].as_str(), Some("opencode"));
    assert_eq!(config["array"]["targets"][1]["id"].as_str(), Some("codex"));
    assert_eq!(
        config["array"]["targets"][1]["agent"]["id"].as_str(),
        Some("codex")
    );
}

#[test]
fn array_add_rejects_noncanonical_agent_alias() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");

    acps_command(tempdir.path())
        .args(["array", "add", "claude-code"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("claude-code"));
}

#[test]
fn array_set_supports_target_custom_provider() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config_path = config_dir.join("acps-config.toml");
    fs::write(&config_path, VALID_CONFIG).expect("config should be written");

    acps_command(tempdir.path())
        .args([
            "array",
            "set",
            "--target",
            "opencode",
            "--custom-provider",
            "--provider",
            "custom-openai",
            "--provider-name",
            "Custom OpenAI",
            "--base-url",
            "https://llm.example.test/v1",
            "--model",
            "custom/model",
            "--model-name",
            "Custom Model",
            "--context",
            "1234",
            "--output-max-tokens",
            "567",
            "--api-key-ref",
            "CUSTOM_OPENAI_KEY",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("array target set: opencode"))
        .stdout(predicates::str::contains("provider: custom-openai"));

    let config: toml::Value = toml::from_str(
        &fs::read_to_string(config_path).expect("updated config should be readable"),
    )
    .expect("config should parse");
    let provider = &primary_array_agent_value(&config)["provider"];
    assert_eq!(provider["id"].as_str(), Some("custom-openai"));
    assert_eq!(provider["model"].as_str(), Some("custom/model"));
    assert_eq!(provider["api_key_ref"].as_str(), Some("CUSTOM_OPENAI_KEY"));
    assert_eq!(
        provider["custom"]["base_url"].as_str(),
        Some("https://llm.example.test/v1")
    );
    assert_eq!(provider["custom"]["context"].as_integer(), Some(1234));
    assert_eq!(
        provider["custom"]["output_max_tokens"].as_integer(),
        Some(567)
    );
}

#[test]
fn array_set_validates_effort_after_the_model_in_one_command() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config_path = config_dir.join("acps-config.toml");
    fs::write(&config_path, VALID_CONFIG).expect("config should be written");
    acps_command(tempdir.path())
        .args(["array", "add", "codex"])
        .assert()
        .success();
    let options_path = write_acp_config_options_with_efforts(
        tempdir.path(),
        &["gpt-5.5", "gpt-5.4"],
        &[],
        &["low", "medium", "high"],
    );

    acps_command(tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args([
            "array", "set", "--target", "codex", "--model", "gpt-5.4", "--effort", "high",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("effort: high"));

    let config: toml::Value = toml::from_str(
        &fs::read_to_string(&config_path).expect("updated config should be readable"),
    )
    .expect("config should parse");
    let codex = &config["array"]["targets"][1]["agent"];
    assert_eq!(codex["model"].as_str(), Some("gpt-5.4"));
    assert_eq!(codex["effort"].as_str(), Some("high"));

    // A rejected effort leaves the whole command unapplied, model included.
    acps_command(tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args([
            "array", "set", "--target", "codex", "--model", "gpt-5.5", "--effort", "bogus",
        ])
        .assert()
        .failure();
    let config: toml::Value = toml::from_str(
        &fs::read_to_string(&config_path).expect("updated config should be readable"),
    )
    .expect("config should parse");
    let codex = &config["array"]["targets"][1]["agent"];
    assert_eq!(codex["model"].as_str(), Some("gpt-5.4"));
    assert_eq!(codex["effort"].as_str(), Some("high"));
}

#[test]
fn agent_default_set_updates_primary_target_only() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config_path = config_dir.join("acps-config.toml");
    fs::write(&config_path, VALID_CONFIG).expect("config should be written");

    acps_command(tempdir.path())
        .args(["array", "add", "codex"])
        .assert()
        .success();
    acps_command(tempdir.path())
        .args(["agent", "default", "set", "codex"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent default: codex"));

    let config: toml::Value = toml::from_str(
        &fs::read_to_string(config_path).expect("updated config should be readable"),
    )
    .expect("config should parse");
    assert_eq!(config["array"]["primary_target"].as_str(), Some("codex"));
    assert_eq!(
        config["array"]["targets"][0]["id"].as_str(),
        Some("opencode")
    );
    assert_eq!(config["array"]["targets"][1]["id"].as_str(), Some("codex"));
}

#[test]
fn array_on_and_off_toggle_enabled_flag() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config_path = config_dir.join("acps-config.toml");
    fs::write(&config_path, VALID_CONFIG).expect("config should be written");

    acps_command(tempdir.path())
        .args(["array", "on"])
        .assert()
        .success()
        .stdout(predicates::str::contains("array: on"));
    let after_on: toml::Value =
        toml::from_str(&fs::read_to_string(&config_path).expect("config should be readable"))
            .expect("config should parse");
    assert_eq!(after_on["array"]["enabled"].as_bool(), Some(true));

    acps_command(tempdir.path())
        .args(["array", "off"])
        .assert()
        .success()
        .stdout(predicates::str::contains("array: off"));
    let after_off: toml::Value =
        toml::from_str(&fs::read_to_string(&config_path).expect("config should be readable"))
            .expect("config should parse");
    assert_eq!(after_off["array"]["enabled"].as_bool(), Some(false));
}

#[test]
fn array_start_rejects_non_default_target_when_array_is_off() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config_path = config_dir.join("acps-config.toml");
    fs::write(&config_path, VALID_CONFIG).expect("config should be written");

    acps_command(tempdir.path())
        .args(["array", "add", "codex"])
        .assert()
        .success();
    acps_command(tempdir.path())
        .args(["array", "start", "--target", "codex"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("Array mode is off"));
}
