use super::*;

const CLAUDE_CODE_API_KEY_HELPER_PREFIX: &str = "printenv ";

pub(super) fn provision_claude_code_config(config: &Config, home: &Path) -> Result<Vec<PathBuf>> {
    let mut written = Vec::new();
    let Some(provider) = config.agent.provider.as_ref() else {
        return Ok(written);
    };
    let settings_path = home.join(".claude").join("settings.json");
    let onboarding_path = home.join(".claude.json");
    let mut settings = read_json_object(&settings_path)?;
    let remove_env = {
        let env = ensure_object_field(&mut settings, "env", &settings_path)?;
        remove_claude_managed_env(env);
        write_claude_provider_env(config, provider, env, &settings_path)?;
        env.is_empty()
    };
    if remove_env {
        settings.remove("env");
    }
    write_claude_api_key_helper(config, provider, &mut settings, &settings_path)?;
    write_json_object(&settings_path, settings)?;
    written.push(settings_path);

    let mut onboarding = read_json_object(&onboarding_path)?;
    onboarding.insert("hasCompletedOnboarding".to_owned(), json!(true));
    write_json_object(&onboarding_path, onboarding)?;
    written.push(onboarding_path);
    Ok(written)
}

pub(super) fn cleanup_claude_code_config(
    config: &Config,
    home: &Path,
) -> Result<Vec<CleanedAgentConfig>> {
    let mut cleaned = Vec::new();
    let Some(provider) = config.agent.provider.as_ref() else {
        return Ok(cleaned);
    };
    let settings_path = home.join(".claude").join("settings.json");
    let expected_env = claude_provider_env_for_config(config, provider, &settings_path)?;
    let expected_helper = claude_api_key_helper_for_provider(config, provider, &settings_path)?;
    if settings_path.exists() {
        let mut settings = read_json_object(&settings_path)?;
        let mut changed = false;
        let mut remove_env = false;
        if let Some(env) = settings
            .get_mut("env")
            .and_then(serde_json::Value::as_object_mut)
        {
            changed |= remove_matching_claude_env(env, &expected_env);
            remove_env = env.is_empty();
        }
        if remove_env {
            settings.remove("env");
            changed = true;
        }
        changed |= remove_matching_claude_api_key_helper(&mut settings, expected_helper.as_deref());
        if changed {
            write_or_remove_json_object(&settings_path, settings)?;
            cleaned.push(CleanedAgentConfig {
                label: "Claude Code config",
                path: settings_path,
            });
        }
    }

    Ok(cleaned)
}

fn claude_provider_env_for_config(
    config: &Config,
    provider: &AgentProviderConfig,
    path: &Path,
) -> Result<Map<String, serde_json::Value>> {
    let mut env = Map::new();
    write_claude_provider_env(config, provider, &mut env, path)?;
    Ok(env)
}

fn write_claude_provider_env(
    config: &Config,
    provider: &AgentProviderConfig,
    env: &mut Map<String, serde_json::Value>,
    path: &Path,
) -> Result<()> {
    if let Some(custom) = provider.custom.as_ref() {
        if custom.api != CustomProviderApi::AnthropicMessages {
            return Err(StackError::AgentConfigProvision {
                path: path.to_path_buf(),
                reason: "Claude Code custom providers only support anthropic-messages".to_owned(),
            });
        }
        env.insert(
            "ANTHROPIC_BASE_URL".to_owned(),
            json!(custom.base_url.clone()),
        );
        if let Some(model) = configured_provider_model(config) {
            insert_claude_model_env(env, model, false);
        }
        return Ok(());
    }

    let Some(profile) = profile_for_provider_id(&provider.id) else {
        return Err(StackError::AgentConfigProvision {
            path: path.to_path_buf(),
            reason: format!(
                "Claude Code provider `{}` has no Claude Code provider profile",
                provider.id
            ),
        });
    };
    for (key, value) in &profile.env {
        env.insert(key.clone(), json!(value));
    }
    if let Some(base_url) = profile.base_url.as_deref() {
        env.insert("ANTHROPIC_BASE_URL".to_owned(), json!(base_url));
    }
    if let Some(model) = configured_provider_model(config).filter(|model| !model.trim().is_empty())
    {
        insert_claude_model_env(env, model, profile.set_subagent_model);
    } else {
        insert_claude_profile_default_model_env(env, profile);
    }
    Ok(())
}

fn write_claude_api_key_helper(
    config: &Config,
    provider: &AgentProviderConfig,
    settings: &mut Map<String, serde_json::Value>,
    path: &Path,
) -> Result<()> {
    match claude_api_key_helper_for_provider(config, provider, path)? {
        Some(helper) => {
            settings.insert("apiKeyHelper".to_owned(), json!(helper));
        }
        None => {
            remove_managed_claude_api_key_helper(settings);
        }
    }
    Ok(())
}

fn claude_api_key_helper_for_provider(
    config: &Config,
    provider: &AgentProviderConfig,
    path: &Path,
) -> Result<Option<String>> {
    if provider.custom.is_some() {
        let api_key_ref =
            require_agent_env_for_provider_config(config, provider, &provider.id, path)?;
        return Ok(Some(claude_api_key_helper_command(api_key_ref)));
    }
    let Some(profile) = profile_for_provider_id(&provider.id) else {
        return Err(StackError::AgentConfigProvision {
            path: path.to_path_buf(),
            reason: format!(
                "Claude Code provider `{}` has no Claude Code provider profile",
                provider.id
            ),
        });
    };
    if profile.agent_native_auth {
        if provider.api_key_ref.is_some() {
            return Err(StackError::AgentConfigProvision {
                path: path.to_path_buf(),
                reason: format!(
                    "Claude Code provider `{}` uses agent-native auth; do not configure api_key_ref",
                    provider.id
                ),
            });
        }
        return Ok(None);
    }
    let api_key_ref = require_agent_env_for_provider_config(config, provider, &provider.id, path)?;
    Ok(Some(claude_api_key_helper_command(api_key_ref)))
}

fn insert_claude_model_env(
    env: &mut Map<String, serde_json::Value>,
    model: &str,
    set_subagent_model: bool,
) {
    for key in [
        "ANTHROPIC_MODEL",
        "ANTHROPIC_DEFAULT_FABLE_MODEL",
        "ANTHROPIC_DEFAULT_SONNET_MODEL",
        "ANTHROPIC_DEFAULT_OPUS_MODEL",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    ] {
        env.insert(key.to_owned(), json!(model));
    }
    if set_subagent_model {
        env.insert("CLAUDE_CODE_SUBAGENT_MODEL".to_owned(), json!(model));
    }
}

fn insert_claude_profile_default_model_env(
    env: &mut Map<String, serde_json::Value>,
    profile: &ClaudeCodeProviderProfile,
) {
    let Some(model) = profile
        .default_model
        .as_deref()
        .filter(|model| !model.trim().is_empty())
    else {
        return;
    };
    let opus_model = profile.default_opus_model.as_deref().unwrap_or(model);
    env.insert("ANTHROPIC_MODEL".to_owned(), json!(model));
    env.insert(
        "ANTHROPIC_DEFAULT_FABLE_MODEL".to_owned(),
        json!(opus_model),
    );
    env.insert("ANTHROPIC_DEFAULT_OPUS_MODEL".to_owned(), json!(opus_model));
    env.insert(
        "ANTHROPIC_DEFAULT_SONNET_MODEL".to_owned(),
        json!(profile.default_sonnet_model.as_deref().unwrap_or(model)),
    );
    env.insert(
        "ANTHROPIC_DEFAULT_HAIKU_MODEL".to_owned(),
        json!(profile.default_haiku_model.as_deref().unwrap_or(model)),
    );
    if profile.set_subagent_model {
        env.insert("CLAUDE_CODE_SUBAGENT_MODEL".to_owned(), json!(model));
    }
}

fn remove_claude_managed_env(env: &mut Map<String, serde_json::Value>) -> bool {
    let mut changed = false;
    for key in CLAUDE_CODE_MANAGED_ENV_KEYS {
        changed |= env.remove(*key).is_some();
    }
    changed
}

fn remove_matching_claude_env(
    env: &mut Map<String, serde_json::Value>,
    expected: &Map<String, serde_json::Value>,
) -> bool {
    let mut changed = false;
    for key in CLAUDE_CODE_MANAGED_ENV_KEYS {
        if expected
            .get(*key)
            .is_some_and(|expected_value| env.get(*key) == Some(expected_value))
        {
            env.remove(*key);
            changed = true;
        }
    }
    changed
}

fn claude_api_key_helper_command(api_key_ref: &str) -> String {
    format!("{CLAUDE_CODE_API_KEY_HELPER_PREFIX}{api_key_ref}")
}

fn remove_managed_claude_api_key_helper(settings: &mut Map<String, serde_json::Value>) -> bool {
    if settings
        .get("apiKeyHelper")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|value| value.starts_with(CLAUDE_CODE_API_KEY_HELPER_PREFIX))
    {
        settings.remove("apiKeyHelper");
        return true;
    }
    false
}

fn remove_matching_claude_api_key_helper(
    settings: &mut Map<String, serde_json::Value>,
    expected: Option<&str>,
) -> bool {
    if let Some(expected) = expected
        && settings
            .get("apiKeyHelper")
            .and_then(serde_json::Value::as_str)
            == Some(expected)
    {
        settings.remove("apiKeyHelper");
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn claude_code_moonshot_writes_endpoint_model_and_helper_without_secret_value() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let mut config = config_with_agent("claude-code", &["MOONSHOT_API_KEY"]);
        config.agent.provider = Some(crate::config::AgentProviderConfig {
            id: "moonshotai".to_owned(),
            model: None,
            api_key_ref: Some("MOONSHOT_API_KEY".to_owned()),
            custom: None,
        });

        let provisioned =
            provision_agent_headless_config(&config, tempdir.path()).expect("provision");

        assert_eq!(provisioned.len(), 2);
        let settings_path = tempdir.path().join(".claude").join("settings.json");
        let settings: Value = serde_json::from_str(
            &std::fs::read_to_string(&settings_path).expect("settings should be readable"),
        )
        .expect("settings json parses");
        assert_eq!(
            settings["env"]["ANTHROPIC_BASE_URL"],
            "https://api.moonshot.ai/anthropic"
        );
        assert_eq!(settings["env"]["ANTHROPIC_MODEL"], "kimi-k2.7-code");
        assert_eq!(
            settings["env"]["ANTHROPIC_DEFAULT_FABLE_MODEL"],
            "kimi-k2.7-code"
        );
        assert_eq!(
            settings["env"]["ANTHROPIC_DEFAULT_OPUS_MODEL"],
            "kimi-k2.7-code"
        );
        assert_eq!(
            settings["env"]["ANTHROPIC_DEFAULT_SONNET_MODEL"],
            "kimi-k2.7-code"
        );
        assert_eq!(
            settings["env"]["ANTHROPIC_DEFAULT_HAIKU_MODEL"],
            "kimi-k2.7-code"
        );
        assert_eq!(
            settings["env"]["CLAUDE_CODE_SUBAGENT_MODEL"],
            "kimi-k2.7-code"
        );
        assert_eq!(settings["env"]["ENABLE_TOOL_SEARCH"], "false");
        assert_eq!(settings["apiKeyHelper"], "printenv MOONSHOT_API_KEY");
        assert!(!settings.to_string().contains("sk-"));

        let onboarding_path = tempdir.path().join(".claude.json");
        let onboarding: Value = serde_json::from_str(
            &std::fs::read_to_string(onboarding_path).expect("onboarding should be readable"),
        )
        .expect("onboarding json parses");
        assert_eq!(onboarding["hasCompletedOnboarding"], true);
    }

    #[test]
    fn claude_code_zai_writes_profile_role_model_defaults() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let mut config = config_with_agent("claude-code", &["ZAI_API_KEY"]);
        config.agent.provider = Some(crate::config::AgentProviderConfig {
            id: "zai".to_owned(),
            model: None,
            api_key_ref: Some("ZAI_API_KEY".to_owned()),
            custom: None,
        });

        provision_agent_headless_config(&config, tempdir.path()).expect("provision");

        let settings_path = tempdir.path().join(".claude").join("settings.json");
        let settings: Value = serde_json::from_str(
            &std::fs::read_to_string(&settings_path).expect("settings should be readable"),
        )
        .expect("settings json parses");
        assert_eq!(
            settings["env"]["ANTHROPIC_BASE_URL"],
            "https://api.z.ai/api/anthropic"
        );
        assert_eq!(settings["env"]["ANTHROPIC_MODEL"], "glm-5.2[1m]");
        assert_eq!(
            settings["env"]["ANTHROPIC_DEFAULT_FABLE_MODEL"],
            "glm-5.2[1m]"
        );
        assert_eq!(
            settings["env"]["ANTHROPIC_DEFAULT_OPUS_MODEL"],
            "glm-5.2[1m]"
        );
        assert_eq!(
            settings["env"]["ANTHROPIC_DEFAULT_SONNET_MODEL"],
            "glm-5.2[1m]"
        );
        assert_eq!(settings["env"]["ANTHROPIC_DEFAULT_HAIKU_MODEL"], "GLM-4.7");
        assert_eq!(
            settings["env"]["CLAUDE_CODE_AUTO_COMPACT_WINDOW"],
            "1000000"
        );
        assert_eq!(settings["env"]["API_TIMEOUT_MS"], "3000000");
        assert_eq!(settings["apiKeyHelper"], "printenv ZAI_API_KEY");
    }

    #[test]
    fn claude_code_bedrock_uses_native_auth_without_helper() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let settings_path = tempdir.path().join(".claude").join("settings.json");
        std::fs::create_dir_all(settings_path.parent().expect("settings has parent"))
            .expect("create settings dir");
        std::fs::write(
            &settings_path,
            r#"{"apiKeyHelper":"printenv OLD_KEY","env":{"KEEP_ME":"yes","ANTHROPIC_BASE_URL":"https://old.example"}}"#,
        )
        .expect("write existing settings");
        let mut config = config_with_agent("claude-code", &[]);
        config.agent.provider = Some(crate::config::AgentProviderConfig {
            id: "amazon-bedrock".to_owned(),
            model: Some("us.anthropic.claude-sonnet-4-5-20250929-v1:0".to_owned()),
            api_key_ref: None,
            custom: None,
        });

        provision_agent_headless_config(&config, tempdir.path()).expect("provision");

        let settings: Value = serde_json::from_str(
            &std::fs::read_to_string(&settings_path).expect("settings should be readable"),
        )
        .expect("settings json parses");
        assert!(settings.get("apiKeyHelper").is_none());
        assert_eq!(settings["env"]["KEEP_ME"], "yes");
        assert_eq!(settings["env"]["CLAUDE_CODE_USE_BEDROCK"], "1");
        assert_eq!(
            settings["env"]["ANTHROPIC_MODEL"],
            "us.anthropic.claude-sonnet-4-5-20250929-v1:0"
        );
    }

    #[test]
    fn claude_code_cleanup_removes_managed_keys_and_keeps_user_settings() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let settings_path = tempdir.path().join(".claude").join("settings.json");
        std::fs::create_dir_all(settings_path.parent().expect("settings has parent"))
            .expect("create settings dir");
        std::fs::write(
            &settings_path,
            r#"{"apiKeyHelper":"printenv MOONSHOT_API_KEY","env":{"ANTHROPIC_BASE_URL":"https://api.moonshot.ai/anthropic","ANTHROPIC_AUTH_TOKEN":"old","ANTHROPIC_API_KEY":"old","ANTHROPIC_MODEL":"kimi-k2.7-code","ANTHROPIC_DEFAULT_FABLE_MODEL":"kimi-k2.7-code","ANTHROPIC_DEFAULT_OPUS_MODEL":"kimi-k2.7-code","ANTHROPIC_DEFAULT_SONNET_MODEL":"kimi-k2.7-code","ANTHROPIC_DEFAULT_HAIKU_MODEL":"kimi-k2.7-code","CLAUDE_CODE_SUBAGENT_MODEL":"kimi-k2.7-code","KEEP_ME":"yes"},"theme":"keep"}"#,
        )
        .expect("write settings");
        let onboarding_path = tempdir.path().join(".claude.json");
        std::fs::write(
            &onboarding_path,
            r#"{"hasCompletedOnboarding":true,"keep":true}"#,
        )
        .expect("write onboarding");
        let mut config = config_with_agent("claude-code", &["MOONSHOT_API_KEY"]);
        config.agent.provider = Some(crate::config::AgentProviderConfig {
            id: "moonshotai".to_owned(),
            model: Some("kimi-k2.7-code".to_owned()),
            api_key_ref: Some("MOONSHOT_API_KEY".to_owned()),
            custom: None,
        });

        let cleaned = cleanup_agent_headless_config(&config, tempdir.path()).expect("cleanup");

        assert_eq!(cleaned.len(), 1);
        let settings: Value = serde_json::from_str(
            &std::fs::read_to_string(&settings_path).expect("settings should be readable"),
        )
        .expect("settings json parses");
        assert!(settings.get("apiKeyHelper").is_none());
        assert!(settings["env"].get("ANTHROPIC_BASE_URL").is_none());
        assert_eq!(settings["env"]["ANTHROPIC_AUTH_TOKEN"], "old");
        assert_eq!(settings["env"]["ANTHROPIC_API_KEY"], "old");
        assert!(settings["env"].get("ANTHROPIC_MODEL").is_none());
        assert!(
            settings["env"]
                .get("ANTHROPIC_DEFAULT_FABLE_MODEL")
                .is_none()
        );
        assert!(
            settings["env"]
                .get("ANTHROPIC_DEFAULT_OPUS_MODEL")
                .is_none()
        );
        assert!(
            settings["env"]
                .get("ANTHROPIC_DEFAULT_SONNET_MODEL")
                .is_none()
        );
        assert!(
            settings["env"]
                .get("ANTHROPIC_DEFAULT_HAIKU_MODEL")
                .is_none()
        );
        assert!(settings["env"].get("CLAUDE_CODE_SUBAGENT_MODEL").is_none());
        assert_eq!(settings["env"]["KEEP_ME"], "yes");
        assert_eq!(settings["theme"], "keep");
        let onboarding: Value = serde_json::from_str(
            &std::fs::read_to_string(onboarding_path).expect("onboarding should be readable"),
        )
        .expect("onboarding json parses");
        assert_eq!(onboarding["hasCompletedOnboarding"], true);
        assert_eq!(onboarding["keep"], true);
    }

    #[test]
    fn claude_code_cleanup_preserves_unmatched_env_and_helper() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let settings_path = tempdir.path().join(".claude").join("settings.json");
        std::fs::create_dir_all(settings_path.parent().expect("settings has parent"))
            .expect("create settings dir");
        std::fs::write(
            &settings_path,
            r#"{"apiKeyHelper":"printenv USER_KEY","env":{"ANTHROPIC_BASE_URL":"https://user.example/anthropic","ANTHROPIC_MODEL":"user-model","KEEP_ME":"yes"}}"#,
        )
        .expect("write settings");
        let mut config = config_with_agent("claude-code", &["MOONSHOT_API_KEY"]);
        config.agent.provider = Some(crate::config::AgentProviderConfig {
            id: "moonshotai".to_owned(),
            model: Some("kimi-k2.7-code".to_owned()),
            api_key_ref: Some("MOONSHOT_API_KEY".to_owned()),
            custom: None,
        });

        let cleaned = cleanup_agent_headless_config(&config, tempdir.path()).expect("cleanup");

        assert!(cleaned.is_empty());
        let settings: Value = serde_json::from_str(
            &std::fs::read_to_string(&settings_path).expect("settings should be readable"),
        )
        .expect("settings json parses");
        assert_eq!(settings["apiKeyHelper"], "printenv USER_KEY");
        assert_eq!(
            settings["env"]["ANTHROPIC_BASE_URL"],
            "https://user.example/anthropic"
        );
        assert_eq!(settings["env"]["ANTHROPIC_MODEL"], "user-model");
        assert_eq!(settings["env"]["KEEP_ME"], "yes");
    }

    #[test]
    fn claude_code_cleanup_preserves_onboarding_when_unshared() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let onboarding_path = tempdir.path().join(".claude.json");
        std::fs::write(&onboarding_path, r#"{"hasCompletedOnboarding":true}"#)
            .expect("write onboarding");
        let mut config = config_with_agent("claude-code", &["MOONSHOT_API_KEY"]);
        config.agent.provider = Some(crate::config::AgentProviderConfig {
            id: "moonshotai".to_owned(),
            model: Some("kimi-k2.7-code".to_owned()),
            api_key_ref: Some("MOONSHOT_API_KEY".to_owned()),
            custom: None,
        });

        let cleaned = cleanup_agent_headless_config(&config, tempdir.path()).expect("cleanup");

        assert!(cleaned.is_empty());
        let onboarding: Value = serde_json::from_str(
            &std::fs::read_to_string(onboarding_path).expect("onboarding should be readable"),
        )
        .expect("onboarding json parses");
        assert_eq!(onboarding["hasCompletedOnboarding"], true);
    }
}
