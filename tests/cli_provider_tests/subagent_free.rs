use serde_json::{Value, json};
use std::fs;

use crate::common::cli::*;

#[test]
fn subagent_free_infers_openrouter_from_main_provider() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = format!(
        "{}\n\n[agent.provider]\nid = \"openrouter\"\nmodel = \"openrouter/deepseek/deepseek-v4-flash\"\napi_key_ref = \"OPENROUTER_API_KEY\"\n",
        VALID_CONFIG.replace(
            r#"env = ["OPENCODE_API_KEY"]"#,
            r#"env = ["OPENROUTER_API_KEY"]"#,
        )
    );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["subagent", "free"])
        .assert()
        .success()
        .stdout(predicates::str::contains("provider: openrouter"))
        .stdout(predicates::str::contains("model: openrouter/free"));

    let config = fs::read_to_string(config_dir.join("acps-config.toml"))
        .expect("updated config should be readable");
    assert!(config.contains("[array.targets.agent.subagent.provider]"));
    assert!(config.contains(r#"id = "openrouter""#));
    assert!(config.contains(r#"model = "openrouter/free""#));
    assert!(config.contains(r#"api_key_ref = "OPENROUTER_API_KEY""#));

    let opencode_path = tempdir
        .path()
        .join(".config")
        .join("opencode")
        .join("opencode.json");
    let opencode: Value = serde_json::from_str(
        &fs::read_to_string(opencode_path).expect("opencode config should be readable"),
    )
    .expect("opencode config should parse");
    assert_eq!(opencode["model"], "openrouter/deepseek/deepseek-v4-flash");
    assert_eq!(opencode["small_model"], "openrouter/free");
    assert_eq!(opencode["enabled_providers"], json!(["openrouter"]));
}

#[test]
fn subagent_free_can_use_opencode_big_pickle() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["subagent", "free"])
        .assert()
        .success()
        .stdout(predicates::str::contains("provider: opencode"))
        .stdout(predicates::str::contains("model: opencode/big-pickle"));

    let config = fs::read_to_string(config_dir.join("acps-config.toml"))
        .expect("updated config should be readable");
    assert!(config.contains(r#"id = "opencode""#));
    assert!(config.contains(r#"model = "opencode/big-pickle""#));
    assert!(config.contains(r#"api_key_ref = "OPENCODE_API_KEY""#));
}

#[test]
fn subagent_free_prefers_current_opencode_provider_over_stale_openrouter_env() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = format!(
        "{}\n\n[agent.provider]\nid = \"opencode-go\"\nmodel = \"opencode-go/deepseek-v4-flash\"\napi_key_ref = \"OPENCODE_API_KEY\"\n",
        VALID_CONFIG.replace(
            r#"env = ["OPENCODE_API_KEY"]"#,
            r#"env = ["OPENCODE_API_KEY", "OPENROUTER_API_KEY"]"#,
        )
    );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["subagent", "free"])
        .assert()
        .success()
        .stdout(predicates::str::contains("provider: opencode"))
        .stdout(predicates::str::contains("model: opencode/big-pickle"));

    let config = fs::read_to_string(config_dir.join("acps-config.toml"))
        .expect("updated config should be readable");
    assert!(config.contains(r#"id = "opencode""#));
    assert!(config.contains(r#"model = "opencode/big-pickle""#));
}

#[test]
fn subagent_free_rejects_provider_without_free_support() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = format!(
        "{}\n\n[agent.provider]\nid = \"openai\"\nmodel = \"openai/gpt-5.5\"\napi_key_ref = \"OPENAI_API_KEY\"\n",
        VALID_CONFIG.replace(
            r#"env = ["OPENCODE_API_KEY"]"#,
            r#"env = ["OPENAI_API_KEY"]"#,
        )
    );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["subagent", "free"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "Current provider does not support free.",
        ));
}

#[test]
fn subagent_free_rejects_unsupported_main_provider_despite_stale_free_env() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = format!(
        "{}\n\n[agent.provider]\nid = \"openai\"\nmodel = \"openai/gpt-5.5\"\napi_key_ref = \"OPENAI_API_KEY\"\n",
        VALID_CONFIG.replace(
            r#"env = ["OPENCODE_API_KEY"]"#,
            r#"env = ["OPENAI_API_KEY", "OPENCODE_API_KEY", "OPENROUTER_API_KEY"]"#,
        )
    );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["subagent", "free"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "Current provider does not support free.",
        ));
}

#[test]
fn subagent_free_resolves_opencode_go_alias_with_custom_main_api_key_ref() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = format!(
        "{}\n\n[agent.provider]\nid = \"opencode-go\"\nmodel = \"opencode-go/deepseek-v4-flash\"\napi_key_ref = \"MY_OPENCODE_KEY\"\n",
        VALID_CONFIG.replace(
            r#"env = ["OPENCODE_API_KEY"]"#,
            r#"env = ["MY_OPENCODE_KEY"]"#,
        )
    );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["subagent", "free"])
        .assert()
        .success()
        .stdout(predicates::str::contains("provider: opencode"))
        .stdout(predicates::str::contains("model: opencode/big-pickle"));

    let config = fs::read_to_string(config_dir.join("acps-config.toml"))
        .expect("updated config should be readable");
    assert!(config.contains(r#"api_key_ref = "MY_OPENCODE_KEY""#));
    assert!(!config.contains("OPENCODE_API_KEY"));
}

#[test]
fn subagent_free_preserves_custom_main_api_key_ref_when_provider_matches() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = format!(
        "{}\n\n[agent.provider]\nid = \"openrouter\"\nmodel = \"openrouter/some-paid-model\"\napi_key_ref = \"MY_OPENROUTER_KEY\"\n",
        VALID_CONFIG.replace(
            r#"env = ["OPENCODE_API_KEY"]"#,
            r#"env = ["MY_OPENROUTER_KEY"]"#,
        )
    );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["subagent", "free"])
        .assert()
        .success()
        .stdout(predicates::str::contains("provider: openrouter"))
        .stdout(predicates::str::contains("model: openrouter/free"));

    let config = fs::read_to_string(config_dir.join("acps-config.toml"))
        .expect("updated config should be readable");
    assert!(config.contains(r#"api_key_ref = "MY_OPENROUTER_KEY""#));
    assert!(!config.contains("OPENROUTER_API_KEY"));
}
