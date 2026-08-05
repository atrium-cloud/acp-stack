use super::*;

pub(crate) const HERMES_AGENT_ID: &str = "hermes";

const HERMES_MODEL_KEY: &str = "model";
const HERMES_MODEL_PROVIDER_KEY: &str = "provider";
const HERMES_MODEL_DEFAULT_KEY: &str = "default";
const HERMES_MODEL_BASE_URL_KEY: &str = "base_url";
const HERMES_CUSTOM_PROVIDER_ID: &str = "custom";

fn hermes_config_path(home: &Path) -> PathBuf {
    home.join(".hermes").join("config.yaml")
}

/// `model.default` carries the bare provider-native model id: Hermes composes
/// its ACP `provider:model` ids itself, so a prefixed default would come back
/// double-prefixed (`openrouter:openrouter:...`) in session model lists.
/// Canonical config may hold the ACP-advertised `provider:model` form (model
/// resolution mirrors advertised ids), so the provider prefix is stripped
/// here.
fn hermes_native_model<'a>(provider_id: &str, model: &'a str) -> &'a str {
    model
        .strip_prefix(provider_id)
        .and_then(|rest| rest.strip_prefix(':'))
        .unwrap_or(model)
}

pub(super) fn provision_hermes_config(config: &Config, home: &Path) -> Result<Vec<PathBuf>> {
    let path = hermes_config_path(home);
    let mut written = Vec::new();
    let Some(provider) = config.agent.provider.as_ref() else {
        return Ok(written);
    };
    let provider_id = provider.id.as_str();
    let api_key_ref = require_agent_env_for_provider(config, provider_id, &path)?;

    let mut root = read_yaml_mapping(&path)?;
    let mut model = match root.remove(YamlValue::String(HERMES_MODEL_KEY.to_owned())) {
        Some(YamlValue::Mapping(existing)) => existing,
        // A scalar `model:` left by a user is superseded by the managed block.
        _ => serde_norway::Mapping::new(),
    };

    if let Some(custom) = provider.custom.as_ref() {
        model.insert(
            YamlValue::String(HERMES_MODEL_PROVIDER_KEY.to_owned()),
            YamlValue::String(HERMES_CUSTOM_PROVIDER_ID.to_owned()),
        );
        model.insert(
            YamlValue::String(HERMES_MODEL_BASE_URL_KEY.to_owned()),
            YamlValue::String(custom.base_url.clone()),
        );
        match configured_provider_model(config) {
            Some(configured) => {
                model.insert(
                    YamlValue::String(HERMES_MODEL_DEFAULT_KEY.to_owned()),
                    YamlValue::String(
                        hermes_native_model(HERMES_CUSTOM_PROVIDER_ID, configured).to_owned(),
                    ),
                );
            }
            None => {
                model.remove(YamlValue::String(HERMES_MODEL_DEFAULT_KEY.to_owned()));
            }
        }
        root.insert(
            YamlValue::String(HERMES_MODEL_KEY.to_owned()),
            YamlValue::Mapping(model),
        );
        write_yaml_mapping(&path, root)?;
        written.push(path);
        return Ok(written);
    }

    let Some(agent_provider_id) = agent_provider_id_for_provider_id(&config.agent.id, provider_id)
    else {
        return Err(StackError::AgentConfigProvision {
            path: path.clone(),
            reason: format!(
                "hermes provider `{provider_id}` has no native provider id in provider/env mapping"
            ),
        });
    };
    let Some(native_ref) = env_var_for_agent_provider_id(&config.agent.id, provider_id) else {
        return Err(StackError::AgentConfigProvision {
            path: path.clone(),
            reason: format!(
                "hermes provider `{provider_id}` has no API-key env mapping in provider/env mapping"
            ),
        });
    };
    if api_key_ref != native_ref {
        return Err(StackError::AgentConfigProvision {
            path: path.clone(),
            reason: format!(
                "hermes provider `{provider_id}` requires provider-native env ref `{native_ref}`, got `{api_key_ref}`"
            ),
        });
    }

    model.insert(
        YamlValue::String(HERMES_MODEL_PROVIDER_KEY.to_owned()),
        YamlValue::String(agent_provider_id.to_owned()),
    );
    // A mapped provider needs no endpoint override; drop one left behind by a
    // previous custom-provider configuration so it cannot shadow the native
    // endpoint.
    model.remove(YamlValue::String(HERMES_MODEL_BASE_URL_KEY.to_owned()));
    // Mirror the canonical config: if no provider model is configured, drop a
    // stale `model.default` from a prior run so the launched Hermes process
    // doesn't keep using it under the new provider.
    match configured_provider_model(config) {
        Some(configured) => {
            model.insert(
                YamlValue::String(HERMES_MODEL_DEFAULT_KEY.to_owned()),
                YamlValue::String(hermes_native_model(agent_provider_id, configured).to_owned()),
            );
        }
        None => {
            model.remove(YamlValue::String(HERMES_MODEL_DEFAULT_KEY.to_owned()));
        }
    }
    root.insert(
        YamlValue::String(HERMES_MODEL_KEY.to_owned()),
        YamlValue::Mapping(model),
    );

    write_yaml_mapping(&path, root)?;
    written.push(path);
    Ok(written)
}

pub(super) fn cleanup_hermes_config(
    _config: &Config,
    home: &Path,
) -> Result<Vec<CleanedAgentConfig>> {
    let mut cleaned = Vec::new();
    let path = hermes_config_path(home);
    if !path.exists() {
        return Ok(cleaned);
    }
    let mut root = read_yaml_mapping(&path)?;
    let mut changed = false;
    if let Some(YamlValue::Mapping(mut model)) =
        root.remove(YamlValue::String(HERMES_MODEL_KEY.to_owned()))
    {
        for key in [
            HERMES_MODEL_PROVIDER_KEY,
            HERMES_MODEL_DEFAULT_KEY,
            HERMES_MODEL_BASE_URL_KEY,
        ] {
            changed |= model.remove(YamlValue::String(key.to_owned())).is_some();
        }
        if !model.is_empty() {
            root.insert(
                YamlValue::String(HERMES_MODEL_KEY.to_owned()),
                YamlValue::Mapping(model),
            );
        }
    }
    if changed {
        write_or_remove_yaml_mapping(&path, root)?;
        cleaned.push(CleanedAgentConfig {
            label: "Hermes config",
            path,
        });
    }
    Ok(cleaned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hermes_config_is_skipped_without_configured_provider() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let config = config_with_agent("hermes", &["OPENROUTER_API_KEY"]);

        let provisioned =
            provision_agent_headless_config(&config, tempdir.path()).expect("provision");

        assert!(provisioned.is_empty());
    }

    #[test]
    fn hermes_config_writes_native_provider_and_bare_default() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let mut config = config_with_agent("hermes", &["OPENROUTER_API_KEY"]);
        config.agent.provider = Some(crate::config::AgentProviderConfig {
            id: "openrouter".to_owned(),
            model: Some("deepseek/deepseek-v4-flash".to_owned()),
            api_key_ref: Some("OPENROUTER_API_KEY".to_owned()),
            custom: None,
        });

        let provisioned =
            provision_agent_headless_config(&config, tempdir.path()).expect("provision");

        let path = tempdir.path().join(".hermes").join("config.yaml");
        assert_eq!(provisioned[0].path, path);
        assert_eq!(provisioned[0].label, "Hermes config");
        let value: serde_norway::Value = serde_norway::from_str(
            &std::fs::read_to_string(&path).expect("hermes config should be readable"),
        )
        .expect("hermes config yaml parses");
        assert_eq!(value["model"]["provider"], "openrouter");
        assert_eq!(value["model"]["default"], "deepseek/deepseek-v4-flash");
    }

    #[test]
    fn hermes_native_model_strips_only_colon_qualified_provider_prefix() {
        assert_eq!(
            hermes_native_model("openrouter", "openrouter:deepseek/deepseek-v4-flash"),
            "deepseek/deepseek-v4-flash"
        );
        // A model that merely starts with the provider id keeps its name.
        assert_eq!(
            hermes_native_model("openrouter", "openrouter-tuned/model"),
            "openrouter-tuned/model"
        );
        assert_eq!(
            hermes_native_model("openrouter", "openrouter"),
            "openrouter"
        );
        assert_eq!(
            hermes_native_model("openrouter", "deepseek/deepseek-v4-flash"),
            "deepseek/deepseek-v4-flash"
        );
    }

    #[test]
    fn hermes_config_strips_advertised_provider_prefix_from_default() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let mut config = config_with_agent("hermes", &["OPENROUTER_API_KEY"]);
        config.agent.provider = Some(crate::config::AgentProviderConfig {
            id: "openrouter".to_owned(),
            model: Some("openrouter:deepseek/deepseek-v4-flash".to_owned()),
            api_key_ref: Some("OPENROUTER_API_KEY".to_owned()),
            custom: None,
        });

        provision_agent_headless_config(&config, tempdir.path()).expect("provision");

        let path = tempdir.path().join(".hermes").join("config.yaml");
        let value: serde_norway::Value = serde_norway::from_str(
            &std::fs::read_to_string(&path).expect("hermes config should be readable"),
        )
        .expect("hermes config yaml parses");
        assert_eq!(value["model"]["default"], "deepseek/deepseek-v4-flash");
    }

    #[test]
    fn hermes_custom_provider_writes_base_url_and_custom_lane() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let config =
            custom_provider_config("hermes", crate::config::CustomProviderApi::ChatCompletions);

        provision_agent_headless_config(&config, tempdir.path()).expect("provision");

        let path = tempdir.path().join(".hermes").join("config.yaml");
        let value: serde_norway::Value = serde_norway::from_str(
            &std::fs::read_to_string(&path).expect("hermes config should be readable"),
        )
        .expect("hermes config yaml parses");
        assert_eq!(value["model"]["provider"], "custom");
        assert_eq!(
            value["model"]["base_url"],
            "https://api.myprovider.example/v1"
        );
        assert_eq!(value["model"]["default"], "my-model");
    }

    #[test]
    fn hermes_custom_to_native_switch_removes_stale_base_url() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = tempdir.path().join(".hermes").join("config.yaml");
        std::fs::create_dir_all(path.parent().expect("path has parent")).expect("create parent");
        std::fs::write(
            &path,
            "model:\n  provider: custom\n  base_url: https://api.myprovider.example/v1\n  default: custom:my-model\n",
        )
        .expect("write existing config");
        let mut config = config_with_agent("hermes", &["OPENROUTER_API_KEY"]);
        config.agent.provider = Some(crate::config::AgentProviderConfig {
            id: "openrouter".to_owned(),
            model: Some("deepseek/deepseek-v4-flash".to_owned()),
            api_key_ref: Some("OPENROUTER_API_KEY".to_owned()),
            custom: None,
        });

        provision_agent_headless_config(&config, tempdir.path()).expect("provision");

        let value: serde_norway::Value =
            serde_norway::from_str(&std::fs::read_to_string(&path).expect("hermes readable"))
                .expect("hermes yaml parses");
        assert_eq!(value["model"]["provider"], "openrouter");
        assert_eq!(value["model"]["default"], "deepseek/deepseek-v4-flash");
        assert!(
            value["model"]
                .as_mapping()
                .is_some_and(|map| !map.contains_key(YamlValue::String("base_url".to_owned()))),
            "model.base_url must be removed so it cannot shadow the native endpoint",
        );
    }

    #[test]
    fn hermes_native_to_custom_switch_overrides_native_lane() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = tempdir.path().join(".hermes").join("config.yaml");
        std::fs::create_dir_all(path.parent().expect("path has parent")).expect("create parent");
        std::fs::write(
            &path,
            "model:\n  provider: openai\n  default: openai:gpt-5\n",
        )
        .expect("write existing config");
        let config =
            custom_provider_config("hermes", crate::config::CustomProviderApi::ChatCompletions);

        provision_agent_headless_config(&config, tempdir.path()).expect("provision");

        let value: serde_norway::Value =
            serde_norway::from_str(&std::fs::read_to_string(&path).expect("hermes readable"))
                .expect("hermes yaml parses");
        assert_eq!(value["model"]["provider"], "custom");
        assert_eq!(
            value["model"]["base_url"],
            "https://api.myprovider.example/v1"
        );
        assert_eq!(value["model"]["default"], "my-model");
    }

    #[test]
    fn hermes_provider_switch_without_model_clears_stale_default() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = tempdir.path().join(".hermes").join("config.yaml");
        std::fs::create_dir_all(path.parent().expect("path has parent")).expect("create parent");
        std::fs::write(
            &path,
            "model:\n  provider: openai\n  default: openai:gpt-5\n  context_length: 128000\nskills_dir: keep\n",
        )
        .expect("write existing config");
        let mut config = config_with_agent("hermes", &["OPENROUTER_API_KEY"]);
        config.agent.provider = Some(crate::config::AgentProviderConfig {
            id: "openrouter".to_owned(),
            model: None,
            api_key_ref: Some("OPENROUTER_API_KEY".to_owned()),
            custom: None,
        });

        provision_agent_headless_config(&config, tempdir.path()).expect("provision");

        let value: serde_norway::Value =
            serde_norway::from_str(&std::fs::read_to_string(&path).expect("hermes readable"))
                .expect("hermes yaml parses");
        assert_eq!(value["model"]["provider"], "openrouter");
        assert!(
            value["model"]
                .as_mapping()
                .is_some_and(|map| !map.contains_key(YamlValue::String("default".to_owned()))),
            "model.default must be removed when no provider model is configured",
        );
        // User-owned keys survive, both inside and outside the model block.
        assert_eq!(value["model"]["context_length"], 128000);
        assert_eq!(value["skills_dir"], "keep");
    }

    #[test]
    fn hermes_rejects_non_native_api_key_ref() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let mut config = config_with_agent("hermes", &["CUSTOM_OPENROUTER_KEY"]);
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
    fn hermes_cleanup_removes_only_managed_model_keys() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let config = config_with_agent("hermes", &["OPENROUTER_API_KEY"]);
        let path = tempdir.path().join(".hermes").join("config.yaml");
        std::fs::create_dir_all(path.parent().expect("path has parent")).expect("create parent");
        std::fs::write(
            &path,
            "model:\n  provider: openrouter\n  default: openrouter:deepseek/deepseek-v4-flash\n  context_length: 128000\nkeep_me: yes\n",
        )
        .expect("write hermes config");

        let cleaned = cleanup_agent_headless_config(&config, tempdir.path()).expect("cleanup");

        assert_eq!(cleaned.len(), 1);
        let value: serde_norway::Value =
            serde_norway::from_str(&std::fs::read_to_string(&path).expect("hermes readable"))
                .expect("hermes yaml parses");
        assert_eq!(value["keep_me"], "yes");
        assert_eq!(value["model"]["context_length"], 128000);
        for key in ["provider", "default"] {
            assert!(
                value["model"]
                    .as_mapping()
                    .is_some_and(|map| !map.contains_key(YamlValue::String(key.to_owned()))),
                "model.{key} should be removed"
            );
        }
    }

    #[test]
    fn hermes_cleanup_removes_file_when_only_managed_keys_existed() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let config = config_with_agent("hermes", &["OPENROUTER_API_KEY"]);
        let path = tempdir.path().join(".hermes").join("config.yaml");
        std::fs::create_dir_all(path.parent().expect("path has parent")).expect("create parent");
        std::fs::write(&path, "model:\n  provider: openrouter\n").expect("write hermes config");

        let cleaned = cleanup_agent_headless_config(&config, tempdir.path()).expect("cleanup");

        assert_eq!(cleaned.len(), 1);
        assert!(!path.exists());
    }
}
