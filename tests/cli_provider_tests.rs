#![cfg(all(feature = "dev-tools", feature = "test-fixtures"))]

use acp_stack::secrets::SecretStore;
use predicates::prelude::PredicateBooleanExt as _;
use serde_json::{Value, json};
use std::fs;

mod common;
use common::agent::spawn_provider_models_server;
use common::cli::*;

fn seed_flat_secrets(home: &std::path::Path, env_names: &[&str]) {
    let mut store = SecretStore::open_or_create(home).expect("secret store should open");
    let values = env_names
        .iter()
        .map(|name| ((*name).to_owned(), format!("test-{name}")))
        .collect::<Vec<_>>();
    store
        .set_many(
            values
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str())),
        )
        .expect("flat test secrets should be stored");
}

#[test]
fn agent_provider_use_updates_config_and_generated_opencode_provider() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");
    seed_provider_credential(tempdir.path(), "openai", &["OPENAI_API_KEY"]);
    seed_flat_secrets(tempdir.path(), &["OPENCODE_API_KEY"]);
    let options_path = write_acp_config_options(tempdir.path(), &["openai/gpt-5.5"], &[]);

    acps_command()
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args([
            "agent",
            "provider",
            "use",
            "openai",
            "--model",
            "openai/gpt-5.5",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("target: opencode"))
        .stdout(predicates::str::contains("provider: openai"))
        .stdout(predicates::str::contains("restart the supervised agent"));

    let config = fs::read_to_string(config_dir.join("acps-config.toml"))
        .expect("updated config should be readable");
    assert!(config.contains("[array.targets.agent.provider]"));
    assert!(config.contains(r#"id = "openai""#));
    assert!(config.contains(r#"model = "openai/gpt-5.5""#));
    assert!(!config.contains(r#"api_key_ref = "OPENAI_API_KEY""#));

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
    assert_eq!(
        opencode["provider"]["openai"]["options"]["apiKey"],
        "{env:OPENAI_API_KEY}"
    );
}

#[test]
fn agent_provider_use_uses_agent_native_provider_id_for_collapsed_provider() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");
    seed_provider_credential(tempdir.path(), "vercel-ai-gateway", &["AI_GATEWAY_API_KEY"]);
    seed_flat_secrets(tempdir.path(), &["OPENCODE_API_KEY"]);
    let options_path = write_acp_config_options(tempdir.path(), &["vercel/test-model"], &[]);

    acps_command()
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args([
            "agent",
            "provider",
            "use",
            "vercel-ai-gateway",
            "--model",
            "test-model",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("provider: vercel-ai-gateway"));

    let config = fs::read_to_string(config_dir.join("acps-config.toml"))
        .expect("updated config should be readable");
    assert!(config.contains(r#"id = "vercel-ai-gateway""#));
    assert!(config.contains(r#"model = "vercel/test-model""#));

    let opencode_path = tempdir
        .path()
        .join(".config")
        .join("opencode")
        .join("opencode.json");
    let opencode: Value = serde_json::from_str(
        &fs::read_to_string(opencode_path).expect("opencode config should be readable"),
    )
    .expect("opencode config should parse");
    assert_eq!(opencode["model"], "vercel/test-model");
    assert_eq!(
        opencode["provider"]["vercel"]["options"]["apiKey"],
        "{env:AI_GATEWAY_API_KEY}"
    );
    assert!(opencode["provider"]["vercel-ai-gateway"].is_null());
}

#[test]
fn agent_set_custom_opencode_provider_writes_generated_config() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "agent",
            "set",
            "--custom-provider",
            "--provider",
            "myprovider",
            "--provider-name",
            "My Provider",
            "--base-url",
            "https://api.myprovider.example/v1",
            "--api-key-ref",
            "CUSTOM_API_KEY",
            "--model",
            "my-model",
            "--model-name",
            "My Model",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("api_key_ref: CUSTOM_API_KEY"));

    let config = fs::read_to_string(config_dir.join("acps-config.toml"))
        .expect("updated config should be readable");
    assert!(config.contains(r#"id = "myprovider""#));
    assert!(config.contains(r#"api_key_ref = "CUSTOM_API_KEY""#));
    assert!(config.contains("[array.targets.agent.provider.custom]"));
    assert!(config.contains(r#"context = 200000"#));
    assert!(config.contains(r#"output_max_tokens = 65536"#));

    let opencode_path = tempdir
        .path()
        .join(".config")
        .join("opencode")
        .join("opencode.json");
    let opencode: Value = serde_json::from_str(
        &fs::read_to_string(opencode_path).expect("opencode config should be readable"),
    )
    .expect("opencode config should parse");
    assert_eq!(opencode["model"], "my-model");
    assert_eq!(
        opencode["provider"]["myprovider"]["options"]["apiKey"],
        "{env:CUSTOM_API_KEY}"
    );
    assert_eq!(
        opencode["provider"]["myprovider"]["models"]["my-model"]["limit"]["context"],
        200000
    );
}

#[test]
fn subagent_set_updates_config_and_generated_opencode_small_model() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = format!(
        "{}\n\n[agent.provider]\nid = \"openai\"\nmodel = \"openai/gpt-5.5\"\napi_key_ref = \"OPENAI_API_KEY\"\n\n[agent.providers]\nactive = [\"openai\", \"opencode-go\"]\n",
        VALID_CONFIG.replace(
            r#"env = ["OPENCODE_API_KEY"]"#,
            r#"env = ["OPENCODE_API_KEY", "OPENAI_API_KEY"]"#,
        )
    );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");
    seed_provider_credential(tempdir.path(), "opencode-go", &["OPENCODE_API_KEY"]);
    let options_path = write_acp_config_options(
        tempdir.path(),
        &["openai/gpt-5.5", "opencode-go/deepseek-v4-flash"],
        &[],
    );

    acps_command()
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args([
            "subagent",
            "set",
            "--provider",
            "opencode-go",
            "--model",
            "deepseek-v4-flash",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent: opencode"))
        .stdout(predicates::str::contains("subagent: small_model"))
        .stdout(predicates::str::contains("api_key_ref:").not());

    let config = fs::read_to_string(config_dir.join("acps-config.toml"))
        .expect("updated config should be readable");
    assert!(config.contains("[array.targets.agent.subagent.provider]"));
    assert!(config.contains(r#"model = "opencode-go/deepseek-v4-flash""#));

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
    assert_eq!(opencode["small_model"], "opencode-go/deepseek-v4-flash");
    assert_eq!(
        opencode["enabled_providers"],
        json!(["openai", "opencode-go"])
    );
}

#[test]
fn subagent_set_rejects_anthropic_messages_custom_provider() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "subagent",
            "set",
            "--custom-provider",
            "--provider",
            "myprovider",
            "--provider-name",
            "My Provider",
            "--base-url",
            "https://api.myprovider.example/anthropic",
            "--provider-api",
            "anthropic-messages",
            "--api-key-ref",
            "CUSTOM_API_KEY",
            "--model",
            "my-model",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "anthropic-messages custom providers only support Claude Code",
        ));
}

#[test]
fn subagent_status_prints_provider_model_and_key_ref() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = format!(
        "{VALID_CONFIG}\n\n[agent.subagent.provider]\nid = \"opencode-go\"\nmodel = \"opencode-go/deepseek-v4-flash\"\napi_key_ref = \"OPENCODE_API_KEY\"\n"
    );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["subagent", "status"])
        .assert()
        .success()
        .stdout(predicates::str::contains("subagent: small_model"))
        .stdout(predicates::str::contains("provider: opencode-go"))
        .stdout(predicates::str::contains(
            "model: opencode-go/deepseek-v4-flash",
        ))
        .stdout(predicates::str::contains("api_key_ref: OPENCODE_API_KEY"));
}

#[test]
fn subagent_status_prints_inherited_main_model() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = format!(
        "{VALID_CONFIG}\n\n[agent.provider]\nid = \"opencode-go\"\nmodel = \"opencode-go/deepseek-v4-flash\"\napi_key_ref = \"OPENCODE_API_KEY\"\n"
    );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["subagent", "status"])
        .assert()
        .success()
        .stdout(predicates::str::contains("subagent: small_model"))
        .stdout(predicates::str::contains("status: inherited"))
        .stdout(predicates::str::contains("provider: opencode-go"))
        .stdout(predicates::str::contains(
            "model: opencode-go/deepseek-v4-flash",
        ))
        .stdout(predicates::str::contains("api_key_ref: OPENCODE_API_KEY"));
}

#[test]
fn subagent_match_clears_explicit_provider_and_uses_main_model() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = format!(
        "{}\n\n[agent.provider]\nid = \"openai\"\nmodel = \"openai/gpt-5.5\"\napi_key_ref = \"OPENAI_API_KEY\"\n\n[agent.subagent.provider]\nid = \"opencode-go\"\nmodel = \"opencode-go/deepseek-v4-flash\"\napi_key_ref = \"OPENCODE_API_KEY\"\n",
        VALID_CONFIG.replace(
            r#"env = ["OPENCODE_API_KEY"]"#,
            r#"env = ["OPENCODE_API_KEY", "OPENAI_API_KEY"]"#,
        )
    );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["subagent", "match"])
        .assert()
        .success()
        .stdout(predicates::str::contains("status: inherited"))
        .stdout(predicates::str::contains("provider: openai"))
        .stdout(predicates::str::contains("model: openai/gpt-5.5"))
        .stdout(predicates::str::contains("api_key_ref: OPENAI_API_KEY"));

    let config = fs::read_to_string(config_dir.join("acps-config.toml"))
        .expect("updated config should be readable");
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
    assert_eq!(opencode["enabled_providers"], json!(["openai"]));
}

#[test]
fn subagent_match_reenables_inherit_after_disable() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = format!(
        "{}\n\n[agent.provider]\nid = \"openai\"\nmodel = \"openai/gpt-5.5\"\napi_key_ref = \"OPENAI_API_KEY\"\n\n[agent.subagent]\ndisabled = true\n",
        VALID_CONFIG.replace(
            r#"env = ["OPENCODE_API_KEY"]"#,
            r#"env = ["OPENCODE_API_KEY", "OPENAI_API_KEY"]"#,
        )
    );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["subagent", "match"])
        .assert()
        .success()
        .stdout(predicates::str::contains("status: inherited"))
        .stdout(predicates::str::contains("model: openai/gpt-5.5"));

    let config = fs::read_to_string(config_dir.join("acps-config.toml"))
        .expect("updated config should be readable");
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
    assert_eq!(opencode["small_model"], "openai/gpt-5.5");
}

#[test]
fn subagent_match_rejects_unsupported_agents() {
    for config in [codex_config(), goose_config()] {
        let tempdir = tempfile::tempdir().expect("tempdir should be created");
        let config_dir = tempdir.path().join(".config/acp-stack");
        fs::create_dir_all(&config_dir).expect("config dir should be created");
        fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

        acps_command()
            .env("HOME", tempdir.path())
            .args(["subagent", "match"])
            .assert()
            .failure()
            .stderr(predicates::str::contains(
                "Current agent does not support subagent configuration.",
            ));
    }
}

#[test]
fn subagent_match_requires_configured_main_model_without_mutating_config() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config_path = config_dir.join("acps-config.toml");
    fs::write(&config_path, VALID_CONFIG).expect("config should be written");
    let before = fs::read_to_string(&config_path).expect("config should be readable");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["subagent", "match"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "main provider/model must be configured before `acps subagent match`",
        ));

    let after = fs::read_to_string(config_path).expect("config should be readable after failure");
    assert_eq!(after, before);
}

#[test]
fn subagent_disable_writes_invalid_opencode_small_model() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = format!(
        "{}\n\n[agent.provider]\nid = \"openai\"\nmodel = \"openai/gpt-5.5\"\napi_key_ref = \"OPENAI_API_KEY\"\n",
        VALID_CONFIG.replace(
            r#"env = ["OPENCODE_API_KEY"]"#,
            r#"env = ["OPENCODE_API_KEY", "OPENAI_API_KEY"]"#,
        )
    );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["subagent", "disable"])
        .assert()
        .success()
        .stdout(predicates::str::contains("status: disabled"))
        .stdout(predicates::str::contains("model: invalid/model"));

    let config = fs::read_to_string(config_dir.join("acps-config.toml"))
        .expect("updated config should be readable");
    assert!(config.contains("[array.targets.agent.subagent]"));
    assert!(config.contains("disabled = true"));
    assert!(!config.contains("[array.targets.agent.subagent.provider]"));

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
    assert_eq!(opencode["small_model"], "invalid/model");
    assert_eq!(opencode["enabled_providers"], json!(["openai"]));
}

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

#[test]
fn subagent_set_inherits_provider_and_api_key_ref_from_main_when_omitted() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = format!(
        "{}\n\n[agent.provider]\nid = \"openai\"\nmodel = \"openai/gpt-5.5\"\napi_key_ref = \"OPENAI_CUSTOM_KEY\"\n",
        VALID_CONFIG.replace(
            r#"env = ["OPENCODE_API_KEY"]"#,
            r#"env = ["OPENCODE_API_KEY", "OPENAI_CUSTOM_KEY"]"#,
        )
    );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");
    let options_path = write_acp_config_options(
        tempdir.path(),
        &["openai/gpt-5.5", "openai/gpt-5.5-mini"],
        &[],
    );

    acps_command()
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args(["subagent", "set", "--model", "openai/gpt-5.5-mini"])
        .assert()
        .success()
        .stdout(predicates::str::contains("provider: openai"))
        .stdout(predicates::str::contains("model: openai/gpt-5.5-mini"))
        .stdout(predicates::str::contains("api_key_ref: OPENAI_CUSTOM_KEY"));

    let config = fs::read_to_string(config_dir.join("acps-config.toml"))
        .expect("updated config should be readable");
    assert!(config.contains("[array.targets.agent.subagent.provider]"));
    assert!(config.contains(r#"id = "openai""#));
    assert!(config.contains(r#"model = "openai/gpt-5.5-mini""#));
    assert!(config.contains(r#"api_key_ref = "OPENAI_CUSTOM_KEY""#));
}

#[test]
fn subagent_set_requires_main_provider_when_provider_omitted() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["subagent", "set", "--model", "openai/gpt-5.5-mini"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "--provider not supplied and no main agent provider configured",
        ));
}

#[test]
fn subagent_set_rejects_unsupported_agents() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = VALID_CONFIG
        .replace(r#"id = "opencode""#, r#"id = "cursor""#)
        .replace(r#"name = "OpenCode""#, r#"name = "Cursor CLI""#)
        .replace(r#"command = "opencode""#, r#"command = "cursor-agent""#);
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "subagent",
            "set",
            "--provider",
            "openai",
            "--model",
            "openai/gpt-5.5",
            "--api-key-ref",
            "OPENAI_API_KEY",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "Current agent does not support subagent configuration.",
        ));
}

#[test]
fn subagent_set_rejects_codex_and_goose() {
    for config in [codex_config(), goose_config()] {
        let tempdir = tempfile::tempdir().expect("tempdir should be created");
        let config_dir = tempdir.path().join(".config/acp-stack");
        fs::create_dir_all(&config_dir).expect("config dir should be created");
        fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

        acps_command()
            .env("HOME", tempdir.path())
            .args([
                "subagent",
                "set",
                "--provider",
                "openai",
                "--model",
                "openai/gpt-5.5",
                "--api-key-ref",
                "OPENAI_API_KEY",
            ])
            .assert()
            .failure()
            .stderr(predicates::str::contains(
                "Current agent does not support subagent configuration.",
            ));
    }
}

#[test]
fn subagent_status_rejects_codex_and_goose() {
    for config in [codex_config(), goose_config()] {
        let tempdir = tempfile::tempdir().expect("tempdir should be created");
        let config_dir = tempdir.path().join(".config/acp-stack");
        fs::create_dir_all(&config_dir).expect("config dir should be created");
        fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

        acps_command()
            .env("HOME", tempdir.path())
            .args(["subagent", "status"])
            .assert()
            .failure()
            .stderr(predicates::str::contains(
                "Current agent does not support subagent configuration.",
            ));
    }
}

#[test]
fn subagent_set_rejects_registry_override_for_non_opencode_agent() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = VALID_CONFIG
        .replace(r#"id = "opencode""#, r#"id = "goose""#)
        .replace(r#"name = "OpenCode""#, r#"name = "Goose""#)
        .replace(r#"command = "opencode""#, r#"command = "goose""#);
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");
    seed_auth_verifiers(tempdir.path(), SESSION_KEY, ADMIN_KEY);
    fs::write(
        config_dir.join("agents.toml"),
        r#"
[[agents]]
id = "goose"
name = "Goose"
kind = "native"
headless_compatible = true
set_provider = true
set_model = true
allow_custom_provider = true
allow_custom_model = true
subagents = true
subagent_alias = "small_model"
support_doc = "docs/agents/goose.md"

[agents.harness]
id = "goose"

[agents.harness.install.shell]
script = "true"
creates = "goose"
"#,
    )
    .expect("registry override should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "subagent",
            "set",
            "--provider",
            "opencode-go",
            "--model",
            "opencode-go/deepseek-v4-flash",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "Current agent does not support subagent configuration.",
        ));
}

#[test]
fn agent_set_custom_provider_rejects_comma_token_limits() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "agent",
            "set",
            "--custom-provider",
            "--provider",
            "myprovider",
            "--provider-name",
            "My Provider",
            "--base-url",
            "https://api.myprovider.example/v1",
            "--api-key-ref",
            "CUSTOM_API_KEY",
            "--model",
            "my-model",
            "--context",
            "200,000",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "must be a plain integer without commas",
        ));
}

#[test]
fn agent_provider_use_goose_provider_updates_generated_config() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = VALID_CONFIG
        .replace(r#"id = "opencode""#, r#"id = "goose""#)
        .replace(r#"name = "OpenCode""#, r#"name = "Goose""#)
        .replace(r#"command = "opencode""#, r#"command = "goose""#)
        .replace(
            r#"env = ["OPENCODE_API_KEY"]"#,
            r#"env = ["OPENROUTER_API_KEY"]"#,
        )
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
    seed_provider_credential(tempdir.path(), "openrouter", &["OPENROUTER_API_KEY"]);
    let options_path =
        write_acp_config_options(tempdir.path(), &["deepseek/deepseek-v4-flash"], &[]);

    acps_command()
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args([
            "agent",
            "provider",
            "use",
            "openrouter",
            "--model",
            "deepseek/deepseek-v4-flash",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("provider: openrouter"))
        .stdout(predicates::str::contains(
            "switched live via ACP session/set_config_option",
        ));

    let config = fs::read_to_string(config_dir.join("acps-config.toml"))
        .expect("updated config should be readable");
    assert!(config.contains("[array.targets.agent.provider]"));
    assert!(config.contains(r#"id = "openrouter""#));
    assert!(config.contains(r#"model = "deepseek/deepseek-v4-flash""#));
    assert!(!config.contains(r#"api_key_ref = "OPENROUTER_API_KEY""#));

    let goose_path = tempdir
        .path()
        .join(".config")
        .join("goose")
        .join("config.yaml");
    let goose: serde_norway::Value = serde_norway::from_str(
        &fs::read_to_string(goose_path).expect("goose config should be readable"),
    )
    .expect("goose config should parse");
    assert_eq!(goose["GOOSE_PROVIDER"], "openrouter");
    assert_eq!(goose["GOOSE_MODEL"], "deepseek/deepseek-v4-flash");
}

#[test]
fn agent_provider_use_codex_openrouter_writes_responses_provider_config() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), codex_config())
        .expect("config should be written");
    seed_provider_credential(tempdir.path(), "openrouter", &["OPENROUTER_API_KEY"]);
    let options_path =
        write_acp_config_options(tempdir.path(), &["deepseek/deepseek-v4-flash"], &[]);

    acps_command()
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args([
            "agent",
            "provider",
            "use",
            "openrouter",
            "--model",
            "deepseek/deepseek-v4-flash",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("provider: openrouter"))
        .stdout(predicates::str::contains("restart the supervised agent"));

    let config = fs::read_to_string(config_dir.join("acps-config.toml"))
        .expect("updated config should be readable");
    assert!(config.contains("[array.targets.agent.provider]"));
    assert!(config.contains(r#"id = "openrouter""#));
    assert!(config.contains(r#"model = "deepseek/deepseek-v4-flash""#));
    assert!(!config.contains(r#"api_key_ref = "OPENROUTER_API_KEY""#));

    let codex_path = tempdir.path().join(".codex").join("config.toml");
    let codex: toml::Value =
        toml::from_str(&fs::read_to_string(codex_path).expect("codex config should be readable"))
            .expect("codex config should parse");
    assert_eq!(codex["model"].as_str(), Some("deepseek/deepseek-v4-flash"));
    assert_eq!(codex["model_provider"].as_str(), Some("openrouter"));
    assert_eq!(
        codex["model_providers"]["openrouter"]["base_url"].as_str(),
        Some("https://openrouter.ai/api/v1")
    );
    assert_eq!(
        codex["model_providers"]["openrouter"]["name"].as_str(),
        Some("OpenRouter")
    );
    assert!(
        codex["model_providers"]["openrouter"]
            .get("env_key")
            .is_none(),
        "command-based auth replaces env_key"
    );
    assert_eq!(
        codex["model_providers"]["openrouter"]["auth"]["command"].as_str(),
        Some("sh")
    );
    assert_eq!(
        codex["model_providers"]["openrouter"]["auth"]["args"]
            .as_array()
            .map(|args| args
                .iter()
                .filter_map(toml::Value::as_str)
                .collect::<Vec<_>>()),
        Some(vec!["-c", "echo $OPENROUTER_API_KEY"])
    );
    assert_eq!(
        codex["model_providers"]["openrouter"]["wire_api"].as_str(),
        Some("responses")
    );
}

#[test]
fn agent_provider_use_codex_openai_model_removes_custom_provider_with_backup() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), codex_config())
        .expect("config should be written");
    SecretStore::open_or_create(tempdir.path()).expect("secret store should open");
    let codex_dir = tempdir.path().join(".codex");
    fs::create_dir_all(&codex_dir).expect("codex config dir should be created");
    fs::write(
        codex_dir.join("config.toml"),
        r#"model = "deepseek/deepseek-v4-flash"
model_provider = "openrouter"
preserve = "yes"

[model_providers.openrouter]
name = "OpenRouter"
base_url = "https://openrouter.ai/api/v1/responses"
env_key = "OPENROUTER_API_KEY"
wire_api = "responses"
"#,
    )
    .expect("codex config should be written");
    fs::write(codex_dir.join("config.openrouter.toml"), "occupied\n")
        .expect("existing backup should be written");
    let options_path = write_acp_config_options(tempdir.path(), &["gpt-5.5"], &[]);

    acps_command()
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args(["agent", "provider", "use", "openai", "--model", "gpt-5.5"])
        .assert()
        .success()
        .stdout(predicates::str::contains("provider: openai"))
        .stdout(predicates::str::contains("model: gpt-5.5"))
        .stdout(predicates::str::contains("restart the supervised agent"));

    let config = fs::read_to_string(config_dir.join("acps-config.toml"))
        .expect("updated config should be readable");
    assert!(config.contains("[array.targets.agent.provider]"));
    assert!(config.contains(r#"id = "openai""#));
    assert!(config.contains(r#"model = "gpt-5.5""#));
    assert!(config.contains("env = []"));
    let parsed_config: toml::Value = toml::from_str(&config).expect("config should parse");
    assert!(
        primary_array_agent_value(&parsed_config)["provider"]
            .get("api_key_ref")
            .is_none()
    );

    let codex: toml::Value = toml::from_str(
        &fs::read_to_string(codex_dir.join("config.toml"))
            .expect("codex config should be readable"),
    )
    .expect("codex config should parse");
    assert_eq!(codex["model"].as_str(), Some("gpt-5.5"));
    assert_eq!(codex["model_provider"].as_str(), Some("openai"));
    assert_eq!(codex["preserve"].as_str(), Some("yes"));
    assert!(codex.get("model_providers").is_none());
    let backup = fs::read_to_string(codex_dir.join("config.openrouter-1.toml"))
        .expect("backup should be readable");
    assert!(backup.contains(r#"model_provider = "openrouter""#));
    assert!(backup.contains("[model_providers.openrouter]"));
}

#[test]
fn agent_provider_use_rejects_api_key_ref_argument() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), codex_config())
        .expect("config should be written");
    SecretStore::open_or_create(tempdir.path()).expect("secret store should open");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "agent",
            "provider",
            "use",
            "openai",
            "--model",
            "gpt-5.5",
            "--api-key-ref",
            "OPENAI_API_KEY",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "unexpected argument '--api-key-ref'",
        ));

    let config =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config should be readable");
    assert!(!config.contains("[array.targets.agent.provider]"));
}

#[test]
fn agent_provider_use_codex_openai_allows_omitting_model() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), codex_config())
        .expect("config should be written");
    SecretStore::open_or_create(tempdir.path()).expect("secret store should open");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["agent", "provider", "use", "openai"])
        .assert()
        .success()
        .stdout(predicates::str::contains("provider: openai"));

    let config =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config should be readable");
    let parsed: toml::Value = toml::from_str(&config).expect("config should parse");
    let provider = &primary_array_agent_value(&parsed)["provider"];
    assert_eq!(provider["id"].as_str(), Some("openai"));
    assert!(provider.get("model").is_none());
}

#[test]
fn agent_provider_use_codex_rejects_unsupported_provider() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), codex_config())
        .expect("config should be written");
    SecretStore::open_or_create(tempdir.path()).expect("secret store should open");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "agent",
            "provider",
            "use",
            "anthropic",
            "--model",
            "anthropic/claude-sonnet-4-5",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "provider `anthropic` is not supported for agent `codex`",
        ));
}

#[test]
fn agent_set_codex_custom_provider_defaults_to_responses() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), codex_config())
        .expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "agent",
            "set",
            "--custom-provider",
            "--provider",
            "myprovider",
            "--provider-name",
            "My Provider",
            "--base-url",
            "https://api.myprovider.example/v1",
            "--api-key-ref",
            "CUSTOM_API_KEY",
            "--model",
            "my-model",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("Codex config:"));

    let config = fs::read_to_string(config_dir.join("acps-config.toml"))
        .expect("updated config should be readable");
    assert!(config.contains(r#"api = "responses""#));

    let codex_path = tempdir.path().join(".codex").join("config.toml");
    let codex: toml::Value =
        toml::from_str(&fs::read_to_string(codex_path).expect("codex config should be readable"))
            .expect("codex config should parse");
    assert_eq!(codex["model_provider"].as_str(), Some("myprovider"));
    assert_eq!(
        codex["model_providers"]["myprovider"]["wire_api"].as_str(),
        Some("responses")
    );
}

#[test]
fn agent_set_codex_rejects_chat_completions_custom_provider() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), codex_config())
        .expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "agent",
            "set",
            "--custom-provider",
            "--provider",
            "myprovider",
            "--provider-name",
            "My Provider",
            "--base-url",
            "https://api.myprovider.example/v1",
            "--provider-api",
            "chat-completions",
            "--api-key-ref",
            "CUSTOM_API_KEY",
            "--model",
            "my-model",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "Codex custom providers only support responses",
        ));
}

#[test]
fn agent_set_opencode_rejects_anthropic_messages_custom_provider() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "agent",
            "set",
            "--custom-provider",
            "--provider",
            "myprovider",
            "--provider-name",
            "My Provider",
            "--base-url",
            "https://api.myprovider.example/anthropic",
            "--provider-api",
            "anthropic-messages",
            "--api-key-ref",
            "CUSTOM_API_KEY",
            "--model",
            "my-model",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "anthropic-messages custom providers only support Claude Code",
        ));
}

#[test]
fn agent_provider_use_claude_code_native_provider_presets_write_headless_config() {
    struct Case {
        provider: &'static str,
        model: &'static str,
        api_key_ref: Option<&'static str>,
        env_refs: &'static [&'static str],
        native_env_key: Option<&'static str>,
    }

    let cases = [
        Case {
            provider: "anthropic",
            model: "claude-sonnet-4-5",
            api_key_ref: Some("ANTHROPIC_API_KEY"),
            env_refs: &["ANTHROPIC_API_KEY"],
            native_env_key: None,
        },
        Case {
            provider: "amazon-bedrock",
            model: "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
            api_key_ref: None,
            env_refs: &[],
            native_env_key: Some("CLAUDE_CODE_USE_BEDROCK"),
        },
        Case {
            provider: "google-vertex-anthropic",
            model: "claude-sonnet-4-vertex",
            api_key_ref: None,
            env_refs: &["ANTHROPIC_VERTEX_PROJECT_ID", "CLOUD_ML_REGION"],
            native_env_key: Some("CLAUDE_CODE_USE_VERTEX"),
        },
        Case {
            provider: "microsoft-foundry",
            model: "claude-sonnet-4-foundry",
            api_key_ref: Some("ANTHROPIC_FOUNDRY_API_KEY"),
            env_refs: &["ANTHROPIC_FOUNDRY_API_KEY", "ANTHROPIC_FOUNDRY_BASE_URL"],
            native_env_key: Some("CLAUDE_CODE_USE_FOUNDRY"),
        },
    ];

    for case in cases {
        let tempdir = tempfile::tempdir().expect("tempdir should be created");
        let config_dir = tempdir.path().join(".config/acp-stack");
        fs::create_dir_all(&config_dir).expect("config dir should be created");
        fs::write(config_dir.join("acps-config.toml"), claude_code_config())
            .expect("config should be written");
        if case.api_key_ref.is_some() {
            seed_provider_credential(tempdir.path(), case.provider, case.env_refs);
        } else if case.env_refs.is_empty() {
            SecretStore::open_or_create(tempdir.path()).expect("secret store should open");
        } else {
            let values = case
                .env_refs
                .iter()
                .map(|name| (*name, "test-native-value"))
                .collect::<Vec<_>>();
            seed_init_secrets(tempdir.path(), &values);
        }

        let output = acps_command()
            .env("HOME", tempdir.path())
            .args([
                "agent",
                "provider",
                "use",
                case.provider,
                "--model",
                case.model,
            ])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let stdout = String::from_utf8(output).expect("stdout should be utf8");
        assert!(
            stdout.contains(&format!("provider: {}", case.provider)),
            "{stdout}"
        );
        assert!(
            stdout.contains(&format!("model: {}", case.model)),
            "{stdout}"
        );

        let config_text = fs::read_to_string(config_dir.join("acps-config.toml"))
            .expect("config should be readable");
        let config: toml::Value = toml::from_str(&config_text).expect("config should parse");
        let agent = primary_array_agent_value(&config);
        let provider = &agent["provider"];
        assert_eq!(provider["id"].as_str(), Some(case.provider));
        assert_eq!(provider["model"].as_str(), Some(case.model));
        assert!(provider.get("api_key_ref").is_none());
        if case.api_key_ref.is_none() {
            let env_refs = agent["env"]
                .as_array()
                .expect("agent env should be an array");
            for expected in case.env_refs {
                assert!(
                    env_refs
                        .iter()
                        .any(|value| value.as_str() == Some(*expected)),
                    "{case_provider} missing env ref {expected}",
                    case_provider = case.provider,
                );
            }
        }

        let settings = claude_settings(tempdir.path());
        assert_eq!(
            settings["env"]["ANTHROPIC_MODEL"].as_str(),
            Some(case.model)
        );
        assert!(settings["env"].get("ANTHROPIC_BASE_URL").is_none());
        if let Some(native_env_key) = case.native_env_key {
            assert_eq!(settings["env"][native_env_key].as_str(), Some("1"));
        }
        if let Some(api_key_ref) = case.api_key_ref {
            let helper = format!("printenv {api_key_ref}");
            assert_eq!(settings["apiKeyHelper"].as_str(), Some(helper.as_str()));
            assert!(!stdout.contains("api_key_ref:"), "{stdout}");
        } else {
            assert!(settings.get("apiKeyHelper").is_none());
            assert!(!stdout.contains("api_key_ref:"), "{stdout}");
        }
    }
}

#[test]
fn agent_provider_use_claude_code_third_party_presets_write_profiled_endpoints() {
    struct Case {
        provider: &'static str,
        base_url: &'static str,
        api_key_ref: &'static str,
    }

    let cases = [
        Case {
            provider: "deepseek",
            base_url: "https://api.deepseek.com/anthropic",
            api_key_ref: "DEEPSEEK_API_KEY",
        },
        Case {
            provider: "moonshotai",
            base_url: "https://api.moonshot.ai/anthropic",
            api_key_ref: "MOONSHOT_API_KEY",
        },
        Case {
            provider: "kimi-coding-plan",
            base_url: "https://api.kimi.com/coding/",
            api_key_ref: "KIMI_API_KEY",
        },
        Case {
            provider: "moonshotai-cn",
            base_url: "https://api.moonshot.cn/anthropic",
            api_key_ref: "MOONSHOT_API_KEY",
        },
        Case {
            provider: "zai",
            base_url: "https://api.z.ai/api/anthropic",
            api_key_ref: "ZAI_API_KEY",
        },
        Case {
            provider: "zhipuai",
            base_url: "https://api.z.ai/api/anthropic",
            api_key_ref: "ZAI_API_KEY",
        },
        Case {
            provider: "minimax",
            base_url: "https://api.minimax.io/anthropic",
            api_key_ref: "MINIMAX_API_KEY",
        },
        Case {
            provider: "minimax-coding-plan",
            base_url: "https://api.minimax.io/anthropic",
            api_key_ref: "MINIMAX_API_KEY",
        },
        Case {
            provider: "minimax-cn",
            base_url: "https://api.minimaxi.com/anthropic",
            api_key_ref: "MINIMAX_CN_API_KEY",
        },
        Case {
            provider: "minimax-cn-coding-plan",
            base_url: "https://api.minimaxi.com/anthropic",
            api_key_ref: "MINIMAX_CN_API_KEY",
        },
        Case {
            provider: "xiaomi",
            base_url: "https://api.xiaomimimo.com/anthropic",
            api_key_ref: "XIAOMI_API_KEY",
        },
        Case {
            provider: "xiaomi-token-plan-cn",
            base_url: "https://token-plan-cn.xiaomimimo.com/anthropic",
            api_key_ref: "XIAOMI_TOKEN_PLAN_CN_API_KEY",
        },
        Case {
            provider: "xiaomi-token-plan-ams",
            base_url: "https://token-plan-ams.xiaomimimo.com/anthropic",
            api_key_ref: "XIAOMI_TOKEN_PLAN_AMS_API_KEY",
        },
        Case {
            provider: "xiaomi-token-plan-sgp",
            base_url: "https://token-plan-sgp.xiaomimimo.com/anthropic",
            api_key_ref: "XIAOMI_TOKEN_PLAN_SGP_API_KEY",
        },
    ];

    for case in cases {
        let tempdir = tempfile::tempdir().expect("tempdir should be created");
        let config_dir = tempdir.path().join(".config/acp-stack");
        fs::create_dir_all(&config_dir).expect("config dir should be created");
        fs::write(config_dir.join("acps-config.toml"), claude_code_config())
            .expect("config should be written");
        seed_provider_credential(tempdir.path(), case.provider, &[case.api_key_ref]);

        acps_command()
            .env("HOME", tempdir.path())
            .args([
                "agent",
                "provider",
                "use",
                case.provider,
                "--model",
                "provider-profile-model",
            ])
            .assert()
            .success();

        let config_text = fs::read_to_string(config_dir.join("acps-config.toml"))
            .expect("config should be readable");
        let config: toml::Value = toml::from_str(&config_text).expect("config should parse");
        assert!(
            primary_array_agent_value(&config)["provider"]
                .get("api_key_ref")
                .is_none()
        );

        let settings = claude_settings(tempdir.path());
        assert_eq!(
            settings["env"]["ANTHROPIC_BASE_URL"].as_str(),
            Some(case.base_url),
            "{}",
            case.provider
        );
        assert_eq!(
            settings["env"]["ANTHROPIC_MODEL"].as_str(),
            Some("provider-profile-model")
        );
        for key in [
            "ANTHROPIC_DEFAULT_FABLE_MODEL",
            "ANTHROPIC_DEFAULT_OPUS_MODEL",
            "ANTHROPIC_DEFAULT_SONNET_MODEL",
            "ANTHROPIC_DEFAULT_HAIKU_MODEL",
        ] {
            assert_eq!(
                settings["env"][key].as_str(),
                Some("provider-profile-model"),
                "{provider} {key}",
                provider = case.provider,
            );
        }
        let helper = format!("printenv {}", case.api_key_ref);
        assert_eq!(settings["apiKeyHelper"].as_str(), Some(helper.as_str()));
        assert!(!settings.to_string().contains("test-secret"));
    }
}

#[test]
fn agent_provider_use_claude_code_third_party_provider_without_model_uses_profile_default() {
    struct Case {
        provider: &'static str,
        base_url: &'static str,
        api_key_ref: &'static str,
        model: &'static str,
        opus_model: &'static str,
        sonnet_model: &'static str,
        haiku_model: &'static str,
        subagent_model: Option<&'static str>,
    }

    let cases = [
        Case {
            provider: "deepseek",
            base_url: "https://api.deepseek.com/anthropic",
            api_key_ref: "DEEPSEEK_API_KEY",
            model: "deepseek-v4-pro[1m]",
            opus_model: "deepseek-v4-pro[1m]",
            sonnet_model: "deepseek-v4-pro[1m]",
            haiku_model: "deepseek-v4-flash",
            subagent_model: Some("deepseek-v4-flash"),
        },
        Case {
            provider: "moonshotai",
            base_url: "https://api.moonshot.ai/anthropic",
            api_key_ref: "MOONSHOT_API_KEY",
            model: "kimi-k3[1m]",
            opus_model: "kimi-k3[1m]",
            sonnet_model: "kimi-k3[1m]",
            haiku_model: "kimi-k3[1m]",
            subagent_model: Some("kimi-k3[1m]"),
        },
        Case {
            provider: "kimi-coding-plan",
            base_url: "https://api.kimi.com/coding/",
            api_key_ref: "KIMI_API_KEY",
            model: "kimi-for-coding",
            opus_model: "kimi-for-coding",
            sonnet_model: "kimi-for-coding",
            haiku_model: "kimi-for-coding",
            subagent_model: Some("kimi-for-coding"),
        },
        Case {
            provider: "moonshotai-cn",
            base_url: "https://api.moonshot.cn/anthropic",
            api_key_ref: "MOONSHOT_API_KEY",
            model: "kimi-k3[1m]",
            opus_model: "kimi-k3[1m]",
            sonnet_model: "kimi-k3[1m]",
            haiku_model: "kimi-k3[1m]",
            subagent_model: Some("kimi-k3[1m]"),
        },
        Case {
            provider: "zai",
            base_url: "https://api.z.ai/api/anthropic",
            api_key_ref: "ZAI_API_KEY",
            model: "glm-5.2[1m]",
            opus_model: "glm-5.2[1m]",
            sonnet_model: "glm-5.2[1m]",
            haiku_model: "GLM-4.7",
            subagent_model: None,
        },
        Case {
            provider: "zhipuai",
            base_url: "https://api.z.ai/api/anthropic",
            api_key_ref: "ZAI_API_KEY",
            model: "glm-5.2[1m]",
            opus_model: "glm-5.2[1m]",
            sonnet_model: "glm-5.2[1m]",
            haiku_model: "GLM-4.7",
            subagent_model: None,
        },
        Case {
            provider: "minimax",
            base_url: "https://api.minimax.io/anthropic",
            api_key_ref: "MINIMAX_API_KEY",
            model: "MiniMax-M3",
            opus_model: "MiniMax-M3",
            sonnet_model: "MiniMax-M3",
            haiku_model: "MiniMax-M3",
            subagent_model: None,
        },
        Case {
            provider: "minimax-coding-plan",
            base_url: "https://api.minimax.io/anthropic",
            api_key_ref: "MINIMAX_API_KEY",
            model: "MiniMax-M3",
            opus_model: "MiniMax-M3",
            sonnet_model: "MiniMax-M3",
            haiku_model: "MiniMax-M3",
            subagent_model: None,
        },
        Case {
            provider: "minimax-cn",
            base_url: "https://api.minimaxi.com/anthropic",
            api_key_ref: "MINIMAX_CN_API_KEY",
            model: "MiniMax-M3",
            opus_model: "MiniMax-M3",
            sonnet_model: "MiniMax-M3",
            haiku_model: "MiniMax-M3",
            subagent_model: None,
        },
        Case {
            provider: "minimax-cn-coding-plan",
            base_url: "https://api.minimaxi.com/anthropic",
            api_key_ref: "MINIMAX_CN_API_KEY",
            model: "MiniMax-M3",
            opus_model: "MiniMax-M3",
            sonnet_model: "MiniMax-M3",
            haiku_model: "MiniMax-M3",
            subagent_model: None,
        },
        Case {
            provider: "xiaomi",
            base_url: "https://api.xiaomimimo.com/anthropic",
            api_key_ref: "XIAOMI_API_KEY",
            model: "mimo-v2.5-pro",
            opus_model: "mimo-v2.5-pro",
            sonnet_model: "mimo-v2.5-pro",
            haiku_model: "mimo-v2.5-pro",
            subagent_model: None,
        },
        Case {
            provider: "xiaomi-token-plan-cn",
            base_url: "https://token-plan-cn.xiaomimimo.com/anthropic",
            api_key_ref: "XIAOMI_TOKEN_PLAN_CN_API_KEY",
            model: "mimo-v2.5-pro",
            opus_model: "mimo-v2.5-pro",
            sonnet_model: "mimo-v2.5-pro",
            haiku_model: "mimo-v2.5-pro",
            subagent_model: None,
        },
        Case {
            provider: "xiaomi-token-plan-ams",
            base_url: "https://token-plan-ams.xiaomimimo.com/anthropic",
            api_key_ref: "XIAOMI_TOKEN_PLAN_AMS_API_KEY",
            model: "mimo-v2.5-pro",
            opus_model: "mimo-v2.5-pro",
            sonnet_model: "mimo-v2.5-pro",
            haiku_model: "mimo-v2.5-pro",
            subagent_model: None,
        },
        Case {
            provider: "xiaomi-token-plan-sgp",
            base_url: "https://token-plan-sgp.xiaomimimo.com/anthropic",
            api_key_ref: "XIAOMI_TOKEN_PLAN_SGP_API_KEY",
            model: "mimo-v2.5-pro",
            opus_model: "mimo-v2.5-pro",
            sonnet_model: "mimo-v2.5-pro",
            haiku_model: "mimo-v2.5-pro",
            subagent_model: None,
        },
    ];

    for case in cases {
        let tempdir = tempfile::tempdir().expect("tempdir should be created");
        let config_dir = tempdir.path().join(".config/acp-stack");
        fs::create_dir_all(&config_dir).expect("config dir should be created");
        fs::write(config_dir.join("acps-config.toml"), claude_code_config())
            .expect("config should be written");
        seed_provider_credential(tempdir.path(), case.provider, &[case.api_key_ref]);

        let output = acps_command()
            .env("HOME", tempdir.path())
            .args(["agent", "provider", "use", case.provider])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let stdout = String::from_utf8(output).expect("stdout should be utf8");
        assert!(stdout.contains(&format!("provider: {}", case.provider)));
        assert!(!stdout.contains("model:"), "{stdout}");

        let config_text = fs::read_to_string(config_dir.join("acps-config.toml"))
            .expect("config should be readable");
        let config: toml::Value = toml::from_str(&config_text).expect("config should parse");
        let provider = &primary_array_agent_value(&config)["provider"];
        assert_eq!(provider["id"].as_str(), Some(case.provider));
        assert!(provider.get("api_key_ref").is_none());
        assert!(provider.get("model").is_none());

        let settings = claude_settings(tempdir.path());
        assert_eq!(
            settings["env"]["ANTHROPIC_BASE_URL"].as_str(),
            Some(case.base_url),
            "{}",
            case.provider
        );
        assert_eq!(
            settings["env"]["ANTHROPIC_MODEL"].as_str(),
            Some(case.model)
        );
        assert_eq!(
            settings["env"]["ANTHROPIC_DEFAULT_FABLE_MODEL"].as_str(),
            Some(case.opus_model)
        );
        assert_eq!(
            settings["env"]["ANTHROPIC_DEFAULT_OPUS_MODEL"].as_str(),
            Some(case.opus_model)
        );
        assert_eq!(
            settings["env"]["ANTHROPIC_DEFAULT_SONNET_MODEL"].as_str(),
            Some(case.sonnet_model)
        );
        assert_eq!(
            settings["env"]["ANTHROPIC_DEFAULT_HAIKU_MODEL"].as_str(),
            Some(case.haiku_model)
        );
        match case.subagent_model {
            Some(model) => {
                assert_eq!(
                    settings["env"]["CLAUDE_CODE_SUBAGENT_MODEL"].as_str(),
                    Some(model)
                );
            }
            None => {
                assert!(
                    settings["env"].get("CLAUDE_CODE_SUBAGENT_MODEL").is_none(),
                    "{}",
                    case.provider
                );
            }
        }
    }
}

#[test]
fn agent_set_claude_code_custom_provider_defaults_to_anthropic_messages() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), claude_code_config())
        .expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "agent",
            "set",
            "--custom-provider",
            "--provider",
            "myanthropic",
            "--provider-name",
            "My Anthropic",
            "--base-url",
            "https://api.myanthropic.example/anthropic",
            "--api-key-ref",
            "CUSTOM_CLAUDE_API_KEY",
            "--model",
            "custom-claude-model",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("Claude Code config:"));

    let config =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config should be readable");
    assert!(config.contains(r#"api = "anthropic-messages""#));
    assert!(config.contains(r#"api_key_ref = "CUSTOM_CLAUDE_API_KEY""#));

    let settings = claude_settings(tempdir.path());
    assert_eq!(
        settings["env"]["ANTHROPIC_BASE_URL"].as_str(),
        Some("https://api.myanthropic.example/anthropic")
    );
    assert_eq!(
        settings["env"]["ANTHROPIC_MODEL"].as_str(),
        Some("custom-claude-model")
    );
    assert_eq!(
        settings["apiKeyHelper"].as_str(),
        Some("printenv CUSTOM_CLAUDE_API_KEY")
    );
    let onboarding: Value = serde_json::from_str(
        &fs::read_to_string(tempdir.path().join(".claude.json"))
            .expect("Claude onboarding config should be readable"),
    )
    .expect("Claude onboarding config should parse");
    assert_eq!(onboarding["hasCompletedOnboarding"], true);
}

#[test]
fn agent_set_claude_code_rejects_non_anthropic_messages_custom_provider() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), claude_code_config())
        .expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "agent",
            "set",
            "--custom-provider",
            "--provider",
            "myanthropic",
            "--provider-name",
            "My Anthropic",
            "--base-url",
            "https://api.myanthropic.example/v1",
            "--provider-api",
            "chat-completions",
            "--api-key-ref",
            "CUSTOM_CLAUDE_API_KEY",
            "--model",
            "custom-claude-model",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "Claude Code custom providers only support anthropic-messages",
        ));
}

#[test]
fn agent_set_cursor_accepts_openai_model_from_acp_options() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = VALID_CONFIG
        .replace(r#"id = "opencode""#, r#"id = "cursor""#)
        .replace(r#"name = "OpenCode""#, r#"name = "Cursor CLI""#)
        .replace(r#"command = "opencode""#, r#"command = "cursor-agent""#)
        .replace(
            r#"env = ["OPENCODE_API_KEY"]"#,
            r#"env = ["CURSOR_API_KEY"]"#,
        )
        .replace(
            r#"
[agent.install]
type = "shell"
shell = "curl -fsSL https://opencode.ai/install | bash"
creates = "opencode"
"#,
            "",
        );
    fs::write(config_dir.join("acps-config.toml"), &config).expect("config should be written");
    let options_path = write_acp_config_options(
        tempdir.path(),
        &["gpt-5.5[context=272k,reasoning=medium,fast=false]"],
        &[],
    );

    acps_command()
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args(["agent", "set", "--model", "gpt-5.5"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "required_env_refs: CURSOR_API_KEY",
        ));

    let after =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config should be readable");
    assert!(after.contains(r#"env = ["CURSOR_API_KEY"]"#));
    assert!(!after.contains("[array.targets.agent.provider]"));
    assert!(after.contains(r#"model = "gpt-5.5[context=272k,reasoning=medium,fast=false]""#));
    assert!(!after.contains(r#"api_key_ref = "CURSOR_API_KEY""#));
}

#[test]
fn agent_set_kimi_accepts_exact_model_without_acp_discovery() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), kimi_config())
        .expect("config should be written");
    seed_init_secrets(tempdir.path(), &[("KIMI_API_KEY", "test-kimi-key")]);

    acps_command()
        .env("HOME", tempdir.path())
        .args(["agent", "set", "--model", "kimi-for-coding-highspeed"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent: kimi"))
        .stdout(predicates::str::contains(
            "model: kimi-for-coding-highspeed",
        ))
        .stdout(predicates::str::contains("required_env_refs: KIMI_API_KEY"));

    let config = fs::read_to_string(config_dir.join("acps-config.toml")).expect("config readable");
    assert!(config.contains(r#"env = ["KIMI_API_KEY"]"#));
    assert!(config.contains(r#"model = "kimi-for-coding-highspeed""#));
    assert!(!config.contains("KIMI_MODEL_"));
    assert!(!config.contains("test-kimi-key"));
    assert!(!config.contains("[agent.provider]"));
}

#[test]
fn agent_provider_use_cursor_rejects_provider_selection() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = VALID_CONFIG
        .replace(r#"id = "opencode""#, r#"id = "cursor""#)
        .replace(r#"name = "OpenCode""#, r#"name = "Cursor CLI""#)
        .replace(r#"command = "opencode""#, r#"command = "cursor-agent""#)
        .replace(
            r#"env = ["OPENCODE_API_KEY"]"#,
            r#"env = ["CURSOR_API_KEY"]"#,
        )
        .replace(
            r#"
[agent.install]
type = "shell"
shell = "curl -fsSL https://opencode.ai/install | bash"
creates = "opencode"
"#,
            "",
        );
    fs::write(config_dir.join("acps-config.toml"), &config).expect("config should be written");
    SecretStore::open_or_create(tempdir.path()).expect("secret store should open");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["agent", "provider", "use", "openai", "--model", "gpt-5.5"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "Cursor CLI does not support mapped provider selection",
        ));

    let after =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config should be readable");
    assert!(!after.contains("[array.targets.agent.provider]"));
}

#[test]
fn agent_set_amp_rejects_custom_provider() {
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

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "agent",
            "set",
            "--custom-provider",
            "--provider",
            "myprovider",
            "--provider-name",
            "My Provider",
            "--base-url",
            "https://api.myprovider.example/v1",
            "--api-key-ref",
            "CUSTOM_API_KEY",
            "--model",
            "my-model",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "Amp Code does not support custom provider setup",
        ));
}

#[test]
fn agent_set_opencode_rejects_model_without_provider() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["agent", "set", "--model", "gpt-5.5"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "select a mapped provider with `acps agent provider use` before setting a model for OpenCode",
        ));
}

#[test]
fn agent_set_model_uses_existing_provider() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = VALID_CONFIG
        .replace(
            r#"env = ["OPENCODE_API_KEY"]"#,
            r#"env = ["OPENAI_API_KEY"]"#,
        )
        .replace(
            r#"restart = "on-crash""#,
            r#"restart = "on-crash"

[agent.provider]
id = "openai"
api_key_ref = "OPENAI_API_KEY""#,
        );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");
    let options_path = write_acp_config_options(tempdir.path(), &["openai/gpt-5.5"], &[]);

    acps_command()
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args(["agent", "set", "--model", "gpt-5.5"])
        .assert()
        .success()
        .stdout(predicates::str::contains("provider: openai"))
        .stdout(predicates::str::contains("model: openai/gpt-5.5"));

    let config =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config should be readable");
    assert!(config.contains("[array.targets.agent.provider]"));
    assert!(config.contains(r#"model = "openai/gpt-5.5""#));
}

#[test]
fn agent_provider_use_rejects_provider_not_supported_by_agent() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");
    SecretStore::open_or_create(tempdir.path()).expect("secret store should open");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "agent",
            "provider",
            "use",
            "azure-openai-responses",
            "--model",
            "azure-openai-responses/test-model",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "provider `azure-openai-responses` is not supported for agent `opencode`",
        ));

    let after =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config should be readable");
    assert!(!after.contains("[array.targets.agent.provider]"));
}

#[test]
fn agent_provider_use_rejects_providers_without_api_key_mapping() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");
    SecretStore::open_or_create(tempdir.path()).expect("secret store should open");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "agent",
            "provider",
            "use",
            "google-vertex",
            "--model",
            "google-vertex/test-model",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "provider `google-vertex` does not use an acps-managed API key",
        ));

    let after =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config should be readable");
    assert!(!after.contains("[array.targets.agent.provider]"));
}

#[test]
fn agent_provider_use_resolves_cloudflare_companion_fields() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = VALID_CONFIG
        .replace(r#"id = "opencode""#, r#"id = "pi""#)
        .replace(r#"name = "OpenCode""#, r#"name = "Pi Agent""#)
        .replace(r#"command = "opencode""#, r#"command = "pi-acp""#)
        .replace(r#"args = ["acp"]"#, r#"args = []"#)
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
    seed_provider_credential(
        tempdir.path(),
        "cloudflare-ai-gateway",
        &[
            "CLOUDFLARE_API_KEY",
            "CLOUDFLARE_ACCOUNT_ID",
            "CLOUDFLARE_GATEWAY_ID",
        ],
    );
    seed_flat_secrets(tempdir.path(), &["OPENCODE_API_KEY"]);
    let options_path = write_acp_config_options(
        tempdir.path(),
        &["workers-ai/@cf/moonshotai/kimi-k2.6"],
        &[],
    );

    acps_command()
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args([
            "agent",
            "provider",
            "use",
            "cloudflare-ai-gateway",
            "--model",
            "workers-ai/@cf/moonshotai/kimi-k2.6",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("provider: cloudflare-ai-gateway"));

    let config = fs::read_to_string(config_dir.join("acps-config.toml"))
        .expect("updated config should be readable");
    assert!(config.contains(r#"id = "cloudflare-ai-gateway""#));
    assert!(config.contains(r#"model = "workers-ai/@cf/moonshotai/kimi-k2.6""#));
    assert!(!config.contains(r#"api_key_ref = "CLOUDFLARE_API_KEY""#));
}

#[test]
fn agent_provider_use_opencode_cloudflare_gateway_uses_canonical_token_env() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");
    seed_provider_credential(
        tempdir.path(),
        "cloudflare-ai-gateway",
        &[
            "CLOUDFLARE_API_KEY",
            "CLOUDFLARE_ACCOUNT_ID",
            "CLOUDFLARE_GATEWAY_ID",
        ],
    );
    seed_flat_secrets(tempdir.path(), &["OPENCODE_API_KEY"]);
    let options_path = write_acp_config_options(
        tempdir.path(),
        &["cloudflare-ai-gateway/workers-ai/@cf/moonshotai/kimi-k2.6"],
        &[],
    );

    acps_command()
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args([
            "agent",
            "provider",
            "use",
            "cloudflare-ai-gateway",
            "--model",
            "cloudflare-ai-gateway/workers-ai/@cf/moonshotai/kimi-k2.6",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("provider: cloudflare-ai-gateway"));

    let config = fs::read_to_string(config_dir.join("acps-config.toml"))
        .expect("updated config should be readable");
    assert!(!config.contains(r#"api_key_ref = "CLOUDFLARE_API_TOKEN""#));
    assert!(!config.contains(r#""CLOUDFLARE_API_KEY""#));

    let opencode_path = tempdir
        .path()
        .join(".config")
        .join("opencode")
        .join("opencode.json");
    let opencode: Value = serde_json::from_str(
        &fs::read_to_string(opencode_path).expect("opencode config should be readable"),
    )
    .expect("opencode config should parse");
    assert_eq!(
        opencode["model"],
        "cloudflare-ai-gateway/workers-ai/@cf/moonshotai/kimi-k2.6"
    );
    assert_eq!(
        opencode["provider"]["cloudflare-ai-gateway"]["options"]["apiKey"],
        "{env:CLOUDFLARE_API_TOKEN}"
    );
}

#[test]
fn agent_provider_use_without_model_selects_provider_without_model() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");
    seed_provider_credential(
        tempdir.path(),
        "cloudflare-workers-ai",
        &["CLOUDFLARE_API_KEY", "CLOUDFLARE_ACCOUNT_ID"],
    );
    seed_flat_secrets(tempdir.path(), &["OPENCODE_API_KEY"]);
    let options_path = write_acp_config_options(
        tempdir.path(),
        &["cloudflare-workers-ai/@cf/moonshotai/kimi-k2.6"],
        &[],
    );

    acps_command()
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args(["agent", "provider", "use", "cloudflare-workers-ai"])
        .assert()
        .success()
        .stdout(predicates::str::contains("provider: cloudflare-workers-ai"));

    let config =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config should be readable");
    assert!(config.contains("[array.targets.agent.provider]"));
    assert!(config.contains(r#"id = "cloudflare-workers-ai""#));
    let parsed: toml::Value = toml::from_str(&config).expect("config should parse");
    assert!(
        primary_array_agent_value(&parsed)["provider"]
            .get("model")
            .is_none()
    );
}

#[test]
fn agent_provider_use_does_not_partially_write_main_config_when_provisioning_fails() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");
    seed_provider_credential(tempdir.path(), "openai", &["OPENAI_API_KEY"]);
    seed_flat_secrets(tempdir.path(), &["OPENCODE_API_KEY"]);
    let options_path = write_acp_config_options(tempdir.path(), &["openai/gpt-5.5"], &[]);
    let opencode_dir = tempdir.path().join(".config").join("opencode");
    fs::create_dir_all(&opencode_dir).expect("opencode config dir should be created");
    fs::write(opencode_dir.join("opencode.json"), "[]")
        .expect("invalid opencode config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args([
            "agent",
            "provider",
            "use",
            "openai",
            "--model",
            "openai/gpt-5.5",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "existing JSON root must be an object",
        ));

    let config =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config should be readable");
    assert!(!config.contains("[array.targets.agent.provider]"));
    assert!(!config.contains(r#""OPENAI_API_KEY""#));
}

#[test]
fn agent_provider_use_validates_model_against_acp_config_options() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");
    seed_provider_credential(tempdir.path(), "openai", &["OPENAI_API_KEY"]);
    let options_path = write_acp_config_options(tempdir.path(), &["openai/gpt-5.5"], &[]);

    acps_command()
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args([
            "agent",
            "provider",
            "use",
            "openai",
            "--model",
            "openai/not-advertised",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "agent did not advertise `openai/not-advertised` as an available `model`",
        ));

    let config =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config should be readable");
    assert!(!config.contains("[array.targets.agent.provider]"));
    assert!(
        !tempdir
            .path()
            .join(".config/opencode/opencode.json")
            .exists(),
        "failed discovery must restore the prior OpenCode config state"
    );
}

#[test]
fn agent_set_amp_accepts_mode_only() {
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
    let options_path = write_acp_config_options(tempdir.path(), &[], &["smart", "rush", "deep"]);

    acps_command()
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args(["agent", "set", "--mode", "smart"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent: amp"))
        .stdout(predicates::str::contains("mode: smart"))
        .stdout(predicates::str::contains(
            "restart the supervised agent (`POST /v1/agent/restart`) to reload from disk",
        ));

    let config =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config should be readable");
    assert!(config.contains(r#"mode = "smart""#));
    assert!(!config.contains("[array.targets.agent.provider]"));
}

#[test]
fn agent_set_opencode_accepts_mode_only() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");
    let options_path = write_acp_config_options(tempdir.path(), &[], &["build", "plan"]);

    acps_command()
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args(["agent", "set", "--mode", "plan"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent: opencode"))
        .stdout(predicates::str::contains("mode: plan"))
        .stdout(predicates::str::contains(
            "restart the supervised agent (`POST /v1/agent/restart`) to reload from disk",
        ));

    let config =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config should be readable");
    assert!(config.contains(r#"mode = "plan""#));
    assert!(!config.contains("[array.targets.agent.provider]"));
}

#[test]
fn agent_set_cursor_accepts_mode_only() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = VALID_CONFIG
        .replace(r#"id = "opencode""#, r#"id = "cursor""#)
        .replace(r#"name = "OpenCode""#, r#"name = "Cursor CLI""#)
        .replace(r#"command = "opencode""#, r#"command = "cursor-agent""#)
        .replace(
            r#"env = ["OPENCODE_API_KEY"]"#,
            r#"env = ["CURSOR_API_KEY"]"#,
        )
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
    let options_path = write_acp_config_options(tempdir.path(), &[], &["agent", "ask", "plan"]);

    acps_command()
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args(["agent", "set", "--mode", "plan"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent: cursor"))
        .stdout(predicates::str::contains("mode: plan"))
        .stdout(predicates::str::contains(
            "restart the supervised agent (`POST /v1/agent/restart`) to reload from disk",
        ));

    let config =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config should be readable");
    assert!(config.contains(r#"mode = "plan""#));
    assert!(!config.contains("[array.targets.agent.provider]"));
}

#[test]
fn agent_set_codex_accepts_mode_only() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), codex_config())
        .expect("config should be written");
    let options_path =
        write_acp_config_options(tempdir.path(), &[], &["read-only", "auto", "full-access"]);

    acps_command()
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args(["agent", "set", "--mode", "full-access"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent: codex"))
        .stdout(predicates::str::contains("mode: full-access"))
        .stdout(predicates::str::contains(
            "restart the supervised agent (`POST /v1/agent/restart`) to reload from disk",
        ));

    let config =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config should be readable");
    assert!(config.contains(r#"mode = "full-access""#));
    assert!(!config.contains("[array.targets.agent.provider]"));
}

#[test]
fn agent_set_pi_rejects_mode() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = VALID_CONFIG
        .replace(r#"id = "opencode""#, r#"id = "pi""#)
        .replace(r#"name = "OpenCode""#, r#"name = "Pi Agent""#)
        .replace(r#"command = "opencode""#, r#"command = "pi-acp""#)
        .replace(r#"args = ["acp"]"#, r#"args = []"#)
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

    acps_command()
        .env("HOME", tempdir.path())
        .args(["agent", "set", "--mode", "plan"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "Pi Agent does not support mode configuration",
        ));
}

#[test]
fn agent_provider_use_amp_rejects_provider_model_settings() {
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
    SecretStore::open_or_create(tempdir.path()).expect("secret store should open");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "agent",
            "provider",
            "use",
            "openai",
            "--model",
            "openai/gpt-5.5",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "Amp Code does not support mapped provider selection",
        ));
}

#[test]
fn agent_install_registry_path_prepares_workspace_root_without_secret_store() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let workspace_root = tempdir.path().join("workspace");
    let binary_path = tempdir
        .path()
        .join(".local")
        .join("bin")
        .join("cli-registry-agent");
    let script = format!(
        "test \"$(pwd -P)\" = \"$(cd {workspace} && pwd -P)\" && mkdir -p {bin} && printf '#!/bin/sh\\n' > {binary} && chmod 755 {binary}",
        workspace = shell_quote_path(&workspace_root),
        bin = shell_quote_path(binary_path.parent().expect("binary has parent")),
        binary = shell_quote_path(&binary_path),
    );
    let config = VALID_CONFIG
        .replace(
            r#"command = "opencode""#,
            r#"command = "cli-registry-agent""#,
        )
        .replace(
            r#"root = "/workspace""#,
            &format!(r#"root = "{}""#, workspace_root.display()),
        )
        .replace(
            r#"uploads = "/workspace/uploads""#,
            &format!(r#"uploads = "{}/uploads""#, workspace_root.display()),
        )
        .replace(r#"args = ["acp"]"#, "args = []")
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
    seed_auth_verifiers(tempdir.path(), SESSION_KEY, ADMIN_KEY);
    fs::write(
        config_dir.join("agents.toml"),
        format!(
            r#"
[[agents]]
id = "opencode"
name = "OpenCode Test"
kind = "native"
headless_compatible = true
support_doc = "docs/agents/opencode.md"

[agents.harness]
id = "opencode"

[agents.harness.install.shell]
script = {script:?}
creates = "cli-registry-agent"
"#
        ),
    )
    .expect("registry should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["agent", "install", "--yes", "--admin-key", ADMIN_KEY])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "progress: preparing agent install",
        ))
        .stdout(predicates::str::contains(
            "progress: resolving agent install plan",
        ))
        .stdout(predicates::str::contains(
            "progress: installing resolved agent artifacts",
        ))
        .stdout(predicates::str::contains("agent install: installed"))
        .stdout(predicates::str::contains(
            binary_path.to_string_lossy().as_ref(),
        ));

    assert!(workspace_root.is_dir());
    assert!(workspace_root.join("uploads").is_dir());
}

fn goose_config() -> String {
    VALID_CONFIG
        .replace(r#"id = "opencode""#, r#"id = "goose""#)
        .replace(r#"name = "OpenCode""#, r#"name = "Goose""#)
        .replace(r#"command = "opencode""#, r#"command = "goose""#)
        .replace(
            r#"env = ["OPENCODE_API_KEY"]"#,
            r#"env = ["OPENROUTER_API_KEY"]"#,
        )
        .replace(
            r#"
[agent.provider]
id = "opencode-go"
model = "opencode-go/deepseek-v4-flash"
api_key_ref = "OPENCODE_API_KEY"
"#,
            r#"
[agent.provider]
id = "openrouter"
model = "deepseek/deepseek-v4-flash"
api_key_ref = "OPENROUTER_API_KEY"
"#,
        )
        .replace(
            r#"
[agent.install]
type = "shell"
shell = "curl -fsSL https://opencode.ai/install | bash"
creates = "opencode"
"#,
            "",
        )
}

fn claude_code_config() -> String {
    VALID_CONFIG
        .replace(r#"id = "opencode""#, r#"id = "claude-code""#)
        .replace(r#"name = "OpenCode""#, r#"name = "Claude Code""#)
        .replace(r#"command = "opencode""#, r#"command = "claude-agent-acp""#)
        .replace(r#"args = ["acp"]"#, r#"args = []"#)
        .replace(r#"env = ["OPENCODE_API_KEY"]"#, r#"env = []"#)
        .replace(
            r#"
[agent.install]
type = "shell"
shell = "curl -fsSL https://opencode.ai/install | bash"
creates = "opencode"
"#,
            "",
        )
}

fn kimi_config() -> String {
    VALID_CONFIG
        .replace(r#"id = "opencode""#, r#"id = "kimi""#)
        .replace(r#"name = "OpenCode""#, r#"name = "Kimi Code""#)
        .replace(r#"command = "opencode""#, r#"command = "kimi""#)
        .replace(r#"env = ["OPENCODE_API_KEY"]"#, r#"env = ["KIMI_API_KEY"]"#)
        .replace(
            r#"
[agent.install]
type = "shell"
shell = "curl -fsSL https://opencode.ai/install | bash"
creates = "opencode"
"#,
            "",
        )
}

#[test]
fn agent_provider_use_codex_openrouter_accepts_custom_model_without_discovery() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), codex_config())
        .expect("config should be written");
    seed_provider_credential(tempdir.path(), "openrouter", &["OPENROUTER_API_KEY"]);

    // No ACP config-options fixture: codex takes the model verbatim for
    // OpenRouter, so the command must succeed without spawning the agent.
    // The dead-port base keeps the best-effort catalog refresh from ever
    // reaching live `openrouter.ai`.
    acps_command()
        .env("HOME", tempdir.path())
        .env("ACP_STACK_PROVIDER_MODELS_BASE", "http://127.0.0.1:1")
        .args([
            "agent",
            "provider",
            "use",
            "openrouter",
            "--model",
            "deepseek/deepseek-v4-flash",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "model: deepseek/deepseek-v4-flash",
        ));

    let codex_path = tempdir.path().join(".codex").join("config.toml");
    let codex: toml::Value =
        toml::from_str(&fs::read_to_string(codex_path).expect("codex config should be readable"))
            .expect("codex config should parse");
    assert_eq!(codex["model"].as_str(), Some("deepseek/deepseek-v4-flash"));
    assert_eq!(codex["model_provider"].as_str(), Some("openrouter"));
}

#[test]
fn claude_code_provider_use_writes_available_models_from_live_catalog() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), claude_code_config())
        .expect("config should be written");
    seed_provider_credential(tempdir.path(), "moonshotai", &["MOONSHOT_API_KEY"]);
    let base = spawn_provider_models_server(json!({
        "data": [
            { "id": "kimi-k3", "name": "Kimi K3" },
            { "id": "kimi-k3[1m]" },
            { "id": "kimi-k2.7-code" },
        ]
    }));

    acps_command()
        .env("HOME", tempdir.path())
        .env("ACP_STACK_PROVIDER_MODELS_BASE", &base)
        .args([
            "agent",
            "provider",
            "use",
            "moonshotai",
            "--model",
            "kimi-k3",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("model: kimi-k3"));

    let settings = claude_settings(tempdir.path());
    assert_eq!(
        settings["availableModels"],
        json!(["kimi-k3", "kimi-k3[1m]", "kimi-k2.7-code"])
    );
    assert_eq!(settings["env"]["ANTHROPIC_MODEL"].as_str(), Some("kimi-k3"));

    let cache_path = tempdir
        .path()
        .join(".config/acp-stack/provider-models.json");
    let cache: Value = serde_json::from_str(
        &fs::read_to_string(cache_path).expect("provider model cache should be readable"),
    )
    .expect("provider model cache parses");
    assert_eq!(
        cache["providers"]["moonshotai"]["models"][0]["value"],
        "kimi-k3"
    );
}

#[test]
fn claude_code_provider_use_succeeds_and_omits_available_models_when_catalog_offline() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), claude_code_config())
        .expect("config should be written");
    seed_provider_credential(tempdir.path(), "moonshotai", &["MOONSHOT_API_KEY"]);

    // Dead endpoint: the fetch must degrade to a warning, never fail the
    // command or leave a stale availableModels list behind.
    acps_command()
        .env("HOME", tempdir.path())
        .env("ACP_STACK_PROVIDER_MODELS_BASE", "http://127.0.0.1:1")
        .args([
            "agent",
            "provider",
            "use",
            "moonshotai",
            "--model",
            "kimi-k3",
        ])
        .assert()
        .success();

    let settings = claude_settings(tempdir.path());
    assert!(settings.get("availableModels").is_none());
    assert_eq!(settings["env"]["ANTHROPIC_MODEL"].as_str(), Some("kimi-k3"));
}
