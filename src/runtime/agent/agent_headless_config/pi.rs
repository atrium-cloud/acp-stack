use super::*;

/// An endpoint override reaches Pi through the launch env (see `acp_bridge::process_env`), so
/// `models.json` carries only a custom provider's own entry.
pub(super) fn provision_pi_config(
    config: &Config,
    workload: &WorkloadHome,
    previous_model: Option<&str>,
) -> Result<Option<PathBuf>> {
    let home = workload.home();
    let path = home.join(".pi").join("agent").join("settings.json");
    let Some(provider) = config.agent.provider.as_ref() else {
        return Ok(None);
    };
    let models_path = home.join(".pi").join("agent").join("models.json");
    if let Some(custom) = provider.custom.as_ref() {
        let api_key_ref = require_agent_env_for_provider(config, &provider.id, &models_path)?;
        write_pi_custom_models_json(workload, &models_path, provider, custom, api_key_ref)?;
    }
    let mut root = read_json_object(workload, &path)?;
    remove_legacy_pi_enabled_models(&mut root, configured_provider_model(config), previous_model);
    let native_provider = if provider.custom.is_some() {
        provider.id.as_str()
    } else {
        agent_provider_id_for_provider_id("pi", &provider.id).ok_or_else(|| {
            StackError::AgentConfigProvision {
                path: path.clone(),
                reason: format!("pi provider `{}` has no native provider id", provider.id),
            }
        })?
    };
    if provider.custom.is_none() {
        remove_pi_models_provider(
            workload,
            &models_path,
            native_provider,
            is_legacy_pi_override,
        )?;
    }
    root.insert("defaultProvider".to_owned(), json!(native_provider));
    match configured_provider_model(config) {
        Some(model) => {
            root.insert(
                "defaultModel".to_owned(),
                json!(pi_bare_model_id(model, &provider.id, native_provider)),
            );
        }
        None => {
            root.remove("defaultModel");
        }
    }

    write_json_object(workload, &path, root)?;
    Ok(Some(path))
}

pub(super) fn cleanup_pi_config(
    config: &Config,
    workload: &WorkloadHome,
) -> Result<Vec<CleanedAgentConfig>> {
    let home = workload.home();
    let mut cleaned = Vec::new();
    let settings_path = home.join(".pi").join("agent").join("settings.json");
    if workload.exists(&settings_path)? {
        let mut root = read_json_object(workload, &settings_path)?;
        let changed =
            remove_legacy_pi_enabled_models(&mut root, configured_provider_model(config), None)
                | root.remove("defaultProvider").is_some()
                | root.remove("defaultModel").is_some();
        if changed {
            write_or_remove_json_object(workload, &settings_path, root)?;
            cleaned.push(CleanedAgentConfig {
                label: "Pi settings",
                path: settings_path,
            });
        }
    }
    if let Some(provider) = config.agent.provider.as_ref() {
        // A custom provider owns its whole models.json entry; a mapped one owns
        // only a legacy endpoint-override entry under its native id.
        let models_path = home.join(".pi").join("agent").join("models.json");
        let changed = match provider.custom.as_ref() {
            Some(_) => remove_pi_models_provider(workload, &models_path, &provider.id, |_| true)?,
            None => match agent_provider_id_for_provider_id("pi", &provider.id) {
                Some(native_provider) => remove_pi_models_provider(
                    workload,
                    &models_path,
                    native_provider,
                    is_legacy_pi_override,
                )?,
                None => false,
            },
        };
        if changed {
            cleaned.push(CleanedAgentConfig {
                label: "Pi models",
                path: models_path,
            });
        }
    }
    Ok(cleaned)
}

/// The `baseUrl`-only entry earlier releases wrote to route a mapped provider. An entry with any
/// other key is an operator's own override, not ours to delete.
fn is_legacy_pi_override(entry: &serde_json::Value) -> bool {
    entry
        .as_object()
        .is_some_and(|entry| entry.len() == 1 && entry.contains_key("baseUrl"))
}

/// Removes `providers.<key>` from Pi's `models.json` when `owned` accepts it, dropping an emptied
/// `providers` object or file. Returns whether anything changed.
fn remove_pi_models_provider(
    workload: &WorkloadHome,
    path: &Path,
    key: &str,
    owned: impl Fn(&serde_json::Value) -> bool,
) -> Result<bool> {
    if !workload.exists(path)? {
        return Ok(false);
    }
    let mut root = read_json_object(workload, path)?;
    let Some(providers) = root
        .get_mut("providers")
        .and_then(serde_json::Value::as_object_mut)
    else {
        return Ok(false);
    };
    if !providers.get(key).is_some_and(owned) {
        return Ok(false);
    }
    providers.remove(key);
    if providers.is_empty() {
        root.remove("providers");
    }
    write_or_remove_json_object(workload, path, root)?;
    Ok(true)
}

fn write_pi_custom_models_json(
    workload: &WorkloadHome,
    path: &Path,
    provider: &crate::config::AgentProviderConfig,
    custom: &AgentCustomProviderConfig,
    api_key_ref: &str,
) -> Result<()> {
    let mut root = read_json_object(workload, path)?;
    let providers = ensure_object_field(&mut root, "providers", path)?;
    providers.insert(
        provider.id.clone(),
        json!({
            "baseUrl": custom.base_url,
            "api": custom.api.as_pi_api(),
            "apiKey": api_key_ref,
            "models": [{
                "id": provider.model.as_deref().unwrap_or(""),
                "name": custom.model_name.as_deref().unwrap_or_else(|| provider.model.as_deref().unwrap_or("")),
                "contextWindow": custom.context,
                "maxTokens": custom.output_max_tokens,
                "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 }
            }]
        }),
    );
    write_json_object(workload, path, root)
}

fn remove_legacy_pi_enabled_models(
    root: &mut Map<String, serde_json::Value>,
    configured_model: Option<&str>,
    previous_model: Option<&str>,
) -> bool {
    let managed_value = configured_model
        .into_iter()
        .chain(previous_model)
        .any(|model| root.get("enabledModels") == Some(&json!([model])));
    if managed_value {
        root.remove("enabledModels");
        return true;
    }
    false
}

fn pi_bare_model_id<'a>(model: &'a str, provider_id: &str, native_provider: &str) -> &'a str {
    model
        .split_once('/')
        .filter(|(prefix, _)| *prefix == provider_id || *prefix == native_provider)
        .map_or(model, |(_, model_id)| model_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// Stores an endpoint override for `provider_id` the way a managed-state apply does.
    fn stage_endpoint_override(home: &Path, provider_id: &str, api_key_ref: &str) {
        let mut store = crate::secrets::SecretStore::open_or_create(home).expect("store");
        store
            .apply_managed_state_credential(
                "managed-state",
                "provider-credential",
                1,
                Some(crate::secrets::ManagedCredentialSelection {
                    provider_id: provider_id.to_owned(),
                    values: std::collections::BTreeMap::from([(
                        api_key_ref.to_owned(),
                        "sk-test".to_owned(),
                    )]),
                    source_refs: std::collections::BTreeMap::new(),
                    base_url: Some("http://127.0.0.1:3129".to_owned()),
                }),
            )
            .expect("stage endpoint override");
    }

    fn seed_pi_models(home: &Path, text: &str) {
        let path = home.join(".pi").join("agent").join("models.json");
        std::fs::create_dir_all(path.parent().expect("path has parent")).expect("create parent");
        std::fs::write(path, text).expect("seed models.json");
    }

    fn pi_models_value(home: &Path) -> Option<Value> {
        let path = home.join(".pi").join("agent").join("models.json");
        std::fs::read_to_string(path)
            .ok()
            .map(|text| serde_json::from_str(&text).expect("pi models json parses"))
    }

    fn pi_anthropic_config() -> Config {
        let mut config = config_with_agent("pi", &["ANTHROPIC_API_KEY"]);
        config.agent.provider = Some(crate::config::AgentProviderConfig {
            id: "anthropic".to_owned(),
            model: Some("anthropic/claude-sonnet-4-5".to_owned()),
            api_key_ref: Some("ANTHROPIC_API_KEY".to_owned()),
            custom: None,
        });
        config
    }

    #[test]
    fn pi_mapped_provider_endpoint_writes_no_models_entry() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        stage_endpoint_override(tempdir.path(), "opencode-go", "OPENCODE_API_KEY");
        let mut config = config_with_agent("pi", &["OPENCODE_API_KEY"]);
        config.agent.provider = Some(crate::config::AgentProviderConfig {
            id: "opencode-go".to_owned(),
            model: Some("opencode-go/deepseek-v4-flash".to_owned()),
            api_key_ref: Some("OPENCODE_API_KEY".to_owned()),
            custom: None,
        });

        provision_agent_headless_config(&config, tempdir.path()).expect("provision");

        assert_eq!(pi_models_value(tempdir.path()), None);
    }

    #[test]
    fn pi_reprovision_removes_the_legacy_endpoint_entry() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        stage_endpoint_override(tempdir.path(), "anthropic", "ANTHROPIC_API_KEY");
        seed_pi_models(
            tempdir.path(),
            r#"{"providers":{"anthropic":{"baseUrl":"http://127.0.0.1:3129"}}}"#,
        );

        provision_agent_headless_config(&pi_anthropic_config(), tempdir.path()).expect("provision");

        assert_eq!(pi_models_value(tempdir.path()), None);
    }

    #[test]
    fn pi_leaves_an_operator_authored_provider_entry_alone() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        stage_endpoint_override(tempdir.path(), "anthropic", "ANTHROPIC_API_KEY");
        seed_pi_models(
            tempdir.path(),
            r#"{"providers":{"anthropic":{"baseUrl":"https://operator.example","headers":{"X":"1"}}}}"#,
        );

        provision_agent_headless_config(&pi_anthropic_config(), tempdir.path()).expect("provision");

        let value = pi_models_value(tempdir.path()).expect("models.json survives");
        assert_eq!(
            value["providers"]["anthropic"]["baseUrl"],
            "https://operator.example"
        );
    }

    #[test]
    fn pi_custom_provider_endpoint_keeps_the_declared_base_url() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        stage_endpoint_override(tempdir.path(), "myprovider", "CUSTOM_API_KEY");
        let config =
            custom_provider_config("pi", crate::config::CustomProviderApi::ChatCompletions);

        provision_agent_headless_config(&config, tempdir.path()).expect("provision");

        let value = pi_models_value(tempdir.path()).expect("models.json written");
        assert_eq!(
            value["providers"]["myprovider"]["baseUrl"],
            "https://api.myprovider.example/v1"
        );
    }

    #[test]
    fn pi_cleanup_removes_managed_model_scope_and_custom_provider() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let config =
            custom_provider_config("pi", crate::config::CustomProviderApi::ChatCompletions);
        let settings_path = tempdir
            .path()
            .join(".pi")
            .join("agent")
            .join("settings.json");
        let models_path = tempdir.path().join(".pi").join("agent").join("models.json");
        std::fs::create_dir_all(settings_path.parent().expect("path has parent"))
            .expect("create parent");
        std::fs::write(
            &settings_path,
            r#"{"enabledModels":["my-model"],"theme":"keep"}"#,
        )
        .expect("write settings");
        std::fs::write(
            &models_path,
            r#"{"providers":{"myprovider":{"baseUrl":"https://api.myprovider.example/v1"},"other":{"baseUrl":"https://api.other.example/v1"}},"keep":true}"#,
        )
        .expect("write models");

        let cleaned = cleanup_agent_headless_config(&config, tempdir.path()).expect("cleanup");

        assert_eq!(cleaned.len(), 2);
        let settings: Value = serde_json::from_str(
            &std::fs::read_to_string(&settings_path).expect("settings readable"),
        )
        .expect("settings json parses");
        assert_eq!(settings["theme"], "keep");
        assert!(settings.get("enabledModels").is_none());
        let models: Value =
            serde_json::from_str(&std::fs::read_to_string(&models_path).expect("models readable"))
                .expect("models json parses");
        assert_eq!(models["keep"], true);
        assert!(models["providers"].get("myprovider").is_none());
        assert_eq!(
            models["providers"]["other"]["baseUrl"],
            "https://api.other.example/v1"
        );
    }

    #[test]
    fn pi_settings_are_skipped_without_configured_provider() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let config = config_with_agent("pi", &["OPENCODE_API_KEY", "ANTHROPIC_API_KEY"]);

        let provisioned =
            provision_agent_headless_config(&config, tempdir.path()).expect("provision");

        assert!(provisioned.is_empty());
    }

    #[test]
    fn pi_configured_provider_updates_existing_model_scope() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = tempdir
            .path()
            .join(".pi")
            .join("agent")
            .join("settings.json");
        std::fs::create_dir_all(path.parent().expect("path has parent")).expect("create parent");
        std::fs::write(&path, r#"{"enabledModels":["anthropic/*"]}"#)
            .expect("write existing settings");
        let mut config = config_with_agent("pi", &["OPENCODE_API_KEY", "ANTHROPIC_API_KEY"]);
        config.agent.provider = Some(crate::config::AgentProviderConfig {
            id: "opencode-go".to_owned(),
            model: Some("opencode-go/deepseek-v4-flash".to_owned()),
            api_key_ref: Some("OPENCODE_API_KEY".to_owned()),
            custom: None,
        });

        provision_agent_headless_config(&config, tempdir.path()).expect("provision");

        let value: Value = serde_json::from_str(
            &std::fs::read_to_string(&path).expect("pi settings should be readable"),
        )
        .expect("pi settings json parses");
        assert_eq!(value["enabledModels"], json!(["anthropic/*"]));
        assert_eq!(value["defaultProvider"], "opencode-go");
        assert_eq!(value["defaultModel"], "deepseek-v4-flash");
    }

    #[test]
    fn pi_removes_only_legacy_acps_managed_enabled_model_value() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = tempdir
            .path()
            .join(".pi")
            .join("agent")
            .join("settings.json");
        std::fs::create_dir_all(path.parent().expect("path has parent")).expect("create parent");
        std::fs::write(
            &path,
            r#"{"enabledModels":["opencode-go/deepseek-v4-flash"]}"#,
        )
        .expect("write existing settings");
        let mut config = config_with_agent("pi", &["OPENCODE_API_KEY"]);
        config.agent.provider = Some(crate::config::AgentProviderConfig {
            id: "opencode-go".to_owned(),
            model: Some("opencode-go/deepseek-v4-flash".to_owned()),
            api_key_ref: Some("OPENCODE_API_KEY".to_owned()),
            custom: None,
        });

        provision_agent_headless_config(&config, tempdir.path()).expect("provision");

        let value: Value = serde_json::from_str(
            &std::fs::read_to_string(path).expect("pi settings should be readable"),
        )
        .expect("pi settings json parses");
        assert!(value.get("enabledModels").is_none());
    }

    #[test]
    fn pi_transition_removes_previous_acps_managed_enabled_model_value() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = tempdir
            .path()
            .join(".pi")
            .join("agent")
            .join("settings.json");
        std::fs::create_dir_all(path.parent().expect("path has parent")).expect("create parent");
        std::fs::write(&path, r#"{"enabledModels":["opencode-go/old-model"]}"#)
            .expect("write existing settings");
        let mut previous = config_with_agent("pi", &["OPENCODE_API_KEY"]);
        previous.agent.provider = Some(crate::config::AgentProviderConfig {
            id: "opencode-go".to_owned(),
            model: Some("opencode-go/old-model".to_owned()),
            api_key_ref: Some("OPENCODE_API_KEY".to_owned()),
            custom: None,
        });
        let mut next = previous.clone();
        next.agent.provider.as_mut().expect("provider").model =
            Some("opencode-go/new-model".to_owned());

        provision_agent_headless_config_transition(&previous, &next, tempdir.path())
            .expect("provision transition");

        let value: Value = serde_json::from_str(
            &std::fs::read_to_string(path).expect("pi settings should be readable"),
        )
        .expect("pi settings json parses");
        assert!(value.get("enabledModels").is_none());
        assert_eq!(value["defaultModel"], "new-model");
    }

    #[test]
    fn pi_multiple_active_providers_only_write_default_lane_settings() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let mut config = config_with_agent("pi", &[]);
        config.agent.provider = Some(crate::config::AgentProviderConfig {
            id: "openrouter".to_owned(),
            model: Some("openrouter/deepseek/deepseek-v4".to_owned()),
            api_key_ref: None,
            custom: None,
        });
        config.agent.providers = Some(crate::config::AgentProvidersConfig {
            active: vec!["openrouter".to_owned(), "anthropic".to_owned()],
            selected_aliases: std::collections::BTreeMap::new(),
        });

        provision_agent_headless_config(&config, tempdir.path()).expect("provision");

        let path = tempdir
            .path()
            .join(".pi")
            .join("agent")
            .join("settings.json");
        let value: Value = serde_json::from_str(
            &std::fs::read_to_string(path).expect("pi settings should be readable"),
        )
        .expect("pi settings json parses");
        assert_eq!(value["defaultProvider"], "openrouter");
        assert_eq!(value["defaultModel"], "deepseek/deepseek-v4");
        assert!(value.get("enabledModels").is_none());
    }

    #[test]
    fn pi_custom_provider_writes_models_json() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let config =
            custom_provider_config("pi", crate::config::CustomProviderApi::ChatCompletions);

        provision_agent_headless_config(&config, tempdir.path()).expect("provision");

        let models_path = tempdir.path().join(".pi").join("agent").join("models.json");
        let models: Value = serde_json::from_str(
            &std::fs::read_to_string(models_path).expect("models json should be readable"),
        )
        .expect("models json parses");
        assert_eq!(
            models["providers"]["myprovider"]["baseUrl"],
            "https://api.myprovider.example/v1"
        );
        assert_eq!(
            models["providers"]["myprovider"]["api"],
            "openai-completions"
        );
        assert_eq!(
            models["providers"]["myprovider"]["apiKey"],
            "CUSTOM_API_KEY"
        );
        assert_eq!(
            models["providers"]["myprovider"]["models"][0]["contextWindow"],
            200_000
        );
        assert_eq!(
            models["providers"]["myprovider"]["models"][0]["maxTokens"],
            65_536
        );
    }
}
