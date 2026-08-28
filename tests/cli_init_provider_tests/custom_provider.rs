use std::fs;

use acp_stack::config::load_config_from_str;
use serde_json::Value;

use crate::common::cli::*;

#[test]
fn init_custom_opencode_provider_writes_generated_config() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    seed_init_secrets(tempdir.path(), &[("CUSTOM_API_KEY", "test-custom-key")]);

    acps_command(tempdir.path())
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

    acps_command(tempdir.path())
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

    // The init gate passed off the catalog, so spawn-time resolution must inject the same
    // credential.
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

/// Registry ids are reserved instance-wide: every site classifies by registry membership before
/// looking at `custom`, so a custom declaration under a registry id fails at spawn.
#[test]
fn init_custom_codex_provider_rejects_known_mapped_provider_id() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    seed_init_secrets(
        tempdir.path(),
        &[("ANTHROPIC_API_KEY", "test-anthropic-key")],
    );

    acps_command(tempdir.path())
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

    // The starter config is written before the provider step, so the file may exist; the
    // rejected custom provider must not be in it.
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

#[test]
fn init_custom_provider_rejects_anthropic_messages_for_non_claude_agent() {
    let tempdir = tempfile::tempdir().expect("tempdir");

    acps_with_empty_path(tempdir.path())
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

    acps_command(tempdir.path())
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

    acps_command(tempdir.path())
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
