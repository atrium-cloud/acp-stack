use std::fs;

use crate::common::cli::*;

#[test]
fn init_rejects_model_for_agents_without_set_model_before_discovery() {
    // No embedded registry agent declares set_model=false anymore, so the
    // fail-fast capability check runs against a registry override; without
    // the gate, `--model` would be silently ignored or surface as a
    // downstream "binary not on PATH" error.
    let tempdir = tempfile::tempdir().expect("tempdir");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    crate::common::agent::write_amp_registry_override(&config_dir);
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
