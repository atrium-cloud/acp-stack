use super::*;

pub(crate) const OPENCODE_AGENT_ID: &str = "opencode";
// OpenCode treats an empty `small_model` as unset and still falls back to its
// implicit small model. This invalid id is the verified no-call sentinel.
pub(crate) const OPENCODE_DISABLED_SMALL_MODEL: &str = "invalid/model";

pub(super) fn provision_opencode_config(config: &Config, home: &Path) -> Result<Option<PathBuf>> {
    let path = home.join(".config").join("opencode").join("opencode.json");
    let active_providers = configured_active_provider_configs(config);
    if active_providers.is_empty() {
        return Ok(None);
    }
    let subagent_disabled = configured_subagent_disabled(config);
    let mut root = read_json_object(&path)?;
    insert_if_missing(
        &mut root,
        "$schema",
        json!("https://opencode.ai/config.json"),
        &path,
    )?;
    // Mirror the canonical config: if no provider model is configured,
    // also clear any stale `model` key in opencode.json. Otherwise an
    // earlier `acps agent set --model X` would silently override a
    // subsequent provider switch where the operator deliberately did
    // not pick a new model.
    match configured_provider_model(config) {
        Some(model) => {
            root.insert("model".to_owned(), json!(model));
            let small_model = if subagent_disabled {
                OPENCODE_DISABLED_SMALL_MODEL
            } else {
                configured_subagent_provider_model(config).unwrap_or(model)
            };
            root.insert("small_model".to_owned(), json!(small_model));
        }
        None => {
            root.remove("model");
            if subagent_disabled {
                root.insert(
                    "small_model".to_owned(),
                    json!(OPENCODE_DISABLED_SMALL_MODEL),
                );
            } else {
                root.remove("small_model");
            }
        }
    }

    let mut enabled_providers = BTreeSet::new();
    let providers = ensure_object_field(&mut root, "provider", &path)?;
    for provider in &active_providers {
        let provider_key = write_opencode_provider_config(config, providers, provider, &path)?;
        enabled_providers.insert(provider_key);
    }
    if enabled_providers.is_empty() {
        root.remove("enabled_providers");
    } else {
        root.insert(
            "enabled_providers".to_owned(),
            json!(enabled_providers.into_iter().collect::<Vec<_>>()),
        );
    }

    write_json_object(&path, root)?;
    Ok(Some(path))
}

pub(super) fn cleanup_opencode_config(
    config: &Config,
    home: &Path,
) -> Result<Vec<CleanedAgentConfig>> {
    let path = home.join(".config").join("opencode").join("opencode.json");
    if !path.exists() {
        return Ok(Vec::new());
    }
    let mut root = read_json_object(&path)?;
    let mut changed = false;
    for key in ["$schema", "model", "small_model", "enabled_providers"] {
        changed |= root.remove(key).is_some();
    }
    let mut provider_keys = BTreeSet::new();
    for provider in configured_active_provider_configs(config) {
        provider_keys.insert(opencode_provider_config_key(config, &provider).to_owned());
    }
    let mut remove_provider_object = false;
    if let Some(providers) = root
        .get_mut("provider")
        .and_then(serde_json::Value::as_object_mut)
    {
        for key in provider_keys {
            changed |= providers.remove(&key).is_some();
        }
        remove_provider_object = providers.is_empty();
    }
    if remove_provider_object {
        root.remove("provider");
    }
    if !changed {
        return Ok(Vec::new());
    }
    write_or_remove_json_object(&path, root)?;
    Ok(vec![CleanedAgentConfig {
        label: "OpenCode config",
        path,
    }])
}

fn opencode_provider_config_key<'a>(
    config: &'a Config,
    provider: &'a AgentProviderConfig,
) -> &'a str {
    provider
        .custom
        .as_ref()
        .map(|_| provider.id.as_str())
        .or_else(|| agent_provider_id_for_provider_id(&config.agent.id, &provider.id))
        .unwrap_or(provider.id.as_str())
}

fn write_opencode_provider_config(
    config: &Config,
    providers: &mut Map<String, serde_json::Value>,
    provider: &AgentProviderConfig,
    path: &Path,
) -> Result<String> {
    let api_key_ref = require_agent_env_for_provider_config(config, provider, &provider.id, path)?;
    if let Some(custom) = provider.custom.as_ref() {
        let provider_config = ensure_object_field(providers, &provider.id, path)?;
        provider_config.insert("npm".to_owned(), json!("@ai-sdk/openai-compatible"));
        provider_config.insert("name".to_owned(), json!(custom.name.clone()));
        let options = ensure_object_field(provider_config, "options", path)?;
        options.insert("baseURL".to_owned(), json!(custom.base_url.clone()));
        options.insert("apiKey".to_owned(), json!(format!("{{env:{api_key_ref}}}")));
        let models = ensure_object_field(provider_config, "models", path)?;
        if let Some(model) = provider
            .model
            .as_deref()
            .filter(|model| !model.trim().is_empty())
        {
            models.insert(
                model.to_owned(),
                json!({
                    "name": custom.model_name.as_deref().unwrap_or(model),
                    "limit": {
                        "context": custom.context,
                        "output": custom.output_max_tokens
                    }
                }),
            );
        }
        return Ok(provider.id.clone());
    }

    let Some(agent_provider_id) = agent_provider_id_for_provider_id(&config.agent.id, &provider.id)
    else {
        return Err(StackError::AgentConfigProvision {
            path: path.to_path_buf(),
            reason: format!(
                "opencode provider `{}` has no native provider id in provider/env mapping",
                provider.id
            ),
        });
    };
    let provider_config = ensure_object_field(providers, agent_provider_id, path)?;
    insert_if_missing(provider_config, "models", json!({}), path)?;
    let options = ensure_object_field(provider_config, "options", path)?;
    options.insert("apiKey".to_owned(), json!(format!("{{env:{api_key_ref}}}")));
    Ok(agent_provider_id.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn opencode_config_is_skipped_without_configured_provider() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let config = config_with_agent("opencode", &["OPENCODE_API_KEY"]);

        let provisioned =
            provision_agent_headless_config(&config, tempdir.path()).expect("provision");

        assert!(provisioned.is_empty());
    }

    #[test]
    fn opencode_config_is_not_merged_without_configured_provider() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = tempdir
            .path()
            .join(".config")
            .join("opencode")
            .join("opencode.json");
        std::fs::create_dir_all(path.parent().expect("path has parent")).expect("create parent");
        std::fs::write(
            &path,
            r#"{"model":"anthropic/claude-sonnet-4-5","provider":{"opencode-go":{"options":{"timeout":600000}}}}"#,
        )
        .expect("write existing config");
        let config = config_with_agent("opencode", &["OPENCODE_API_KEY"]);

        let provisioned =
            provision_agent_headless_config(&config, tempdir.path()).expect("provision");

        let value: Value = serde_json::from_str(
            &std::fs::read_to_string(&path).expect("opencode config should be readable"),
        )
        .expect("opencode config json parses");
        assert!(provisioned.is_empty());
        assert_eq!(value["model"], "anthropic/claude-sonnet-4-5");
        assert_eq!(
            value["provider"]["opencode-go"]["options"]["timeout"],
            600000
        );
        assert!(value["provider"]["opencode-go"]["options"]["apiKey"].is_null());
    }

    #[test]
    fn opencode_configured_provider_updates_model_and_api_key() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = tempdir
            .path()
            .join(".config")
            .join("opencode")
            .join("opencode.json");
        std::fs::create_dir_all(path.parent().expect("path has parent")).expect("create parent");
        std::fs::write(
            &path,
            r#"{"model":"opencode-go/deepseek-v4-flash","provider":{"openai":{"options":{"apiKey":"{env:OLD_KEY}","timeout":600000}}}}"#,
        )
        .expect("write existing config");
        let mut config = config_with_agent("opencode", &["OPENCODE_API_KEY", "OPENAI_API_KEY"]);
        config.agent.provider = Some(crate::config::AgentProviderConfig {
            id: "openai".to_owned(),
            model: Some("openai/gpt-5.5".to_owned()),
            api_key_ref: Some("OPENAI_API_KEY".to_owned()),
            custom: None,
        });

        provision_agent_headless_config(&config, tempdir.path()).expect("provision");

        let value: Value = serde_json::from_str(
            &std::fs::read_to_string(&path).expect("opencode config should be readable"),
        )
        .expect("opencode config json parses");
        assert_eq!(value["model"], "openai/gpt-5.5");
        assert_eq!(value["small_model"], "openai/gpt-5.5");
        assert_eq!(value["enabled_providers"], json!(["openai"]));
        assert_eq!(
            value["provider"]["openai"]["options"]["apiKey"],
            "{env:OPENAI_API_KEY}"
        );
        assert_eq!(value["provider"]["openai"]["options"]["timeout"], 600000);
    }

    #[test]
    fn opencode_writes_every_active_provider_and_exact_allowlist() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let mut config = config_with_agent("opencode", &[]);
        config.agent.provider = Some(crate::config::AgentProviderConfig {
            id: "opencode-go".to_owned(),
            model: Some("opencode-go/deepseek-v4-flash".to_owned()),
            api_key_ref: None,
            custom: None,
        });
        config.agent.providers = Some(crate::config::AgentProvidersConfig {
            active: vec!["opencode-go".to_owned(), "openrouter".to_owned()],
            selected_aliases: std::collections::BTreeMap::new(),
        });

        provision_agent_headless_config(&config, tempdir.path()).expect("provision");

        let path = tempdir
            .path()
            .join(".config")
            .join("opencode")
            .join("opencode.json");
        let value: Value = serde_json::from_str(
            &std::fs::read_to_string(path).expect("opencode config should be readable"),
        )
        .expect("opencode config json parses");
        assert_eq!(
            value["enabled_providers"],
            json!(["opencode-go", "openrouter"])
        );
        assert_eq!(
            value["provider"]["opencode-go"]["options"]["apiKey"],
            "{env:OPENCODE_API_KEY}"
        );
        assert_eq!(
            value["provider"]["openrouter"]["options"]["apiKey"],
            "{env:OPENROUTER_API_KEY}"
        );
    }

    #[test]
    fn opencode_configured_subagent_updates_small_model_and_enabled_providers() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let mut config = config_with_agent("opencode", &["OPENAI_API_KEY", "OPENCODE_API_KEY"]);
        config.agent.provider = Some(crate::config::AgentProviderConfig {
            id: "openai".to_owned(),
            model: Some("openai/gpt-5.5".to_owned()),
            api_key_ref: Some("OPENAI_API_KEY".to_owned()),
            custom: None,
        });
        config.agent.subagent = Some(crate::config::AgentSubagentConfig {
            disabled: false,
            provider: Some(crate::config::AgentProviderConfig {
                id: "opencode-go".to_owned(),
                model: Some("opencode-go/deepseek-v4-flash".to_owned()),
                api_key_ref: Some("OPENCODE_API_KEY".to_owned()),
                custom: None,
            }),
        });

        provision_agent_headless_config(&config, tempdir.path()).expect("provision");

        let path = tempdir
            .path()
            .join(".config")
            .join("opencode")
            .join("opencode.json");
        let value: Value = serde_json::from_str(
            &std::fs::read_to_string(path).expect("opencode config should be readable"),
        )
        .expect("opencode config json parses");
        assert_eq!(value["model"], "openai/gpt-5.5");
        assert_eq!(value["small_model"], "opencode-go/deepseek-v4-flash");
        assert_eq!(value["enabled_providers"], json!(["openai", "opencode-go"]));
        assert_eq!(
            value["provider"]["openai"]["options"]["apiKey"],
            "{env:OPENAI_API_KEY}"
        );
        assert_eq!(
            value["provider"]["opencode-go"]["options"]["apiKey"],
            "{env:OPENCODE_API_KEY}"
        );
    }

    #[test]
    fn opencode_cleanup_removes_managed_keys_and_keeps_user_settings() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = tempdir
            .path()
            .join(".config")
            .join("opencode")
            .join("opencode.json");
        std::fs::create_dir_all(path.parent().expect("path has parent")).expect("create parent");
        std::fs::write(
            &path,
            r#"{"$schema":"https://opencode.ai/config.json","model":"openai/gpt-5.5","small_model":"openai/gpt-5.5","enabled_providers":["openai"],"provider":{"openai":{"options":{"apiKey":"{env:OPENAI_API_KEY}"}},"anthropic":{"options":{"timeout":600000}}},"theme":"keep"}"#,
        )
        .expect("write opencode config");
        let mut config = config_with_agent("opencode", &["OPENAI_API_KEY"]);
        config.agent.provider = Some(crate::config::AgentProviderConfig {
            id: "openai".to_owned(),
            model: Some("openai/gpt-5.5".to_owned()),
            api_key_ref: Some("OPENAI_API_KEY".to_owned()),
            custom: None,
        });

        let cleaned = cleanup_agent_headless_config(&config, tempdir.path()).expect("cleanup");

        assert_eq!(cleaned[0].path, path);
        let value: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read opencode config"))
                .expect("json parses");
        assert_eq!(value["theme"], "keep");
        assert_eq!(value["provider"]["anthropic"]["options"]["timeout"], 600000);
        assert!(value.get("model").is_none());
        assert!(value["provider"].get("openai").is_none());
    }

    #[test]
    fn opencode_custom_provider_writes_model_limits() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let config = custom_provider_config(
            "opencode",
            crate::config::CustomProviderApi::ChatCompletions,
        );

        provision_agent_headless_config(&config, tempdir.path()).expect("provision");

        let path = tempdir
            .path()
            .join(".config")
            .join("opencode")
            .join("opencode.json");
        let value: Value = serde_json::from_str(
            &std::fs::read_to_string(path).expect("opencode config should be readable"),
        )
        .expect("opencode config json parses");
        assert_eq!(value["model"], "my-model");
        assert_eq!(value["small_model"], "my-model");
        assert_eq!(value["enabled_providers"], json!(["myprovider"]));
        assert_eq!(
            value["provider"]["myprovider"]["npm"],
            "@ai-sdk/openai-compatible"
        );
        assert_eq!(
            value["provider"]["myprovider"]["options"]["baseURL"],
            "https://api.myprovider.example/v1"
        );
        assert_eq!(
            value["provider"]["myprovider"]["options"]["apiKey"],
            "{env:CUSTOM_API_KEY}"
        );
        assert_eq!(
            value["provider"]["myprovider"]["models"]["my-model"]["limit"]["context"],
            200_000
        );
        assert_eq!(
            value["provider"]["myprovider"]["models"]["my-model"]["limit"]["output"],
            65_536
        );
    }
}
