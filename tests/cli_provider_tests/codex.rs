use acp_stack::secrets::SecretStore;
use std::fs;

use crate::common::cli::*;

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
