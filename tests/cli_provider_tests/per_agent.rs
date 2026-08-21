use std::fs;

use crate::common::cli::*;

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
fn agent_set_kilo_accepts_advertised_model_and_keeps_env_scoped_auth() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), kilo_config())
        .expect("config should be written");
    seed_init_secrets(tempdir.path(), &[("KILO_API_KEY", "test-kilo-key")]);
    let options_path = write_acp_config_options(tempdir.path(), &["kilo/auto"], &[]);

    acps_command()
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args(["agent", "set", "--model", "kilo/auto"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent: kilo"))
        .stdout(predicates::str::contains("model: kilo/auto"))
        .stdout(predicates::str::contains("required_env_refs: KILO_API_KEY"))
        .stdout(predicates::str::contains(
            "model and mode take effect on new sessions via ACP session/set_config_option",
        ));

    let config = fs::read_to_string(config_dir.join("acps-config.toml")).expect("config readable");
    assert!(config.contains(r#"env = ["KILO_API_KEY"]"#));
    assert!(config.contains(r#"model = "kilo/auto""#));
    assert!(!config.contains("test-kilo-key"));
    assert!(!config.contains("[agent.provider]"));
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
fn agent_set_hermes_accepts_exact_model_and_updates_hermes_yaml() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), hermes_config())
        .expect("config should be written");
    seed_init_secrets(
        tempdir.path(),
        &[("OPENROUTER_API_KEY", "test-openrouter-key")],
    );

    // No ACP config-options fixture: Hermes advertises only the pre-1.0
    // models/modes session state, so the model is taken verbatim and the
    // command must succeed without spawning the agent.
    acps_command()
        .env("HOME", tempdir.path())
        .args(["agent", "set", "--model", "z-ai/glm-5.1"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent: hermes"))
        .stdout(predicates::str::contains("model: z-ai/glm-5.1"));

    let config = fs::read_to_string(config_dir.join("acps-config.toml")).expect("config readable");
    assert!(config.contains(r#"model = "z-ai/glm-5.1""#));
    assert!(!config.contains("test-openrouter-key"));

    let hermes_yaml = fs::read_to_string(tempdir.path().join(".hermes/config.yaml"))
        .expect("hermes config should be readable");
    let hermes: serde_norway::Value =
        serde_norway::from_str(&hermes_yaml).expect("hermes config parses");
    assert_eq!(hermes["model"]["provider"], "openrouter");
    assert_eq!(hermes["model"]["default"], "z-ai/glm-5.1");
    assert!(!hermes_yaml.contains("test-openrouter-key"));
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

fn kilo_config() -> String {
    VALID_CONFIG
        .replace(r#"id = "opencode""#, r#"id = "kilo""#)
        .replace(r#"name = "OpenCode""#, r#"name = "Kilo Code""#)
        .replace(r#"command = "opencode""#, r#"command = "kilo""#)
        .replace(r#"env = ["OPENCODE_API_KEY"]"#, r#"env = ["KILO_API_KEY"]"#)
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

fn hermes_config() -> String {
    let config = VALID_CONFIG
        .replace(r#"id = "opencode""#, r#"id = "hermes""#)
        .replace(r#"name = "OpenCode""#, r#"name = "Hermes Agent""#)
        .replace(r#"command = "opencode""#, r#"command = "hermes""#)
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
    format!(
        "{config}\n\n[agent.provider]\nid = \"openrouter\"\nmodel = \"deepseek/deepseek-v4-flash\"\napi_key_ref = \"OPENROUTER_API_KEY\"\n"
    )
}
