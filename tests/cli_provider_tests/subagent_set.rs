use predicates::prelude::PredicateBooleanExt as _;
use serde_json::{Value, json};
use std::fs;

use crate::common::cli::*;

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

    acps_command(tempdir.path())
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

    acps_command(tempdir.path())
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

    acps_command(tempdir.path())
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

    acps_command(tempdir.path())
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

    acps_command(tempdir.path())
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

    acps_command(tempdir.path())
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

        acps_command(tempdir.path())
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

    acps_command(tempdir.path())
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

    acps_command(tempdir.path())
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

    acps_command(tempdir.path())
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

    acps_command(tempdir.path())
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
        .replace(r#"id = "opencode""#, r#"id = "kimi""#)
        .replace(r#"name = "OpenCode""#, r#"name = "Kimi Code""#)
        .replace(r#"command = "opencode""#, r#"command = "kimi""#);
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    acps_command(tempdir.path())
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

        acps_command(tempdir.path())
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

        acps_command(tempdir.path())
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

    acps_command(tempdir.path())
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
