use std::fs;

use serde_json::Value;

use crate::common::cli::*;
use crate::support::write_workspace_init_config;

#[test]
fn init_explicit_model_validates_against_acp_advertised_values() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    write_workspace_init_config(tempdir.path());
    seed_init_secrets(tempdir.path(), &[("OPENAI_API_KEY", "test-openai-key")]);
    let options_path = write_acp_config_options(tempdir.path(), &["openai/gpt-5.5"], &[]);

    acps_with_empty_path(tempdir.path())
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args([
            "init",
            "--agent",
            "opencode",
            "--provider",
            "openai",
            "--api-key-ref",
            "OPENAI_API_KEY",
            "--model",
            "openai/gpt-5.5",
        ])
        .assert()
        .success();

    let config = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    assert!(config.contains(r#"model = "openai/gpt-5.5""#));
    assert!(!config.contains("[array.targets.agent.subagent"));

    let opencode_path = tempdir
        .path()
        .join(".config")
        .join("opencode")
        .join("opencode.json");
    let opencode: Value = serde_json::from_str(
        &fs::read_to_string(opencode_path).expect("opencode config should be readable"),
    )
    .expect("opencode config should parse");
    assert_eq!(opencode["model"], "openai/gpt-5.5");
    assert_eq!(opencode["small_model"], "openai/gpt-5.5");
}

#[test]
fn init_explicit_model_accepts_provider_model_shorthand() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    write_workspace_init_config(tempdir.path());
    seed_init_secrets(
        tempdir.path(),
        &[("OPENROUTER_API_KEY", "test-openrouter-key")],
    );
    let options_path = write_acp_config_options(
        tempdir.path(),
        &["openrouter/deepseek/deepseek-v4-flash"],
        &[],
    );

    acps_with_empty_path(tempdir.path())
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args([
            "init",
            "--agent",
            "opencode",
            "--provider",
            "openrouter",
            "--api-key-ref",
            "OPENROUTER_API_KEY",
            "--model",
            "deepseek/deepseek-v4-flash",
        ])
        .assert()
        .success();

    let config = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    assert!(config.contains(r#"model = "openrouter/deepseek/deepseek-v4-flash""#));

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
}

#[test]
fn init_explicit_model_shorthand_prefers_selected_provider() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    write_workspace_init_config(tempdir.path());
    seed_init_secrets(
        tempdir.path(),
        &[("OPENROUTER_API_KEY", "test-openrouter-key")],
    );
    let options_path = write_acp_config_options(
        tempdir.path(),
        &[
            "deepseek/deepseek-v4-flash",
            "openrouter/deepseek/deepseek-v4-flash",
        ],
        &[],
    );

    acps_with_empty_path(tempdir.path())
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args([
            "init",
            "--agent",
            "opencode",
            "--provider",
            "openrouter",
            "--api-key-ref",
            "OPENROUTER_API_KEY",
            "--model",
            "deepseek/deepseek-v4-flash",
        ])
        .assert()
        .success();

    let config = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    assert!(config.contains(r#"model = "openrouter/deepseek/deepseek-v4-flash""#));
    assert!(!config.contains(r#"model = "deepseek/deepseek-v4-flash""#));
}

#[test]
fn init_rejected_model_restores_prior_headless_config() {
    // Pre-write a prior opencode headless config, then run init with
    // an unadvertised --model. The init must reject the value AND
    // leave the prior headless config exactly as it was (rollback
    // guarantee).
    let tempdir = tempfile::tempdir().expect("tempdir");
    write_workspace_init_config(tempdir.path());
    seed_init_secrets(tempdir.path(), &[("OPENAI_API_KEY", "test")]);
    let prior_opencode_path = tempdir
        .path()
        .join(".config")
        .join("opencode")
        .join("opencode.json");
    fs::create_dir_all(prior_opencode_path.parent().expect("parent")).expect("opencode dir");
    let prior_bytes = b"{\"prior\":\"sentinel\"}";
    fs::write(&prior_opencode_path, prior_bytes).expect("prior opencode config");

    let options_path = write_acp_config_options(tempdir.path(), &["openai/gpt-5.5"], &[]);

    acps_with_empty_path(tempdir.path())
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args([
            "init",
            "--agent",
            "opencode",
            "--provider",
            "openai",
            "--api-key-ref",
            "OPENAI_API_KEY",
            "--model",
            "definitely-not-advertised",
        ])
        .assert()
        .failure();

    let after = fs::read(&prior_opencode_path).expect("opencode config readable after rejection");
    assert_eq!(
        after, prior_bytes,
        "rejected --model must restore prior opencode headless config exactly",
    );
}

#[test]
fn init_explicit_model_rejects_value_not_in_advertised_list() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    write_workspace_init_config(tempdir.path());
    seed_init_secrets(tempdir.path(), &[("OPENAI_API_KEY", "test-openai-key")]);
    let options_path = write_acp_config_options(tempdir.path(), &["openai/gpt-5.5"], &[]);

    acps_with_empty_path(tempdir.path())
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args([
            "init",
            "--agent",
            "opencode",
            "--provider",
            "openai",
            "--api-key-ref",
            "OPENAI_API_KEY",
            "--model",
            "made-up-model",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "agent did not advertise `made-up-model` as an available `model`",
        ))
        .stderr(predicates::str::contains(
            "advertised models: [openai/gpt-5.5]",
        ));

    let config = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    assert!(!config.contains("made-up-model"));
}

#[test]
fn init_noninteractive_missing_model_prints_advertised_values_without_mutating_config() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    write_workspace_init_config(tempdir.path());
    seed_init_secrets(tempdir.path(), &[("OPENAI_API_KEY", "test-openai-key")]);
    let options_path =
        write_acp_config_options(tempdir.path(), &["openai/gpt-5.5", "openai/o4-mini"], &[]);

    acps_with_empty_path(tempdir.path())
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args([
            "init",
            "--agent",
            "opencode",
            "--provider",
            "openai",
            "--api-key-ref",
            "OPENAI_API_KEY",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("advertised models for OpenCode:"))
        .stdout(predicates::str::contains("openai/gpt-5.5"))
        .stdout(predicates::str::contains("openai/o4-mini"))
        .stdout(predicates::str::contains(
            "rerun with `acps init --model <value>` to write a model into config",
        ));

    let config = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    // L87 contract: provider was set this run, but the no-flag path
    // must not write a model into config.
    assert!(config.contains(r#"id = "openai""#));
    assert!(!config.contains(r#"model = "openai/"#));
}
