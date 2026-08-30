use std::fs;

use serde_json::json;

use crate::common::agent::spawn_provider_models_server;
use crate::common::cli::*;

fn codex_openrouter_home() -> (tempfile::TempDir, String) {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), codex_config())
        .expect("config should be written");
    seed_provider_credential(tempdir.path(), "openrouter", &["OPENROUTER_API_KEY"]);
    let models_base = spawn_provider_models_server(json!({
        "data": [
            {
                "id": "deepseek/deepseek-v4-flash",
                "reasoning": { "mandatory": false, "supported_efforts": ["max", "xhigh", "high"] }
            },
            { "id": "meta-llama/llama-3.1-8b-instruct" }
        ]
    }));
    acps_command(tempdir.path())
        .env("ACP_STACK_PROVIDER_MODELS_BASE", &models_base)
        .args([
            "agent",
            "provider",
            "use",
            "openrouter",
            "--model",
            "deepseek/deepseek-v4-flash",
        ])
        .assert()
        .success();
    (tempdir, models_base)
}

fn codex_toml(home: &std::path::Path) -> toml::Value {
    toml::from_str(
        &fs::read_to_string(home.join(".codex").join("config.toml"))
            .expect("codex config should be readable"),
    )
    .expect("codex config should parse")
}

#[test]
fn agent_set_codex_openrouter_validates_effort_against_the_catalog_and_pins_it() {
    let (tempdir, models_base) = codex_openrouter_home();

    // No ACP fixture: the catalog, not a provisional session, is the source here.
    acps_command(tempdir.path())
        .env("ACP_STACK_PROVIDER_MODELS_BASE", &models_base)
        .args(["agent", "set", "--effort", "high"])
        .assert()
        .success()
        .stdout(predicates::str::contains("effort: high"));

    let config = fs::read_to_string(
        tempdir
            .path()
            .join(".config/acp-stack")
            .join("acps-config.toml"),
    )
    .expect("config should be readable");
    assert!(config.contains(r#"effort = "high""#));
    let codex = codex_toml(tempdir.path());
    assert_eq!(codex["model_reasoning_effort"].as_str(), Some("high"));
    assert_eq!(
        codex["model_supports_reasoning_summaries"].as_bool(),
        Some(true)
    );
}

#[test]
fn agent_set_codex_openrouter_rejects_effort_outside_the_catalog() {
    let (tempdir, models_base) = codex_openrouter_home();

    // `max` is in the catalog but not parseable by codex; `bogus` is in neither.
    for value in ["max", "bogus"] {
        acps_command(tempdir.path())
            .env("ACP_STACK_PROVIDER_MODELS_BASE", &models_base)
            .args(["agent", "set", "--effort", value])
            .assert()
            .failure()
            .stderr(predicates::str::contains("catalog efforts: [xhigh, high]"));
    }

    let config = fs::read_to_string(
        tempdir
            .path()
            .join(".config/acp-stack")
            .join("acps-config.toml"),
    )
    .expect("config should be readable");
    assert!(!config.contains("effort = "));
    let codex = codex_toml(tempdir.path());
    assert!(codex.get("model_reasoning_effort").is_none(), "{codex}");
}

#[test]
fn agent_set_codex_openrouter_rejects_effort_for_a_model_without_levels() {
    let (tempdir, models_base) = codex_openrouter_home();
    acps_command(tempdir.path())
        .env("ACP_STACK_PROVIDER_MODELS_BASE", &models_base)
        .args([
            "agent",
            "provider",
            "use",
            "openrouter",
            "--model",
            "meta-llama/llama-3.1-8b-instruct",
        ])
        .assert()
        .success();

    acps_command(tempdir.path())
        .env("ACP_STACK_PROVIDER_MODELS_BASE", &models_base)
        .args(["agent", "set", "--effort", "high"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "reports no reasoning-effort values for `meta-llama/llama-3.1-8b-instruct`",
        ));
}

#[test]
fn agent_set_codex_accepts_effort_only() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), codex_config())
        .expect("config should be written");
    let options_path =
        write_acp_config_options_with_efforts(tempdir.path(), &[], &[], &["low", "medium", "high"]);

    acps_command(tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args(["agent", "set", "--effort", "high"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent: codex"))
        .stdout(predicates::str::contains("effort: high"));

    let config =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config should be readable");
    assert!(config.contains(r#"effort = "high""#));
}

#[test]
fn agent_set_codex_rejects_unadvertised_effort() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), codex_config())
        .expect("config should be written");
    let options_path =
        write_acp_config_options_with_efforts(tempdir.path(), &[], &[], &["low", "medium", "high"]);

    acps_command(tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args(["agent", "set", "--effort", "bogus"])
        .assert()
        .failure();

    let config =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config should be readable");
    assert!(!config.contains("effort = "));
}

#[test]
fn agent_set_amp_rejects_effort() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = VALID_CONFIG
        .replace(r#"id = "opencode""#, r#"id = "amp""#)
        .replace(r#"name = "OpenCode""#, r#"name = "Amp Code""#)
        .replace(r#"command = "opencode""#, r#"command = "amp-acp""#)
        .replace(r#"args = ["acp"]"#, r#"args = []"#)
        .replace(r#"env = ["OPENCODE_API_KEY"]"#, r#"env = ["AMP_API_KEY"]"#)
        .replace(
            r#"
[agent.install]
type = "shell"
shell = "curl -fsSL https://opencode.ai/install | bash"
creates = "opencode"
"#,
            "",
        );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    acps_command(tempdir.path())
        .args(["agent", "set", "--effort", "high"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "Amp Code does not support reasoning-effort configuration",
        ));
}

#[test]
fn agent_set_rejects_mode_combined_with_effort() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), codex_config())
        .expect("config should be written");

    acps_command(tempdir.path())
        .args(["agent", "set", "--mode", "auto", "--effort", "high"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "--mode cannot be combined with --provider, --model, --effort, or --api-key-ref",
        ));
}
