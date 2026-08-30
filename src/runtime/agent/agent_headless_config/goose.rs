use super::*;
use crate::runtime::agent::provider_keys::goose_host_env_for_native_provider_id;

/// Every host setting acps may write into goose's `config.yaml`; all are managed keys so a
/// cleared or moved override never leaves a stale host behind.
const GOOSE_MANAGED_HOST_KEYS: [&str; 4] = [
    "OPENAI_HOST",
    "ANTHROPIC_HOST",
    "OPENROUTER_HOST",
    "XAI_HOST",
];

pub(super) fn provision_goose_config(
    config: &Config,
    home: &Path,
    endpoint: Option<&crate::secrets::ProviderEndpointOverride>,
) -> Result<Vec<PathBuf>> {
    let path = home.join(".config").join("goose").join("config.yaml");
    let mut written = Vec::new();
    let Some(provider) = config.agent.provider.as_ref() else {
        return Ok(written);
    };
    let provider_id = provider.id.as_str();
    let api_key_ref = require_agent_env_for_provider(config, provider_id, &path)?;
    if let Some(custom) = provider.custom.as_ref() {
        let base_url_override =
            super::rerouted_base_url_for(endpoint, provider_id, &custom.base_url)?;
        let custom_provider_path = write_goose_custom_provider(
            home,
            provider_id,
            custom,
            api_key_ref,
            base_url_override.as_deref().unwrap_or(&custom.base_url),
        )?;
        let mut root = read_yaml_mapping(&path)?;
        // A custom provider carries its endpoint in its own file; a host left by an earlier
        // mapped-provider override would otherwise linger in config.yaml.
        for key in GOOSE_MANAGED_HOST_KEYS {
            root.remove(YamlValue::String(key.to_owned()));
        }
        let values = [
            ("GOOSE_PROVIDER", YamlValue::String(provider_id.to_owned())),
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
        // An empty pin is not a model: goose fails to resolve it while starting a
        // session, so an unset model must leave the key absent instead.
        write_goose_model(&mut root, config);
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
    // Goose appends its own request path to a host setting, so the override origin is the
    // whole value. Every managed host key is dropped first so a cleared or moved override
    // restores the vendor endpoint.
    for key in GOOSE_MANAGED_HOST_KEYS {
        root.remove(YamlValue::String(key.to_owned()));
    }
    if let Some(origin) = super::endpoint_origin_for(endpoint, provider_id) {
        let Some(host_env) = goose_host_env_for_native_provider_id(agent_provider_id) else {
            return Err(StackError::AgentConfigProvision {
                path: path.clone(),
                reason: format!(
                    "goose provider `{provider_id}` has no host setting, so it cannot be routed \
                     through a custom endpoint"
                ),
            });
        };
        root.insert(
            YamlValue::String(host_env.to_owned()),
            YamlValue::String(super::endpoint_origin(origin)?),
        );
    }
    write_goose_model(&mut root, config);

    write_yaml_mapping(&path, root)?;
    written.push(path.clone());
    Ok(written)
}

/// With no provider model configured, drop any stale `GOOSE_MODEL` so the launched process
/// neither keeps the previous provider's model nor starts with an unresolvable empty one.
fn write_goose_model(root: &mut serde_norway::Mapping, config: &Config) {
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
        ]
        .into_iter()
        .chain(GOOSE_MANAGED_HOST_KEYS)
        {
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
    base_url: &str,
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
    root.insert("base_url".to_owned(), json!(base_url));
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
    fn goose_custom_provider_without_model_clears_stale_goose_model() {
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
        let mut config =
            custom_provider_config("goose", crate::config::CustomProviderApi::ChatCompletions);
        // The operator declared the custom provider but skipped model setup; an empty pin is one
        // goose cannot resolve while starting a session.
        config
            .agent
            .provider
            .as_mut()
            .expect("custom provider")
            .model = None;

        provision_agent_headless_config(&config, tempdir.path()).expect("provision");

        let value = goose_config_value(tempdir.path());
        assert_eq!(value["GOOSE_PROVIDER"], "myprovider");
        assert!(
            value.as_mapping().is_some_and(|map| {
                !map.contains_key(serde_norway::Value::String("GOOSE_MODEL".to_owned()))
            }),
            "GOOSE_MODEL must be removed rather than written empty: {value:?}",
        );
        assert_eq!(value["KEEP_ME"], "yes");
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

    fn goose_endpoint(provider_id: &str) -> crate::secrets::ProviderEndpointOverride {
        crate::secrets::ProviderEndpointOverride {
            provider_id: provider_id.to_owned(),
            base_url: "http://127.0.0.1:3129".to_owned(),
            companion_values: std::collections::BTreeMap::new(),
        }
    }

    fn goose_openrouter_config() -> Config {
        let mut config = config_with_agent("goose", &["OPENROUTER_API_KEY"]);
        config.agent.provider = Some(crate::config::AgentProviderConfig {
            id: "openrouter".to_owned(),
            model: Some("deepseek/deepseek-v4-flash".to_owned()),
            api_key_ref: Some("OPENROUTER_API_KEY".to_owned()),
            custom: None,
        });
        config
    }

    fn goose_config_value(home: &Path) -> serde_norway::Value {
        let path = home.join(".config").join("goose").join("config.yaml");
        serde_norway::from_str(&std::fs::read_to_string(path).expect("goose config readable"))
            .expect("goose config yaml parses")
    }

    /// Every host key the provider mapping can hand out must be one cleanup removes.
    #[test]
    fn goose_managed_host_keys_cover_every_provider_host_env() {
        let mapped: Vec<&str> = ["openai", "anthropic", "openrouter", "xai"]
            .into_iter()
            .map(|native| goose_host_env_for_native_provider_id(native).expect("host env"))
            .collect();
        assert_eq!(mapped.len(), GOOSE_MANAGED_HOST_KEYS.len());
        for host_env in mapped {
            assert!(GOOSE_MANAGED_HOST_KEYS.contains(&host_env), "{host_env}");
        }
    }

    #[test]
    fn goose_mapped_provider_endpoint_rejects_an_override_carrying_a_path() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let config = goose_openrouter_config();
        let mut override_ = goose_endpoint("openrouter");
        override_.base_url = "http://127.0.0.1:3129/v1".to_owned();

        let error = provision_goose_config(&config, tempdir.path(), Some(&override_))
            .expect_err("a path in the stored override must not reach the host setting");
        assert!(error.to_string().contains("carries a path"), "{error}");
    }

    #[test]
    fn goose_mapped_provider_endpoint_writes_the_host_origin_and_restores_it() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let config = goose_openrouter_config();

        provision_goose_config(&config, tempdir.path(), Some(&goose_endpoint("openrouter")))
            .expect("provision with override");
        let value = goose_config_value(tempdir.path());
        assert_eq!(value["OPENROUTER_HOST"], "http://127.0.0.1:3129");

        provision_goose_config(&config, tempdir.path(), None).expect("provision without");
        let value = goose_config_value(tempdir.path());
        assert!(value["OPENROUTER_HOST"].is_null(), "{value:?}");
        assert_eq!(value["GOOSE_PROVIDER"], "openrouter");
    }

    #[test]
    fn goose_endpoint_for_another_provider_is_ignored() {
        let tempdir = tempfile::tempdir().expect("tempdir");

        provision_goose_config(
            &goose_openrouter_config(),
            tempdir.path(),
            Some(&goose_endpoint("openai")),
        )
        .expect("provision");

        let value = goose_config_value(tempdir.path());
        assert!(value["OPENAI_HOST"].is_null(), "{value:?}");
        assert!(value["OPENROUTER_HOST"].is_null(), "{value:?}");
    }

    #[test]
    fn goose_provider_without_a_host_setting_refuses_the_override() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let mut config = config_with_agent("goose", &["CEREBRAS_API_KEY"]);
        config.agent.provider = Some(crate::config::AgentProviderConfig {
            id: "cerebras".to_owned(),
            model: Some("llama3.1-8b".to_owned()),
            api_key_ref: Some("CEREBRAS_API_KEY".to_owned()),
            custom: None,
        });

        let error =
            provision_goose_config(&config, tempdir.path(), Some(&goose_endpoint("cerebras")))
                .expect_err("no host setting must refuse");

        assert!(error.to_string().contains("no host setting"), "{error}");
    }

    #[test]
    fn goose_custom_provider_endpoint_keeps_the_declared_path() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let config =
            custom_provider_config("goose", crate::config::CustomProviderApi::ChatCompletions);

        provision_goose_config(&config, tempdir.path(), Some(&goose_endpoint("myprovider")))
            .expect("provision");

        let provider_path = tempdir
            .path()
            .join(".config/goose/custom_providers/myprovider.json");
        let provider: Value = serde_json::from_str(
            &std::fs::read_to_string(provider_path).expect("custom provider should be readable"),
        )
        .expect("custom provider parses");
        assert_eq!(provider["base_url"], "http://127.0.0.1:3129/v1");
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
