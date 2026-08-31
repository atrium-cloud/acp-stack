use acp_stack::secrets::SecretStore;
use serde_json::{Value, json};
use std::fs;

use crate::common::agent::spawn_provider_models_server;
use crate::common::cli::*;

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

        let output = acps_command(tempdir.path())
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
            base_url: "https://open.bigmodel.cn/api/anthropic",
            api_key_ref: "ZHIPU_API_KEY",
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

        acps_command(tempdir.path())
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
            model: "glm-5.3[1m]",
            opus_model: "glm-5.3[1m]",
            sonnet_model: "glm-5.3[1m]",
            haiku_model: "glm-4.7",
            subagent_model: None,
        },
        Case {
            provider: "zhipuai",
            base_url: "https://open.bigmodel.cn/api/anthropic",
            api_key_ref: "ZHIPU_API_KEY",
            model: "glm-5.3[1m]",
            opus_model: "glm-5.3[1m]",
            sonnet_model: "glm-5.3[1m]",
            haiku_model: "glm-4.7",
            subagent_model: None,
        },
        Case {
            provider: "minimax",
            base_url: "https://api.minimax.io/anthropic",
            api_key_ref: "MINIMAX_API_KEY",
            model: "MiniMax-M3[1m]",
            opus_model: "MiniMax-M3[1m]",
            sonnet_model: "MiniMax-M3[1m]",
            haiku_model: "MiniMax-M3[1m]",
            subagent_model: None,
        },
        Case {
            provider: "minimax-coding-plan",
            base_url: "https://api.minimax.io/anthropic",
            api_key_ref: "MINIMAX_API_KEY",
            model: "MiniMax-M3[1m]",
            opus_model: "MiniMax-M3[1m]",
            sonnet_model: "MiniMax-M3[1m]",
            haiku_model: "MiniMax-M3[1m]",
            subagent_model: None,
        },
        Case {
            provider: "minimax-cn",
            base_url: "https://api.minimaxi.com/anthropic",
            api_key_ref: "MINIMAX_CN_API_KEY",
            model: "MiniMax-M3[1m]",
            opus_model: "MiniMax-M3[1m]",
            sonnet_model: "MiniMax-M3[1m]",
            haiku_model: "MiniMax-M3[1m]",
            subagent_model: None,
        },
        Case {
            provider: "minimax-cn-coding-plan",
            base_url: "https://api.minimaxi.com/anthropic",
            api_key_ref: "MINIMAX_CN_API_KEY",
            model: "MiniMax-M3[1m]",
            opus_model: "MiniMax-M3[1m]",
            sonnet_model: "MiniMax-M3[1m]",
            haiku_model: "MiniMax-M3[1m]",
            subagent_model: None,
        },
        Case {
            provider: "xiaomi",
            base_url: "https://api.xiaomimimo.com/anthropic",
            api_key_ref: "XIAOMI_API_KEY",
            model: "mimo-v2.5-pro[1m]",
            opus_model: "mimo-v2.5-pro[1m]",
            sonnet_model: "mimo-v2.5-pro[1m]",
            haiku_model: "mimo-v2.5-pro[1m]",
            subagent_model: None,
        },
        Case {
            provider: "xiaomi-token-plan-cn",
            base_url: "https://token-plan-cn.xiaomimimo.com/anthropic",
            api_key_ref: "XIAOMI_TOKEN_PLAN_CN_API_KEY",
            model: "mimo-v2.5-pro[1m]",
            opus_model: "mimo-v2.5-pro[1m]",
            sonnet_model: "mimo-v2.5-pro[1m]",
            haiku_model: "mimo-v2.5-pro[1m]",
            subagent_model: None,
        },
        Case {
            provider: "xiaomi-token-plan-ams",
            base_url: "https://token-plan-ams.xiaomimimo.com/anthropic",
            api_key_ref: "XIAOMI_TOKEN_PLAN_AMS_API_KEY",
            model: "mimo-v2.5-pro[1m]",
            opus_model: "mimo-v2.5-pro[1m]",
            sonnet_model: "mimo-v2.5-pro[1m]",
            haiku_model: "mimo-v2.5-pro[1m]",
            subagent_model: None,
        },
        Case {
            provider: "xiaomi-token-plan-sgp",
            base_url: "https://token-plan-sgp.xiaomimimo.com/anthropic",
            api_key_ref: "XIAOMI_TOKEN_PLAN_SGP_API_KEY",
            model: "mimo-v2.5-pro[1m]",
            opus_model: "mimo-v2.5-pro[1m]",
            sonnet_model: "mimo-v2.5-pro[1m]",
            haiku_model: "mimo-v2.5-pro[1m]",
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

        let output = acps_command(tempdir.path())
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

    acps_command(tempdir.path())
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

    acps_command(tempdir.path())
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

    acps_command(tempdir.path())
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

    // A dead endpoint must degrade to a warning, never failing the command or
    // leaving a stale availableModels list behind.
    acps_command(tempdir.path())
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

fn claude_code_config() -> String {
    VALID_CONFIG
        .replace(r#"id = "opencode""#, r#"id = "claude""#)
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
