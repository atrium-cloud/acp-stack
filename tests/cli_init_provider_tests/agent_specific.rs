use std::fs;

use acp_stack::config::load_config_from_str;
use acp_stack::dev_gates::TEST_SKIP_AGENT_INSTALL_ENV;
use predicates::prelude::PredicateBooleanExt as _;
use serde_json::Value;

use crate::common::agent::spawn_provider_models_server;
use crate::common::cli::*;
use crate::support::write_workspace_init_config;

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
