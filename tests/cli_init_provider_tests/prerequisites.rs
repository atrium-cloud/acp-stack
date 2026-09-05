use std::fs;

use acp_stack::config::load_config_from_str;
use acp_stack::dev_gates::TEST_SKIP_AGENT_INSTALL_ENV;
use predicates::prelude::PredicateBooleanExt as _;
use serde_json::Value;

use crate::common::cli::*;

#[test]
fn init_skips_opencode_config_without_configured_provider() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");

    acps_command(tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .success()
        .stdout(predicates::str::contains("OpenCode config:").not());

    let opencode_path = tempdir
        .path()
        .join(".config")
        .join("opencode")
        .join("opencode.json");
    assert!(!opencode_path.exists());
}

#[test]
fn init_provider_sets_opencode_auth_config_without_model() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    seed_init_secrets(tempdir.path(), &[("OPENAI_API_KEY", "test-openai-key")]);

    acps_command(tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "opencode",
            "--provider",
            "openai",
            "--api-key-ref",
            "OPENAI_API_KEY",
            "--skip-workspace-init",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("OpenCode config:"));

    let config = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    assert!(config.contains("[array.targets.agent.provider]"));
    assert!(config.contains(r#"id = "openai""#));
    assert!(config.contains(r#"api_key_ref = "OPENAI_API_KEY""#));
    assert!(!config.contains(r#"model ="#));
    assert!(config.contains(r#"env = ["OPENAI_API_KEY"]"#));
    assert!(!config.contains(r#""OPENCODE_API_KEY""#));

    let opencode_path = tempdir
        .path()
        .join(".config")
        .join("opencode")
        .join("opencode.json");
    let opencode: Value = serde_json::from_str(
        &fs::read_to_string(opencode_path).expect("opencode config should be readable"),
    )
    .expect("opencode config should parse");
    assert!(opencode.get("model").is_none());
    assert_eq!(
        opencode["provider"]["openai"]["options"]["apiKey"],
        "{env:OPENAI_API_KEY}"
    );
}

#[test]
fn init_provider_fails_noninteractive_when_default_secret_is_missing() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    let output = acps_command(tempdir.path())
        .env_remove(TEST_SKIP_AGENT_INSTALL_ENV)
        .args([
            "dev",
            "init",
            "--agent",
            "opencode",
            "--provider",
            "openai",
            "--skip-workspace-init",
        ])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let stderr = String::from_utf8(output).expect("stderr should be utf8");
    assert!(
        stderr.contains("secret `OPENAI_API_KEY` was not found in the secret store"),
        "{stderr}"
    );
    let run_id = stderr
        .lines()
        .find_map(|line| line.strip_prefix("init failed in run "))
        .expect("stderr should include failed init run id");
    assert!(
        stderr.contains("failed step: provider_configure"),
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!("retry: acps init --resume --run-id {run_id}")),
        "{stderr}"
    );
}

#[test]
fn init_existing_provider_requires_secret_before_model_discovery() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    seed_init_secrets(tempdir.path(), &[]);
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = format!(
        "{}\n\n[agent.provider]\nid = \"openai\"\napi_key_ref = \"OPENAI_API_KEY\"\n",
        VALID_CONFIG.replace(r#"env = ["OPENCODE_API_KEY"]"#, "env = []")
    );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    acps_command(tempdir.path())
        .args([
            "dev",
            "init",
            "--model",
            "openai/gpt-5.5",
            "--skip-workspace-init",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "secret `OPENAI_API_KEY` was not found in the secret store",
        ))
        .stderr(predicates::str::contains("failed step: provider_configure"));
}

#[test]
fn init_existing_provider_requires_secret_without_model_flag() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    seed_init_secrets(tempdir.path(), &[]);
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = format!(
        "{}\n\n[agent.provider]\nid = \"openai\"\napi_key_ref = \"OPENAI_API_KEY\"\n",
        VALID_CONFIG.replace(r#"env = ["OPENCODE_API_KEY"]"#, "env = []")
    );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    acps_command(tempdir.path())
        .args(["dev", "init", "--skip-workspace-init"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "secret `OPENAI_API_KEY` was not found in the secret store",
        ))
        .stderr(predicates::str::contains("failed step: provider_configure"));
}

#[test]
fn init_existing_provider_repairs_env_before_model_discovery() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    seed_init_secrets(tempdir.path(), &[("OPENAI_API_KEY", "test-openai-key")]);
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = format!(
        "{}\n\n[agent.provider]\nid = \"openai\"\napi_key_ref = \"OPENAI_API_KEY\"\n",
        VALID_CONFIG.replace(r#"env = ["OPENCODE_API_KEY"]"#, "env = []")
    );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");
    let options_path = write_acp_config_options(tempdir.path(), &["openai/gpt-5.5"], &[]);

    acps_command(tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args([
            "dev",
            "init",
            "--model",
            "openai/gpt-5.5",
            "--skip-workspace-init",
        ])
        .assert()
        .success();

    let config =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config should be readable");
    assert!(config.contains(r#"env = ["OPENAI_API_KEY"]"#));
    assert!(config.contains(r#"model = "openai/gpt-5.5""#));
}

#[test]
fn init_existing_provider_fills_default_api_key_ref_before_model_discovery() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    seed_init_secrets(tempdir.path(), &[("OPENAI_API_KEY", "test-openai-key")]);
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = format!(
        "{}\n\n[agent.provider]\nid = \"openai\"\n",
        VALID_CONFIG.replace(r#"env = ["OPENCODE_API_KEY"]"#, "env = []")
    );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");
    let options_path = write_acp_config_options(tempdir.path(), &["openai/gpt-5.5"], &[]);

    acps_command(tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args([
            "dev",
            "init",
            "--model",
            "openai/gpt-5.5",
            "--skip-workspace-init",
        ])
        .assert()
        .success();

    let config =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config should be readable");
    assert!(config.contains(r#"env = ["OPENAI_API_KEY"]"#));
    assert!(config.contains(r#"api_key_ref = "OPENAI_API_KEY""#));
    assert!(config.contains(r#"model = "openai/gpt-5.5""#));
}

#[test]
fn init_rejects_imported_provider_that_agent_does_not_support() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    seed_init_secrets(tempdir.path(), &[("AMP_API_KEY", "test-amp-key")]);
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = format!(
        "{}\n\n[agent.provider]\nid = \"amp-code\"\napi_key_ref = \"AMP_API_KEY\"\n",
        VALID_CONFIG.replace(r#"env = ["OPENCODE_API_KEY"]"#, "env = []")
    );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    acps_command(tempdir.path())
        .args(["dev", "init", "--skip-workspace-init"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "provider `amp-code` is not supported for agent `opencode`",
        ))
        .stderr(predicates::str::contains("failed step: provider_configure"));
}

#[test]
fn init_rejects_stale_unsupported_provider_block_for_kimi() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    seed_init_secrets(tempdir.path(), &[]);
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = format!(
        "{}\n\n[agent.provider]\nid = \"mistral\"\nmodel = \"mistral-large\"\napi_key_ref = \"MISTRAL_API_KEY\"\n",
        VALID_CONFIG
            .replace(r#"id = "opencode""#, r#"id = "kimi""#)
            .replace(r#"name = "OpenCode""#, r#"name = "Kimi Code""#)
            .replace(r#"command = "opencode""#, r#"command = "kimi""#)
            .replace(r#"env = ["OPENCODE_API_KEY"]"#, "env = []")
    );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    acps_command(tempdir.path())
        .args(["dev", "init", "--skip-workspace-init"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "provider `mistral` is not supported for agent `kimi`",
        ));
}

#[test]
fn init_skips_stale_provider_block_when_agent_cannot_set_provider() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    seed_init_secrets(tempdir.path(), &[]);
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = format!(
        "{}\n\n[agent.provider]\nid = \"openai\"\nmodel = \"openai/gpt-5.5\"\napi_key_ref = \"OPENAI_API_KEY\"\n",
        VALID_CONFIG
            .replace(r#"id = "opencode""#, r#"id = "amp""#)
            .replace(r#"name = "OpenCode""#, r#"name = "Amp Code""#)
            .replace(r#"command = "opencode""#, r#"command = "amp-acp""#)
            .replace(r#"args = ["acp"]"#, r#"args = []"#)
            .replace(r#"env = ["OPENCODE_API_KEY"]"#, "env = []")
    );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    acps_command(tempdir.path())
        .args(["dev", "init", "--skip-workspace-init"])
        .assert()
        .success();

    let config =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config should be readable");
    let config = load_config_from_str(&config).expect("config should parse");
    assert_eq!(config.agent.id, "amp");
    assert!(config.agent.provider.is_none());
    assert!(
        !config.agent.env.iter().any(|name| name == "OPENAI_API_KEY"),
        "provider setup must not repair env for agents that cannot set provider"
    );
}

#[test]
fn init_provider_succeeds_noninteractive_when_default_secret_exists() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    seed_init_secrets(tempdir.path(), &[("OPENAI_API_KEY", "test-openai-key")]);

    acps_command(tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "opencode",
            "--provider",
            "openai",
            "--skip-workspace-init",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("OpenCode config:"));

    let config = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    assert!(config.contains(r#"api_key_ref = "OPENAI_API_KEY""#));
    assert!(config.contains(r#"env = ["OPENAI_API_KEY"]"#));
}
