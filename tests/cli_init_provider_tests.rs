#![cfg(all(feature = "dev-tools", feature = "test-fixtures"))]

use acp_stack::config::load_config_from_str;
use acp_stack::dev_gates::TEST_SKIP_AGENT_INSTALL_ENV;
use acp_stack::state::{InitStepRecord, StateStore, default_state_path};
use predicates::prelude::PredicateBooleanExt as _;
use serde_json::Value;
use std::fs;

mod common;
use common::agent::spawn_provider_models_server;
use common::cli::*;

#[test]
fn init_skips_opencode_config_without_configured_provider() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
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

    acps_command()
        .env("HOME", tempdir.path())
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

    let output = acps_command()
        .env_remove(TEST_SKIP_AGENT_INSTALL_ENV)
        .env("HOME", tempdir.path())
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

    acps_command()
        .env("HOME", tempdir.path())
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

    acps_command()
        .env("HOME", tempdir.path())
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

    acps_command()
        .env("HOME", tempdir.path())
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

    acps_command()
        .env("HOME", tempdir.path())
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
    seed_init_secrets(tempdir.path(), &[("CURSOR_API_KEY", "test-cursor-key")]);
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = format!(
        "{}\n\n[agent.provider]\nid = \"cursor\"\napi_key_ref = \"CURSOR_API_KEY\"\n",
        VALID_CONFIG.replace(r#"env = ["OPENCODE_API_KEY"]"#, "env = []")
    );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--skip-workspace-init"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "provider `cursor` is not supported for agent `opencode`",
        ))
        .stderr(predicates::str::contains("failed step: provider_configure"));
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
            .replace(r#"id = "opencode""#, r#"id = "cursor""#)
            .replace(r#"name = "OpenCode""#, r#"name = "Cursor CLI""#)
            .replace(r#"command = "opencode""#, r#"command = "cursor-agent""#)
            .replace(r#"env = ["OPENCODE_API_KEY"]"#, "env = []")
    );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");
    let options_path = write_acp_config_options(tempdir.path(), &["cursor/gpt-5.5"], &[]);

    acps_command()
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args([
            "dev",
            "init",
            "--model",
            "cursor/gpt-5.5",
            "--skip-workspace-init",
        ])
        .assert()
        .success();

    let config =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config should be readable");
    let config = load_config_from_str(&config).expect("config should parse");
    assert_eq!(config.agent.id, "cursor");
    assert_eq!(config.agent.model.as_deref(), Some("cursor/gpt-5.5"));
    assert!(config.agent.provider.is_none());
    assert!(
        !config.agent.env.iter().any(|name| name == "OPENAI_API_KEY"),
        "provider setup must not repair env for agents that cannot set provider"
    );
}

#[test]
fn init_resume_restores_recorded_edge_request_before_edge_step_exists() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    let output = acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "opencode",
            "--provider",
            "openai",
            "--edge",
            "cloudflare",
            "--exposure",
            "tunnel",
            "--hostname",
            "agent.example.com",
            "--cloudflared-deployment",
            "external",
            "--no-skills",
            "--skip-workspace-init",
        ])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let stderr = String::from_utf8(output).expect("stderr should be utf8");
    let run_id = stderr
        .lines()
        .find_map(|line| line.strip_prefix("init failed in run "))
        .expect("stderr should include failed init run id");

    seed_init_secrets(tempdir.path(), &[("OPENAI_API_KEY", "test-openai-key")]);

    acps_command()
        .env("HOME", tempdir.path())
        .args(["init", "--resume", "--run-id", run_id])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "progress: preparing Cloudflare edge artifacts",
        ))
        .stdout(predicates::str::contains(
            "workspace: skipped (--skip-workspace-init)",
        ))
        .stdout(predicates::str::contains("progress: materializing workspace sources").not());

    assert!(
        tempdir
            .path()
            .join(".config/acp-stack/cloudflared/config.yml")
            .is_file()
    );
    assert!(!tempdir.path().join("workspace").exists());
}

#[test]
fn init_resume_with_nothing_to_resume_writes_no_placeholder_config() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["init", "--resume"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("no resumable init run found"));

    assert!(
        !tempdir
            .path()
            .join(".config/acp-stack/acps-config.toml")
            .exists(),
        "a failed --resume must not leave a starter config on disk"
    );
}

#[test]
fn init_resume_restores_recorded_provider_args_before_provider_step_exists() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir");
    let workspace = tempdir.path().join("workspace");
    fs::create_dir_all(&workspace).expect("workspace");
    let local_bin = tempdir.path().join(".local/bin");
    let managed_opencode = local_bin.join("opencode");
    fs::write(
        config_dir.join("agents.toml"),
        format!(
            r#"
[[agents]]
id = "opencode"
name = "OpenCode"
kind = "native"
headless_compatible = true
set_provider = true
set_model = true
allow_custom_provider = true
allow_custom_model = true
set_mode = true
support_doc = "docs/agents/opencode.md"

[agents.harness]
id = "opencode"

[agents.harness.install.shell]
script = "exit 9"
creates = {}
"#,
            toml_string(&managed_opencode.to_string_lossy()),
        ),
    )
    .expect("agents override");

    let output = acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "opencode",
            "--provider",
            "myprovider",
            "--custom-provider",
            "--provider-name",
            "My Provider",
            "--base-url",
            "https://api.myprovider.example/v1",
            "--api-key-ref",
            "MY_PROVIDER_API_KEY",
            "--model",
            "my-model",
            "--model-name",
            "My Model",
            "--workspace-root",
            workspace.to_str().expect("workspace UTF-8"),
            "--no-skills",
            "--skip-workspace-init",
            "--skip-testflight",
        ])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let stderr = String::from_utf8(output).expect("stderr should be utf8");
    let run_id = stderr
        .lines()
        .find_map(|line| line.strip_prefix("init failed in run "))
        .expect("stderr should include failed init run id");
    let config_before =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config should be readable");
    assert!(!config_before.contains("[array.targets.agent.provider]"));

    fs::write(
        config_dir.join("agents.toml"),
        format!(
            r#"
[[agents]]
id = "opencode"
name = "OpenCode"
kind = "native"
headless_compatible = true
set_provider = true
set_model = true
allow_custom_provider = true
allow_custom_model = true
set_mode = true
support_doc = "docs/agents/opencode.md"

[agents.harness]
id = {}

[agents.harness.install.shell]
script = "true"
creates = "opencode"
"#,
            toml_string(env!("CARGO_BIN_EXE_placebo-agent")),
        ),
    )
    .expect("agents override");
    seed_init_secrets(
        tempdir.path(),
        &[("MY_PROVIDER_API_KEY", "test-provider-key")],
    );

    acps_command()
        .env("HOME", tempdir.path())
        .args(["init", "--resume", "--run-id", run_id])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "workspace: skipped (--skip-workspace-init)",
        ));

    let config_after =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config should be readable");
    assert!(config_after.contains("[array.targets.agent.provider]"));
    assert!(config_after.contains(r#"id = "myprovider""#));
    assert!(config_after.contains("[array.targets.agent.provider.custom]"));
    assert!(config_after.contains(r#"name = "My Provider""#));
    assert!(config_after.contains(r#"api_key_ref = "MY_PROVIDER_API_KEY""#));
    assert!(config_after.contains(r#"base_url = "https://api.myprovider.example/v1""#));
    assert!(config_after.contains(r#"model_name = "My Model""#));
}

#[test]
fn init_resume_restores_recorded_skip_testflight_before_testflight_step_exists() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    let output = acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "opencode",
            "--provider",
            "openai",
            "--no-skills",
            "--skip-workspace-init",
            "--skip-testflight",
        ])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let stderr = String::from_utf8(output).expect("stderr should be utf8");
    let run_id = stderr
        .lines()
        .find_map(|line| line.strip_prefix("init failed in run "))
        .expect("stderr should include failed init run id");

    seed_init_secrets(tempdir.path(), &[("OPENAI_API_KEY", "test-openai-key")]);

    acps_command()
        .env("HOME", tempdir.path())
        .args(["init", "--resume", "--run-id", run_id])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "testflight: skipped (--skip-testflight)",
        ))
        .stdout(
            predicates::str::contains(
                "testflight: skipped (non-interactive run; pass --testflight to opt in)",
            )
            .not(),
        );
}

#[test]
fn init_resume_restores_recorded_testflight_before_testflight_step_exists() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    let output = acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "opencode",
            "--provider",
            "openai",
            "--no-skills",
            "--skip-workspace-init",
            "--testflight",
        ])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let stderr = String::from_utf8(output).expect("stderr should be utf8");
    let run_id = stderr
        .lines()
        .find_map(|line| line.strip_prefix("init failed in run "))
        .expect("stderr should include failed init run id");

    seed_init_secrets(tempdir.path(), &[("OPENAI_API_KEY", "test-openai-key")]);

    let output = acps_command()
        .env("HOME", tempdir.path())
        .args(["init", "--resume", "--run-id", run_id])
        .assert()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("stdout should be utf8");
    assert!(
        stdout.contains("this may consume provider credits."),
        "{stdout}"
    );
    assert!(
        !stdout.contains("testflight: skipped (non-interactive run; pass --testflight to opt in)"),
        "{stdout}"
    );
}

#[test]
fn init_provider_succeeds_noninteractive_when_default_secret_exists() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    seed_init_secrets(tempdir.path(), &[("OPENAI_API_KEY", "test-openai-key")]);

    acps_command()
        .env("HOME", tempdir.path())
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

#[test]
fn init_custom_opencode_provider_writes_generated_config() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    seed_init_secrets(tempdir.path(), &[("CUSTOM_API_KEY", "test-custom-key")]);

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "opencode",
            "--provider",
            "myprovider",
            "--custom-provider",
            "--provider-name",
            "My Provider",
            "--base-url",
            "https://api.myprovider.example/v1",
            "--api-key-ref",
            "CUSTOM_API_KEY",
            "--model",
            "my-model",
            "--skip-workspace-init",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("OpenCode config:"));

    let config = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    assert!(config.contains(r#"id = "myprovider""#));
    assert!(config.contains(r#"api_key_ref = "CUSTOM_API_KEY""#));
    assert!(config.contains("[array.targets.agent.provider.custom]"));
    assert!(config.contains(r#"api = "chat-completions""#));
    assert!(config.contains(r#"env = ["CUSTOM_API_KEY"]"#));

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
}

#[test]
fn init_custom_provider_succeeds_with_catalog_only_credential() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    seed_init_secrets(tempdir.path(), &[]);
    seed_catalog_only_provider_credential(
        tempdir.path(),
        "myprovider",
        &[("CUSTOM_API_KEY", "catalog-secret")],
    );

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "opencode",
            "--provider",
            "myprovider",
            "--custom-provider",
            "--provider-name",
            "My Provider",
            "--base-url",
            "https://api.myprovider.example/v1",
            "--api-key-ref",
            "CUSTOM_API_KEY",
            "--model",
            "my-model",
            "--skip-workspace-init",
        ])
        .assert()
        .success();

    // Lockstep guarantee: the init gate passed off the catalog, so spawn-time
    // resolution must inject the same credential.
    let config = load_config_from_str(
        &fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
            .expect("config should be readable"),
    )
    .expect("config should parse");
    let store =
        acp_stack::secrets::SecretStore::open(tempdir.path()).expect("secret store should reopen");
    let resolved =
        acp_stack::runtime::agent::provider_keys::resolve_agent_environment(&config, &store)
            .expect("spawn environment should resolve from the catalog");
    assert_eq!(resolved.env["CUSTOM_API_KEY"], "catalog-secret");
    let snapshot = resolved
        .providers
        .iter()
        .find(|provider| provider.provider_id == "myprovider")
        .expect("custom provider snapshot present");
    assert!(snapshot.revision.is_some());
}

/// Registry ids are reserved instance-wide: every runtime and apply site
/// classifies by registry membership before it looks at `custom`, so a custom
/// declaration under a registry id resolves down the mapped path and fails at
/// spawn. Codex-with-Anthropic must use a distinct id such as `anthropic-1`.
#[test]
fn init_custom_codex_provider_rejects_known_mapped_provider_id() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    seed_init_secrets(
        tempdir.path(),
        &[("ANTHROPIC_API_KEY", "test-anthropic-key")],
    );

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "codex",
            "--provider",
            "anthropic",
            "--custom-provider",
            "--provider-name",
            "Anthropic Custom",
            "--base-url",
            "https://api.anthropic.example/v1",
            "--api-key-ref",
            "ANTHROPIC_API_KEY",
            "--model",
            "claude-custom",
            "--skip-workspace-init",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "reserved by the mapped-provider registry",
        ))
        .stderr(predicates::str::contains("anthropic-1"));

    // The starter config is written before the provider step, so the file may
    // exist; what must not exist is the rejected custom provider inside it.
    let config_path = tempdir.path().join(".config/acp-stack/acps-config.toml");
    if config_path.exists() {
        let config = fs::read_to_string(&config_path).expect("config should be readable");
        assert!(
            !config.contains("provider.custom"),
            "a rejected custom provider must not be written: {config}"
        );
        assert!(
            !config.contains(r#"id = "anthropic""#),
            "a rejected custom provider must not be written: {config}"
        );
    }
}

// L84-L87 cover the provisional ACP discovery flow during init: validate
// explicit `--model` against the harness's advertised values (L86) and
// surface the list when non-interactive callers omit `--model` (L87). The
// fixture env var short-circuits the actual spawn so these tests don't depend
// on a real opencode binary being installed.
fn write_workspace_init_config(home: &std::path::Path) {
    let config_dir = home.join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir");
    let workspace = home.join("workspace");
    fs::create_dir_all(&workspace).expect("workspace dir");
    let config = VALID_CONFIG
        .replace(
            r#"root = "/workspace""#,
            &format!(r#"root = "{}""#, workspace.display()),
        )
        .replace(
            r#"uploads = "/workspace/uploads""#,
            &format!(r#"uploads = "{}/uploads""#, workspace.display()),
        )
        .replace(
            r#"cwd = "/workspace""#,
            &format!(r#"cwd = "{}""#, workspace.display()),
        )
        .replace(r#"command = "opencode""#, r#"command = "/bin/true""#);
    fs::write(config_dir.join("acps-config.toml"), config).expect("config");
}

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

#[test]
fn init_rejects_mode_flag_for_agents_without_set_mode() {
    // pi/goose/hermes declare set_mode=false; `--mode` must fail fast as a
    // capability check rather than being silently dropped.
    for agent in ["pi", "goose", "hermes"] {
        let tempdir = tempfile::tempdir().expect("tempdir");

        acps_command()
            .env("HOME", tempdir.path())
            .args(["init", "--agent", agent, "--mode", "plan"])
            .assert()
            .failure()
            .stderr(predicates::str::contains(
                "does not support mode configuration through `acps init`",
            ));
    }
}

#[test]
fn init_rejects_mode_flag_for_custom_agents() {
    let tempdir = tempfile::tempdir().expect("tempdir");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "init",
            "--custom-agent-id",
            "bespoke",
            "--custom-agent-command",
            "bespoke",
            "--custom-agent-install",
            "true",
            "--mode",
            "plan",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "cannot be used with '--mode <MODE>'",
        ));
}

#[test]
fn init_model_setup_does_not_write_agent_mode() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    write_workspace_init_config(tempdir.path());
    seed_init_secrets(tempdir.path(), &[("OPENAI_API_KEY", "test-openai-key")]);
    let options_path =
        write_acp_config_options(tempdir.path(), &["openai/gpt-5.5"], &["build", "plan"]);

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
        .success()
        .stdout(predicates::str::contains("advertised modes").not());

    let config = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    assert!(!config.contains(r#"mode = "plan""#));
    assert!(!config.contains(r#"mode = "build""#));
    assert!(provider_configure_payload(tempdir.path()).contains(r#""mode_action":"Skipped""#));
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
fn init_explicit_mode_writes_agent_mode() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    write_workspace_init_config(tempdir.path());
    seed_init_secrets(tempdir.path(), &[("OPENAI_API_KEY", "test-openai-key")]);
    let options_path =
        write_acp_config_options(tempdir.path(), &["openai/gpt-5.5"], &["build", "plan"]);

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
            "--mode",
            "plan",
        ])
        .assert()
        .success();

    let config = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    assert!(config.contains(r#"mode = "plan""#));
    assert!(config.contains(r#"model = "openai/gpt-5.5""#));
    let payload = provider_configure_payload(tempdir.path());
    assert!(
        payload.contains(r#""mode_action":"Set""#),
        "payload {payload}"
    );
    assert!(
        payload.contains(r#""model_action":"Set""#),
        "payload {payload}"
    );
}

#[test]
fn init_rejects_mode_not_advertised_by_the_agent() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    write_workspace_init_config(tempdir.path());
    seed_init_secrets(tempdir.path(), &[("OPENAI_API_KEY", "test-openai-key")]);
    let options_path =
        write_acp_config_options(tempdir.path(), &["openai/gpt-5.5"], &["build", "plan"]);

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
            "--mode",
            "bogus",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("advertised modes: [build, plan]"));

    let config_text = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    let config = load_config_from_str(&config_text).expect("canonical config parses");
    assert!(config.agent.mode.is_none());
}

#[test]
fn init_mode_lane_runs_for_an_agent_without_model_support() {
    // amp declares set_model=false/set_mode=true: the mode lane must be
    // reachable even though the model lane returns before any discovery.
    let tempdir = tempfile::tempdir().expect("tempdir");
    write_workspace_init_config(tempdir.path());
    seed_init_secrets(tempdir.path(), &[("AMP_API_KEY", "test-amp-key")]);
    let options_path = write_acp_config_options(tempdir.path(), &[], &["smart", "rush", "deep"]);

    acps_with_empty_path(tempdir.path())
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args(["init", "--agent", "amp", "--mode", "deep"])
        .assert()
        .success();

    let config_text = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    let config = load_config_from_str(&config_text).expect("canonical config parses");
    assert_eq!(config.agent.mode.as_deref(), Some("deep"));
    assert!(config.agent.model.is_none());
    assert!(config.agent.provider.is_none());
}

#[test]
fn init_resume_reapplies_a_recorded_mode_instead_of_replaying_provider_configure_as_skipped() {
    // amp has neither a provider nor a model lane, so `--mode` is the only
    // explicit flag the `provider_configure` verifier can see: without its
    // `args.mode.is_none()` term the prior succeeded row would replay as
    // skipped and the recorded mode would never be re-applied.
    let tempdir = tempfile::tempdir().expect("tempdir");
    write_workspace_init_config(tempdir.path());
    seed_init_secrets(tempdir.path(), &[("AMP_API_KEY", "test-amp-key")]);
    let options_path = write_acp_config_options(tempdir.path(), &[], &["smart", "rush", "deep"]);
    // Fail the run after `provider_configure`: the edge step cannot create its
    // artifact directory while a file sits at that path.
    let cloudflared_dir = tempdir.path().join(".config/acp-stack/cloudflared");
    fs::write(&cloudflared_dir, "not a directory").expect("edge path collision");

    let edge_args = [
        "--edge",
        "cloudflare",
        "--exposure",
        "tunnel",
        "--hostname",
        "agent.example.com",
        "--cloudflared-deployment",
        "external",
    ];
    let output = acps_with_empty_path(tempdir.path())
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args(["init", "--agent", "amp", "--mode", "deep"])
        .args(edge_args)
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let stderr = String::from_utf8(output).expect("stderr should be utf8");
    let run_id = stderr
        .lines()
        .find_map(|line| line.strip_prefix("init failed in run "))
        .expect("stderr should include failed init run id");
    assert_eq!(provider_configure_step(tempdir.path()).status, "succeeded");

    fs::remove_file(&cloudflared_dir).expect("clear the edge path collision");
    acps_with_empty_path(tempdir.path())
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args(["init", "--resume", "--run-id", run_id])
        .assert()
        .success();

    let step = provider_configure_step(tempdir.path());
    assert_eq!(
        step.status, "succeeded",
        "a resumed run carrying a recorded --mode must re-run provider_configure: {step:?}"
    );
    assert!(
        step.payload_json.contains(r#""mode_action":"Set""#),
        "payload {}",
        step.payload_json
    );
    let config_text = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    let config = load_config_from_str(&config_text).expect("canonical config parses");
    assert_eq!(config.agent.mode.as_deref(), Some("deep"));
}

#[test]
fn init_codex_openrouter_writes_both_explicit_model_and_mode() {
    // codex+openrouter takes `--model` verbatim without consulting the
    // advertised list; the mode lane must still reach the shared session.
    let tempdir = tempfile::tempdir().expect("tempdir");
    write_workspace_init_config(tempdir.path());
    seed_init_secrets(
        tempdir.path(),
        &[("OPENROUTER_API_KEY", "test-openrouter-key")],
    );
    let options_path = write_acp_config_options(
        tempdir.path(),
        &["gpt-5.5"],
        &["read-only", "auto", "full-access"],
    );

    acps_with_empty_path(tempdir.path())
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
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
            "--mode",
            "auto",
        ])
        .assert()
        .success();

    let config_text = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    let config = load_config_from_str(&config_text).expect("canonical config parses");
    assert_eq!(config.agent.mode.as_deref(), Some("auto"));
    let provider = config.agent.provider.as_ref().expect("provider configured");
    assert_eq!(
        provider.model.as_deref(),
        Some("deepseek/deepseek-v4-flash")
    );
}

#[test]
fn init_kimi_pins_its_model_once_and_still_reaches_the_mode_lane() {
    // Kimi's model is pinned without discovery; the pin must happen exactly
    // once and must not suppress the mode lane that follows it.
    let tempdir = tempfile::tempdir().expect("tempdir");
    write_workspace_init_config(tempdir.path());
    seed_init_secrets(tempdir.path(), &[("KIMI_API_KEY", "test-kimi-key")]);
    let options_path = write_acp_config_options(tempdir.path(), &[], &["default", "plan"]);

    acps_with_empty_path(tempdir.path())
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args(["init", "--agent", "kimi", "--mode", "plan"])
        .assert()
        .success();

    let config_text = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    assert_eq!(config_text.matches("model = ").count(), 1);
    let config = load_config_from_str(&config_text).expect("canonical config parses");
    assert_eq!(config.agent.model.as_deref(), Some("kimi-for-coding"));
    assert_eq!(config.agent.mode.as_deref(), Some("plan"));
}

#[test]
fn init_without_mode_flag_never_enters_the_mode_lane() {
    // An unattended run for a mode-capable agent with an unspawnable binary
    // must not attempt discovery on the mode lane's behalf: no failure, no
    // mode written, and the step payload records the skip.
    let tempdir = tempfile::tempdir().expect("tempdir");
    write_workspace_init_config(tempdir.path());
    seed_init_secrets(tempdir.path(), &[("AMP_API_KEY", "test-amp-key")]);

    acps_with_empty_path(tempdir.path())
        .env("HOME", tempdir.path())
        .args(["init", "--agent", "amp"])
        .assert()
        .success()
        .stdout(predicates::str::contains("discovery skipped").not());

    let config_text = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    let config = load_config_from_str(&config_text).expect("canonical config parses");
    assert!(config.agent.mode.is_none());
    assert!(provider_configure_payload(tempdir.path()).contains(r#""mode_action":"Skipped""#));
}

#[test]
fn init_provider_change_without_model_clears_stale_opencode_model() {
    // Pre-existing opencode.json with a stale model from a prior run.
    // An init that switches provider without picking a new model
    // (L87 path) must clear the stale model field so the launched
    // harness doesn't silently use it under the new provider.
    let tempdir = tempfile::tempdir().expect("tempdir");
    write_workspace_init_config(tempdir.path());
    seed_init_secrets(tempdir.path(), &[("OPENAI_API_KEY", "test")]);
    let opencode_path = tempdir
        .path()
        .join(".config")
        .join("opencode")
        .join("opencode.json");
    fs::create_dir_all(opencode_path.parent().expect("parent")).expect("opencode dir");
    fs::write(
        &opencode_path,
        br#"{"model":"anthropic/claude-sonnet-stale","provider":{"anthropic":{}}}"#,
    )
    .expect("prior opencode config");

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
        ])
        .assert()
        .success();

    let after: Value =
        serde_json::from_str(&fs::read_to_string(&opencode_path).expect("opencode readable"))
            .expect("opencode parses");
    assert!(
        after.get("model").is_none(),
        "opencode.json must not retain the stale model field after L87 provider-only init",
    );
}

#[test]
fn init_same_provider_without_model_preserves_existing_model() {
    // First init pins provider=openai, model=openai/gpt-5.5. Second
    // init re-runs with --provider openai but no --model. The L87
    // path must print the advertised list while preserving the
    // previously-pinned model — wiping it would silently change the
    // launched harness's model on a no-op rerun.
    let tempdir = tempfile::tempdir().expect("tempdir");
    write_workspace_init_config(tempdir.path());
    seed_init_secrets(tempdir.path(), &[("OPENAI_API_KEY", "test")]);
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
            "--model",
            "openai/gpt-5.5",
        ])
        .assert()
        .success();

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
        .success();

    let config = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    assert!(
        config.contains(r#"model = "openai/gpt-5.5""#),
        "second init --provider openai (no --model) must preserve the previously pinned model",
    );
}

#[test]
fn init_custom_provider_rejects_anthropic_messages_for_non_claude_agent() {
    let tempdir = tempfile::tempdir().expect("tempdir");

    acps_with_empty_path(tempdir.path())
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "opencode",
            "--provider",
            "myprovider",
            "--custom-provider",
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
            "--skip-workspace-init",
            "--skip-testflight",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "anthropic-messages custom providers only support Claude Code",
        ));
}

#[test]
fn init_claude_code_explicit_profile_model_skips_acp_discovery() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    seed_init_secrets(tempdir.path(), &[("MOONSHOT_API_KEY", "test-moonshot-key")]);

    acps_with_empty_path(tempdir.path())
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "claude-code",
            "--provider",
            "moonshotai",
            "--model",
            "kimi-k2.7-code",
            "--skip-workspace-init",
            "--skip-testflight",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "agent: Claude Code (claude-code)",
        ));

    let config = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    assert!(config.contains(r#"id = "claude-code""#));
    assert!(config.contains(r#"id = "moonshotai""#));
    assert!(config.contains(r#"model = "kimi-k2.7-code""#));

    let settings = claude_settings(tempdir.path());
    assert_eq!(
        settings["env"]["ANTHROPIC_BASE_URL"].as_str(),
        Some("https://api.moonshot.ai/anthropic")
    );
    assert_eq!(
        settings["apiKeyHelper"].as_str(),
        Some("printenv MOONSHOT_API_KEY")
    );
}

#[test]
fn init_kimi_explicit_model_skips_acp_discovery_and_persists_canonical_secret_ref() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    write_workspace_init_config(tempdir.path());
    seed_init_secrets(tempdir.path(), &[("KIMI_API_KEY", "test-kimi-key")]);

    acps_command_without_placebo()
        .env("HOME", tempdir.path())
        .env(TEST_SKIP_AGENT_INSTALL_ENV, "1")
        .args([
            "dev",
            "init",
            "--agent",
            "kimi",
            "--model",
            "k3",
            "--skip-workspace-init",
            "--skip-testflight",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent: Kimi Code (kimi)"));

    let config = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    assert!(config.contains(r#"id = "kimi""#));
    assert!(config.contains(r#"command = "kimi""#));
    assert!(config.contains(r#"args = ["acp"]"#));
    assert!(config.contains(r#"env = ["KIMI_API_KEY"]"#));
    assert!(config.contains(r#"model = "k3""#));
    assert!(!config.contains("KIMI_MODEL_"));
    assert!(!config.contains("test-kimi-key"));
    assert!(!config.contains("[agent.provider]"));
}

#[test]
fn init_hermes_writes_provider_backed_config_and_hermes_yaml() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    write_workspace_init_config(tempdir.path());
    seed_init_secrets(
        tempdir.path(),
        &[("OPENROUTER_API_KEY", "test-openrouter-key")],
    );

    acps_command_without_placebo()
        .env("HOME", tempdir.path())
        .env(TEST_SKIP_AGENT_INSTALL_ENV, "1")
        .args([
            "dev",
            "init",
            "--agent",
            "hermes",
            "--provider",
            "openrouter",
            "--api-key-ref",
            "OPENROUTER_API_KEY",
            "--model",
            "deepseek/deepseek-v4-flash",
            "--skip-workspace-init",
            "--skip-testflight",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent: Hermes Agent (hermes)"));

    let config_text = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    assert!(!config_text.contains("test-openrouter-key"));
    let config = load_config_from_str(&config_text).expect("canonical config parses");
    assert_eq!(config.agent.id, "hermes");
    assert_eq!(config.agent.command, "hermes");
    assert_eq!(config.agent.args, ["acp"]);
    assert_eq!(config.agent.env, ["OPENROUTER_API_KEY"]);
    let provider = config.agent.provider.as_ref().expect("provider configured");
    assert_eq!(provider.id, "openrouter");
    assert_eq!(
        provider.model.as_deref(),
        Some("deepseek/deepseek-v4-flash")
    );
    assert_eq!(provider.api_key_ref.as_deref(), Some("OPENROUTER_API_KEY"));
    assert!(config.agent.model.is_none());

    let hermes_yaml = fs::read_to_string(tempdir.path().join(".hermes/config.yaml"))
        .expect("hermes config should be readable");
    let hermes: serde_norway::Value =
        serde_norway::from_str(&hermes_yaml).expect("hermes config parses");
    assert_eq!(hermes["model"]["provider"], "openrouter");
    assert_eq!(hermes["model"]["default"], "deepseek/deepseek-v4-flash");
    assert!(!hermes_yaml.contains("test-openrouter-key"));
}

#[test]
fn init_kimi_without_model_pins_default_and_keeps_operator_selection_on_rerun() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    write_workspace_init_config(tempdir.path());
    seed_init_secrets(tempdir.path(), &[("KIMI_API_KEY", "test-kimi-key")]);

    let init_args = [
        "dev",
        "init",
        "--agent",
        "kimi",
        "--skip-workspace-init",
        "--skip-testflight",
    ];
    acps_command_without_placebo()
        .env("HOME", tempdir.path())
        .env(TEST_SKIP_AGENT_INSTALL_ENV, "1")
        .args(init_args)
        .assert()
        .success()
        .stdout(predicates::str::contains("agent: Kimi Code (kimi)"));

    let config_path = tempdir.path().join(".config/acp-stack/acps-config.toml");
    let config = fs::read_to_string(&config_path).expect("config should be readable");
    assert!(config.contains(r#"model = "kimi-for-coding""#));
    assert!(!config.contains("KIMI_MODEL_"));

    // A model the operator already picked survives a model-less re-init.
    let selected = config.replace(
        r#"model = "kimi-for-coding""#,
        r#"model = "kimi-for-coding-highspeed""#,
    );
    fs::write(&config_path, selected).expect("config should be writable");
    acps_command_without_placebo()
        .env("HOME", tempdir.path())
        .env(TEST_SKIP_AGENT_INSTALL_ENV, "1")
        .args(init_args)
        .assert()
        .success();
    let config = fs::read_to_string(&config_path).expect("config should be readable");
    assert!(config.contains(r#"model = "kimi-for-coding-highspeed""#));
}

#[test]
fn init_claude_code_profile_provider_filters_builtin_model_aliases() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    seed_init_secrets(tempdir.path(), &[("MOONSHOT_API_KEY", "test-moonshot-key")]);
    let options_path = write_acp_config_options(
        tempdir.path(),
        &["opus", "sonnet", "kimi-k2.7-code", "haiku"],
        &[],
    );

    acps_with_empty_path(tempdir.path())
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args([
            "dev",
            "init",
            "--agent",
            "claude-code",
            "--provider",
            "moonshotai",
            "--skip-workspace-init",
            "--skip-testflight",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "advertised models for Claude Code:",
        ))
        .stdout(predicates::str::contains("  kimi-k2.7-code"))
        .stdout(predicates::str::contains("  opus").not())
        .stdout(predicates::str::contains("  sonnet").not())
        .stdout(predicates::str::contains("  haiku").not());

    let config = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    assert!(config.contains(r#"id = "moonshotai""#));
    assert!(!config.contains(r#"model = "kimi-k2.7-code""#));
}

#[test]
fn init_codex_openrouter_lists_provider_catalog_models() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    seed_init_secrets(
        tempdir.path(),
        &[("OPENROUTER_API_KEY", "test-openrouter-key")],
    );
    // codex-acp advertises codex-core's bundled OpenAI presets regardless of
    // the configured provider; the init model list must come from the live
    // provider catalog instead.
    let options_path = write_acp_config_options(tempdir.path(), &["gpt-5.5"], &[]);
    let base = spawn_provider_models_server(serde_json::json!({
        "data": [
            { "id": "deepseek/deepseek-v4-flash", "name": "DeepSeek V4 Flash" },
            { "id": "moonshotai/kimi-k3" },
        ]
    }));

    acps_with_empty_path(tempdir.path())
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .env("ACP_STACK_PROVIDER_MODELS_BASE", &base)
        .args([
            "dev",
            "init",
            "--agent",
            "codex",
            "--provider",
            "openrouter",
            "--skip-workspace-init",
            "--skip-testflight",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "provider catalog models for Codex:",
        ))
        .stdout(predicates::str::contains("  deepseek/deepseek-v4-flash"))
        .stdout(predicates::str::contains("  moonshotai/kimi-k3"))
        .stdout(predicates::str::contains("  gpt-5.5").not())
        .stdout(predicates::str::contains("advertised models for Codex:").not());

    let config = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    assert!(config.contains(r#"id = "openrouter""#));
    assert!(!config.contains(r#"model = "gpt-5.5""#));
}

#[test]
fn init_codex_openrouter_without_catalog_skips_model_list() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    seed_init_secrets(
        tempdir.path(),
        &[("OPENROUTER_API_KEY", "test-openrouter-key")],
    );
    let options_path = write_acp_config_options(tempdir.path(), &["gpt-5.5"], &[]);

    // Dead endpoint: the catalog refresh degrades to a warning, and the
    // model lane must not fall back to codex-acp's OpenAI presets.
    acps_with_empty_path(tempdir.path())
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .env("ACP_STACK_PROVIDER_MODELS_BASE", "http://127.0.0.1:1")
        .args([
            "dev",
            "init",
            "--agent",
            "codex",
            "--provider",
            "openrouter",
            "--skip-workspace-init",
            "--skip-testflight",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "no live model catalog available for Codex",
        ))
        .stdout(predicates::str::contains("  gpt-5.5").not());
}

#[test]
fn init_goose_custom_provider_provision_failure_removes_sidecar() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    seed_init_secrets(tempdir.path(), &[("CUSTOM_API_KEY", "test")]);
    let goose_config_path = tempdir
        .path()
        .join(".config")
        .join("goose")
        .join("config.yaml");
    fs::create_dir_all(goose_config_path.parent().expect("parent")).expect("goose config dir");
    fs::write(&goose_config_path, "[").expect("invalid goose config");

    let sidecar_path = tempdir
        .path()
        .join(".config")
        .join("goose")
        .join("custom_providers")
        .join("myprovider.json");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "goose",
            "--provider",
            "myprovider",
            "--custom-provider",
            "--provider-name",
            "My Provider",
            "--base-url",
            "https://api.myprovider.example/v1",
            "--api-key-ref",
            "CUSTOM_API_KEY",
            "--model",
            "my-freeform-model",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("existing YAML is invalid"));

    assert!(
        !sidecar_path.exists(),
        "failed goose custom-provider init must remove the generated sidecar",
    );
}

#[test]
fn init_pi_custom_provider_provision_failure_removes_models_json() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    seed_init_secrets(tempdir.path(), &[("CUSTOM_API_KEY", "test")]);
    let settings_path = tempdir
        .path()
        .join(".pi")
        .join("agent")
        .join("settings.json");
    fs::create_dir_all(settings_path.parent().expect("parent")).expect("pi settings dir");
    fs::write(&settings_path, "not json").expect("invalid pi settings");

    let models_path = tempdir.path().join(".pi").join("agent").join("models.json");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "pi",
            "--provider",
            "myprovider",
            "--custom-provider",
            "--provider-name",
            "My Provider",
            "--base-url",
            "https://api.myprovider.example/v1",
            "--api-key-ref",
            "CUSTOM_API_KEY",
            "--model",
            "my-freeform-model",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("existing JSON is invalid"));

    assert!(
        !models_path.exists(),
        "failed pi custom-provider init must remove generated models.json",
    );
}

#[test]
fn init_rejects_model_for_agents_without_set_model_before_discovery() {
    // amp has set_model=false; --model must fail fast as a capability
    // check rather than being silently ignored or surfacing as a
    // downstream "binary not on PATH" error.
    let tempdir = tempfile::tempdir().expect("tempdir");
    seed_init_secrets(tempdir.path(), &[("AMP_API_KEY", "test")]);

    acps_command()
        .env("HOME", tempdir.path())
        .args(["init", "--agent", "amp", "--model", "anything"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "does not support model configuration through `acps init`",
        ));
}

#[test]
fn init_custom_codex_provider_rejects_openai_provider_id() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    seed_init_secrets(
        tempdir.path(),
        &[("CUSTOM_OPENAI_API_KEY", "test-custom-openai-key")],
    );

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "codex",
            "--provider",
            "openai",
            "--custom-provider",
            "--provider-name",
            "OpenAI Compatible",
            "--base-url",
            "https://api.compat.example/v1",
            "--api-key-ref",
            "CUSTOM_OPENAI_API_KEY",
            "--model",
            "custom-responses-model",
            "--skip-workspace-init",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "reserved by the mapped-provider registry",
        ))
        .stderr(predicates::str::contains("openai-1"));
}

#[test]
fn init_custom_provider_fails_noninteractive_when_required_fields_are_missing() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "init",
            "--agent",
            "opencode",
            "--provider",
            "myprovider",
            "--custom-provider",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "--provider-name is required for custom provider init",
        ));
}

#[test]
fn init_codex_openai_rejects_api_key_ref() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
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
        .failure()
        .stderr(predicates::str::contains(
            "Codex OpenAI uses Codex-native auth; do not pass --api-key-ref",
        ));
}

#[test]
fn init_provider_failure_persists_selected_agent_for_resume() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["init", "--agent", "amp", "--provider", "openai"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "Amp Code does not support provider configuration during init",
        ));

    let config =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config should be readable");
    assert!(config.contains(r#"id = "amp""#));
    assert!(!config.contains(r#"id = "opencode""#));
    assert!(!config.contains("[array.targets.agent.provider]"));
}

#[test]
fn init_requires_provider_for_provider_capable_agent_without_existing_provider() {
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
        .args(["dev", "init", "--agent", "pi", "--skip-workspace-init"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "Pi Agent supports provider configuration; pass --provider <id>",
        ))
        .stderr(predicates::str::contains("failed step: provider_configure"));
}
