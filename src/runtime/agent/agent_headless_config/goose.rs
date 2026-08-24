use super::*;

pub(super) fn provision_goose_config(config: &Config, home: &Path) -> Result<Vec<PathBuf>> {
    let path = home.join(".config").join("goose").join("config.yaml");
    let mut written = Vec::new();
    let Some(provider) = config.agent.provider.as_ref() else {
        return Ok(written);
    };
    let provider_id = provider.id.as_str();
    let api_key_ref = require_agent_env_for_provider(config, provider_id, &path)?;
    if let Some(custom) = provider.custom.as_ref() {
        let custom_provider_path =
            write_goose_custom_provider(home, provider_id, custom, api_key_ref)?;
        let mut root = read_yaml_mapping(&path)?;
        let values = [
            ("GOOSE_PROVIDER", YamlValue::String(provider_id.to_owned())),
            (
                "GOOSE_MODEL",
                YamlValue::String(configured_provider_model(config).unwrap_or("").to_owned()),
            ),
            ("GOOSE_MODE", YamlValue::String("auto".to_owned())),
            (
                "GOOSE_CONTEXT_STRATEGY",
                YamlValue::String("summarize".to_owned()),
            ),
            ("GOOSE_DISABLE_SESSION_NAMING", YamlValue::Bool(true)),
        ];
        for (key, value) in values {
            root.insert(YamlValue::String(key.to_owned()), value);
        }
        write_yaml_mapping(&path, root)?;
        written.push(path.clone());
        written.push(custom_provider_path);
        return Ok(written);
    }
    let Some(agent_provider_id) = agent_provider_id_for_provider_id(&config.agent.id, provider_id)
    else {
        return Err(StackError::AgentConfigProvision {
            path: path.clone(),
            reason: format!(
                "goose provider `{provider_id}` has no native provider id in provider/env mapping"
            ),
        });
    };
    let Some(native_ref) = env_var_for_agent_provider_id(&config.agent.id, provider_id) else {
        return Err(StackError::AgentConfigProvision {
            path: path.clone(),
            reason: format!(
                "goose provider `{provider_id}` has no API-key env mapping in provider/env mapping"
            ),
        });
    };
    if api_key_ref != native_ref {
        return Err(StackError::AgentConfigProvision {
            path: path.clone(),
            reason: format!(
                "goose provider `{provider_id}` requires provider-native env ref `{native_ref}`, got `{api_key_ref}`"
            ),
        });
    }

    let mut root = read_yaml_mapping(&path)?;
    let values = [
        (
            "GOOSE_PROVIDER",
            YamlValue::String(agent_provider_id.to_owned()),
        ),
        ("GOOSE_MODE", YamlValue::String("auto".to_owned())),
        (
            "GOOSE_CONTEXT_STRATEGY",
            YamlValue::String("summarize".to_owned()),
        ),
        ("GOOSE_DISABLE_SESSION_NAMING", YamlValue::Bool(true)),
    ];
    for (key, value) in values {
        root.insert(YamlValue::String(key.to_owned()), value);
    }
    // With no provider model configured, drop any stale `GOOSE_MODEL` so the
    // launched process does not keep using it under the new provider.
    match configured_provider_model(config) {
        Some(model) => {
            root.insert(
                YamlValue::String("GOOSE_MODEL".to_owned()),
                YamlValue::String(model.to_owned()),
            );
        }
        None => {
            root.remove(YamlValue::String("GOOSE_MODEL".to_owned()));
        }
    }

    write_yaml_mapping(&path, root)?;
    written.push(path.clone());
    Ok(written)
}

pub(super) fn cleanup_goose_config(
    config: &Config,
    home: &Path,
) -> Result<Vec<CleanedAgentConfig>> {
    let mut cleaned = Vec::new();
    let path = home.join(".config").join("goose").join("config.yaml");
    if path.exists() {
        let mut root = read_yaml_mapping(&path)?;
        let mut changed = false;
        for key in [
            "GOOSE_PROVIDER",
            "GOOSE_MODEL",
            "GOOSE_MODE",
            "GOOSE_CONTEXT_STRATEGY",
            "GOOSE_DISABLE_SESSION_NAMING",
        ] {
            changed |= root.remove(YamlValue::String(key.to_owned())).is_some();
        }
        if changed {
            write_or_remove_yaml_mapping(&path, root)?;
            cleaned.push(CleanedAgentConfig {
                label: "Goose config",
                path: path.clone(),
            });
        }
    }
    if let Some(provider) = config.agent.provider.as_ref()
        && provider.custom.is_some()
    {
        let path = home
            .join(".config")
            .join("goose")
            .join("custom_providers")
            .join(format!("{}.json", provider.id));
        if remove_file_if_exists(&path)? {
            cleaned.push(CleanedAgentConfig {
                label: "Goose custom provider",
                path,
            });
        }
    }
    Ok(cleaned)
}

fn write_goose_custom_provider(
    home: &Path,
    provider_id: &str,
    custom: &AgentCustomProviderConfig,
    api_key_ref: &str,
) -> Result<PathBuf> {
    let path = home
        .join(".config")
        .join("goose")
        .join("custom_providers")
        .join(format!("{provider_id}.json"));
    let mut root = Map::new();
    root.insert("id".to_owned(), json!(provider_id));
    root.insert("name".to_owned(), json!(custom.name.clone()));
    root.insert("engine".to_owned(), json!("openai"));
    root.insert("base_url".to_owned(), json!(custom.base_url.clone()));
    root.insert("api_key_env".to_owned(), json!(api_key_ref));
    root.insert("context_limit".to_owned(), json!(custom.context));
    root.insert(
        "output_max_tokens".to_owned(),
        json!(custom.output_max_tokens),
    );
    write_json_object(&path, root)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn goose_config_is_skipped_without_configured_provider() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let config = config_with_agent("goose", &["OPENROUTER_API_KEY"]);

        let provisioned =
            provision_agent_headless_config(&config, tempdir.path()).expect("provision");

        assert!(provisioned.is_empty());
    }

    #[test]
    fn goose_custom_provider_writes_provider_file_and_selection() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let config =
            custom_provider_config("goose", crate::config::CustomProviderApi::ChatCompletions);

        provision_agent_headless_config(&config, tempdir.path()).expect("provision");

        let provider_path = tempdir
            .path()
            .join(".config/goose/custom_providers/myprovider.json");
        let provider: Value = serde_json::from_str(
            &std::fs::read_to_string(provider_path).expect("custom provider should be readable"),
        )
        .expect("custom provider parses");
        assert_eq!(provider["base_url"], "https://api.myprovider.example/v1");
        assert_eq!(provider["api_key_env"], "CUSTOM_API_KEY");
        assert_eq!(provider["context_limit"], 200_000);

        let goose_path = tempdir.path().join(".config/goose/config.yaml");
        let goose: serde_norway::Value = serde_norway::from_str(
            &std::fs::read_to_string(goose_path).expect("goose config should be readable"),
        )
        .expect("goose config parses");
        assert_eq!(goose["GOOSE_PROVIDER"], "myprovider");
        assert_eq!(goose["GOOSE_MODEL"], "my-model");
    }

    #[test]
    fn goose_config_references_provider_native_env() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let mut config = config_with_agent("goose", &["OPENROUTER_API_KEY"]);
        config.agent.provider = Some(crate::config::AgentProviderConfig {
            id: "openrouter".to_owned(),
            model: Some("deepseek/deepseek-v4-flash".to_owned()),
            api_key_ref: Some("OPENROUTER_API_KEY".to_owned()),
            custom: None,
        });

        let provisioned =
            provision_agent_headless_config(&config, tempdir.path()).expect("provision");

        let path = tempdir
            .path()
            .join(".config")
            .join("goose")
            .join("config.yaml");
        assert_eq!(provisioned[0].path, path);
        let value: serde_norway::Value = serde_norway::from_str(
            &std::fs::read_to_string(&path).expect("goose config should be readable"),
        )
        .expect("goose config yaml parses");
        assert_eq!(value["GOOSE_PROVIDER"], "openrouter");
        assert_eq!(value["GOOSE_MODEL"], "deepseek/deepseek-v4-flash");
        assert_eq!(value["GOOSE_MODE"], "auto");
        assert_eq!(value["GOOSE_CONTEXT_STRATEGY"], "summarize");
        assert_eq!(value["GOOSE_DISABLE_SESSION_NAMING"], true);
    }

    #[test]
    fn goose_configured_provider_updates_provider_without_model() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = tempdir
            .path()
            .join(".config")
            .join("goose")
            .join("config.yaml");
        std::fs::create_dir_all(path.parent().expect("path has parent")).expect("create parent");
        std::fs::write(
            &path,
            "GOOSE_PROVIDER: openrouter\nGOOSE_MODEL: old/model\nCUSTOM_SETTING: keep\n",
        )
        .expect("write existing config");
        let mut config = config_with_agent("goose", &["OPENROUTER_API_KEY", "CEREBRAS_API_KEY"]);
        config.agent.provider = Some(crate::config::AgentProviderConfig {
            id: "cerebras".to_owned(),
            model: Some("llama3.1-8b".to_owned()),
            api_key_ref: Some("CEREBRAS_API_KEY".to_owned()),
            custom: None,
        });

        provision_agent_headless_config(&config, tempdir.path()).expect("provision");

        let value: serde_norway::Value = serde_norway::from_str(
            &std::fs::read_to_string(&path).expect("goose config should be readable"),
        )
        .expect("goose config yaml parses");
        assert_eq!(value["GOOSE_PROVIDER"], "cerebras");
        assert_eq!(value["GOOSE_MODEL"], "llama3.1-8b");
        assert_eq!(value["CUSTOM_SETTING"], "keep");
    }

    #[test]
    fn goose_provider_switch_without_model_clears_stale_goose_model() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = tempdir
            .path()
            .join(".config")
            .join("goose")
            .join("config.yaml");
        std::fs::create_dir_all(path.parent().expect("path has parent")).expect("create parent");
        std::fs::write(
            &path,
            "GOOSE_PROVIDER: openrouter\nGOOSE_MODEL: anthropic/claude-stale\nKEEP_ME: yes\n",
        )
        .expect("write existing config");
        let mut config = config_with_agent("goose", &["CEREBRAS_API_KEY"]);
        // New provider with no model selected, as when the operator picks a
        // provider but skips model setup.
        config.agent.provider = Some(crate::config::AgentProviderConfig {
            id: "cerebras".to_owned(),
            model: None,
            api_key_ref: Some("CEREBRAS_API_KEY".to_owned()),
            custom: None,
        });

        provision_agent_headless_config(&config, tempdir.path()).expect("provision");

        let value: serde_norway::Value =
            serde_norway::from_str(&std::fs::read_to_string(&path).expect("goose readable"))
                .expect("goose yaml parses");
        assert_eq!(value["GOOSE_PROVIDER"], "cerebras");
        assert!(
            value.as_mapping().is_some_and(|map| {
                !map.contains_key(serde_norway::Value::String("GOOSE_MODEL".to_owned()))
            }),
            "GOOSE_MODEL must be removed when no provider model is configured",
        );
        assert_eq!(value["KEEP_ME"], "yes");
    }

    #[test]
    fn goose_rejects_non_native_api_key_ref() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let mut config = config_with_agent("goose", &["CUSTOM_OPENROUTER_KEY"]);
        config.agent.provider = Some(crate::config::AgentProviderConfig {
            id: "openrouter".to_owned(),
            model: Some("deepseek/deepseek-v4-flash".to_owned()),
            api_key_ref: Some("CUSTOM_OPENROUTER_KEY".to_owned()),
            custom: None,
        });

        let err = provision_agent_headless_config(&config, tempdir.path()).expect_err("fails");

        assert!(
            err.to_string()
                .contains("requires provider-native env ref `OPENROUTER_API_KEY`"),
            "{err}"
        );
    }

    #[test]
    fn goose_rejects_invalid_existing_yaml() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = tempdir
            .path()
            .join(".config")
            .join("goose")
            .join("config.yaml");
        std::fs::create_dir_all(path.parent().expect("path has parent")).expect("create parent");
        std::fs::write(&path, "not: [valid").expect("write invalid yaml");
        let mut config = config_with_agent("goose", &["OPENROUTER_API_KEY"]);
        config.agent.provider = Some(crate::config::AgentProviderConfig {
            id: "openrouter".to_owned(),
            model: Some("deepseek/deepseek-v4-flash".to_owned()),
            api_key_ref: Some("OPENROUTER_API_KEY".to_owned()),
            custom: None,
        });

        let err = provision_agent_headless_config(&config, tempdir.path()).expect_err("fails");

        assert!(
            err.to_string().contains("existing YAML is invalid"),
            "{err}"
        );
    }

    #[test]
    fn goose_cleanup_removes_managed_keys_and_custom_provider() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let config =
            custom_provider_config("goose", crate::config::CustomProviderApi::ChatCompletions);
        let goose_path = tempdir
            .path()
            .join(".config")
            .join("goose")
            .join("config.yaml");
        let custom_provider_path = tempdir
            .path()
            .join(".config")
            .join("goose")
            .join("custom_providers")
            .join("myprovider.json");
        std::fs::create_dir_all(custom_provider_path.parent().expect("path has parent"))
            .expect("create parent");
        std::fs::write(
            &goose_path,
            "GOOSE_PROVIDER: myprovider\nGOOSE_MODEL: my-model\nGOOSE_MODE: auto\nGOOSE_CONTEXT_STRATEGY: summarize\nGOOSE_DISABLE_SESSION_NAMING: true\nKEEP_ME: yes\n",
        )
        .expect("write goose config");
        std::fs::write(&custom_provider_path, r#"{"id":"myprovider"}"#)
            .expect("write custom provider");

        let cleaned = cleanup_agent_headless_config(&config, tempdir.path()).expect("cleanup");

        assert_eq!(cleaned.len(), 2);
        assert!(!custom_provider_path.exists());
        let value: serde_norway::Value =
            serde_norway::from_str(&std::fs::read_to_string(&goose_path).expect("goose readable"))
                .expect("goose yaml parses");
        assert_eq!(value["KEEP_ME"], "yes");
        for key in [
            "GOOSE_PROVIDER",
            "GOOSE_MODEL",
            "GOOSE_MODE",
            "GOOSE_CONTEXT_STRATEGY",
            "GOOSE_DISABLE_SESSION_NAMING",
        ] {
            assert!(
                value.as_mapping().is_some_and(|map| {
                    !map.contains_key(serde_norway::Value::String(key.to_owned()))
                }),
                "{key} should be removed"
            );
        }
    }
}
