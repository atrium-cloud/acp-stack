use std::fs;

use acp_stack::config::load_config_from_str;
use acp_stack::state::{InitStepRecord, StateStore, default_state_path};
use predicates::prelude::PredicateBooleanExt as _;

use crate::common::cli::*;
use crate::support::write_workspace_init_config;

#[test]
fn init_rejects_effort_flag_for_agents_without_set_effort() {
    // amp/hermes leave set_effort false, so `--effort` must fail the
    // capability check rather than being silently dropped.
    for agent in ["amp", "hermes"] {
        let tempdir = tempfile::tempdir().expect("tempdir");

        acps_command(tempdir.path())
            .args(["init", "--agent", agent, "--effort", "high"])
            .assert()
            .failure()
            .stderr(predicates::str::contains(
                "does not support reasoning-effort configuration through `acps init`",
            ));
    }
}

#[test]
fn init_rejects_effort_flag_for_custom_agents() {
    let tempdir = tempfile::tempdir().expect("tempdir");

    acps_command(tempdir.path())
        .args([
            "init",
            "--custom-agent-id",
            "bespoke",
            "--custom-agent-command",
            "bespoke",
            "--custom-agent-install",
            "true",
            "--effort",
            "high",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "cannot be used with '--effort <EFFORT>'",
        ));
}

/// The `provider_configure` step payload, which records what each lane did.
fn provider_configure_step(home: &std::path::Path) -> InitStepRecord {
    let store = StateStore::open(default_state_path(home)).expect("state store");
    let run = store
        .latest_init_run()
        .expect("latest init run")
        .expect("init run exists");
    let steps = store.query_init_steps(&run.id).expect("init steps");
    steps
        .iter()
        .find(|step| step.kind == "provider_configure")
        .unwrap_or_else(|| panic!("provider_configure recorded: {steps:?}"))
        .clone()
}

fn provider_configure_payload(home: &std::path::Path) -> String {
    provider_configure_step(home).payload_json
}

#[test]
fn init_explicit_effort_writes_agent_effort() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    write_workspace_init_config(tempdir.path());
    seed_init_secrets(tempdir.path(), &[("OPENAI_API_KEY", "test-openai-key")]);
    let options_path = write_acp_config_options_with_efforts(
        tempdir.path(),
        &["gpt-5.5"],
        &["read-only", "auto"],
        &["low", "medium", "high"],
    );

    acps_with_empty_path(tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args([
            "init",
            "--agent",
            "codex",
            "--provider",
            "openai",
            "--api-key-ref",
            "OPENAI_API_KEY",
            "--model",
            "gpt-5.5",
            "--effort",
            "high",
        ])
        .assert()
        .success();

    let config_text = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    let config = load_config_from_str(&config_text).expect("canonical config parses");
    assert_eq!(config.agent.effort.as_deref(), Some("high"));
    let payload = provider_configure_payload(tempdir.path());
    assert!(
        payload.contains(r#""effort_action":"Set""#),
        "payload {payload}"
    );
}

#[test]
fn init_codex_openrouter_validates_effort_against_the_catalog_and_pins_it() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    write_workspace_init_config(tempdir.path());
    seed_init_secrets(
        tempdir.path(),
        &[("OPENROUTER_API_KEY", "test-openrouter-key")],
    );
    // The adapter advertises no effort option for an OpenRouter model.
    let options_path =
        write_acp_config_options_with_efforts(tempdir.path(), &["gpt-5.5"], &[], &[]);
    let models_base = crate::common::agent::spawn_provider_models_server(serde_json::json!({
        "data": [{
            "id": "deepseek/deepseek-v4-flash",
            "reasoning": { "mandatory": false, "supported_efforts": ["max", "xhigh", "high"] }
        }]
    }));

    acps_with_empty_path(tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .env("ACP_STACK_PROVIDER_MODELS_BASE", &models_base)
        .args([
            "init",
            "--agent",
            "codex",
            "--provider",
            "openrouter",
            "--api-key-ref",
            "OPENROUTER_API_KEY",
            "--model",
            "deepseek/deepseek-v4-flash",
            "--effort",
            "max",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("catalog efforts: [xhigh, high]"));

    acps_with_empty_path(tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .env("ACP_STACK_PROVIDER_MODELS_BASE", &models_base)
        .args([
            "init",
            "--agent",
            "codex",
            "--provider",
            "openrouter",
            "--api-key-ref",
            "OPENROUTER_API_KEY",
            "--model",
            "deepseek/deepseek-v4-flash",
            "--effort",
            "high",
        ])
        .assert()
        .success();

    let config_text = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    let config = load_config_from_str(&config_text).expect("canonical config parses");
    assert_eq!(config.agent.effort.as_deref(), Some("high"));
    let codex: toml::Value = toml::from_str(
        &fs::read_to_string(tempdir.path().join(".codex").join("config.toml"))
            .expect("codex config should be readable"),
    )
    .expect("codex config should parse");
    assert_eq!(codex["model_reasoning_effort"].as_str(), Some("high"));
    assert_eq!(
        codex["model_supports_reasoning_summaries"].as_bool(),
        Some(true)
    );
}

#[test]
fn init_explicit_model_and_effort_validate_against_one_post_model_advertisement() {
    // Explicit `--model` plus `--effort` writes the model before the single
    // discovery spawn, so the effort validates against that model's advertisement.
    // Re-discovery after an interactive model pick is covered in-crate by
    // `effort_prompt_reads_the_advertisement_of_the_model_just_picked`.
    let tempdir = tempfile::tempdir().expect("tempdir");
    write_workspace_init_config(tempdir.path());
    seed_init_secrets(
        tempdir.path(),
        &[("OPENROUTER_API_KEY", "test-openrouter-key")],
    );
    let options_path = write_acp_config_options_with_efforts(
        tempdir.path(),
        &["openrouter/model-a", "openrouter/model-b"],
        &[],
        &["high"],
    );

    acps_with_empty_path(tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .env("ACP_STACK_PROVIDER_MODELS_BASE", "http://127.0.0.1:1")
        .args([
            "init",
            "--agent",
            "opencode",
            "--provider",
            "openrouter",
            "--api-key-ref",
            "OPENROUTER_API_KEY",
            "--model",
            "model-b",
            "--effort",
            "high",
        ])
        .assert()
        .success();

    let config_text = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    let config = load_config_from_str(&config_text).expect("canonical config parses");
    assert_eq!(
        config
            .agent
            .provider
            .as_ref()
            .and_then(|provider| provider.model.as_deref()),
        Some("openrouter/model-b")
    );
    assert_eq!(config.agent.effort.as_deref(), Some("high"));
}

#[test]
fn init_rejects_effort_not_advertised_by_the_agent() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    write_workspace_init_config(tempdir.path());
    seed_init_secrets(tempdir.path(), &[("OPENAI_API_KEY", "test-openai-key")]);
    let options_path = write_acp_config_options_with_efforts(
        tempdir.path(),
        &["gpt-5.5"],
        &[],
        &["low", "medium", "high"],
    );

    acps_with_empty_path(tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args([
            "init",
            "--agent",
            "codex",
            "--provider",
            "openai",
            "--api-key-ref",
            "OPENAI_API_KEY",
            "--effort",
            "bogus",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "advertised efforts: [high, low, medium]",
        ));

    let config_text = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    let config = load_config_from_str(&config_text).expect("canonical config parses");
    assert!(config.agent.effort.is_none());
}

#[test]
fn init_without_effort_flag_never_enters_the_effort_lane() {
    // An unattended run for an effort-capable agent must not enter the effort
    // lane; the step payload records the skip.
    let tempdir = tempfile::tempdir().expect("tempdir");
    write_workspace_init_config(tempdir.path());
    seed_init_secrets(tempdir.path(), &[("OPENAI_API_KEY", "test-openai-key")]);

    acps_with_empty_path(tempdir.path())
        .args([
            "init",
            "--agent",
            "codex",
            "--provider",
            "openai",
            "--api-key-ref",
            "OPENAI_API_KEY",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("advertised models"))
        .stdout(predicates::str::contains("effort").not());

    let config_text = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    let config = load_config_from_str(&config_text).expect("canonical config parses");
    assert!(config.agent.effort.is_none());
    assert!(provider_configure_payload(tempdir.path()).contains(r#""effort_action":"Skipped""#));
}
