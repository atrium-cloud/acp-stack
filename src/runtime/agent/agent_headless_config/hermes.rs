use super::*;

const HERMES_MODEL_KEY: &str = "model";
const HERMES_MODEL_PROVIDER_KEY: &str = "provider";
const HERMES_MODEL_DEFAULT_KEY: &str = "default";
const HERMES_MODEL_BASE_URL_KEY: &str = "base_url";
const HERMES_PROVIDERS_KEY: &str = "providers";
const HERMES_CUSTOM_PROVIDER_ID: &str = "custom";
const HERMES_MANAGED_ENTRY_KEY: &str = "acps-managed";
// `model.provider` references the managed named entry as `custom:<entry-key>`;
// keep this ref in sync with HERMES_MANAGED_ENTRY_KEY.
const HERMES_MANAGED_PROVIDER_REF: &str = "custom:acps-managed";

fn hermes_config_path(home: &Path) -> PathBuf {
    home.join(".hermes").join("config.yaml")
}

/// Strips a `provider:` prefix so `model.default` stays the bare id the adapter composes its `provider/model` option ids from.
fn hermes_native_model<'a>(provider_id: &str, model: &'a str) -> &'a str {
    model
        .strip_prefix(provider_id)
        .and_then(|rest| rest.strip_prefix(':'))
        .unwrap_or(model)
}

/// Writes the single managed `providers:` entry, preserving user-owned entries.
/// Every endpoint-carrying configuration rides this entry because Hermes' bare `custom` lane cannot carry a credential on a loopback endpoint (key resolution falls through to `no-key-required`) and derives its api_mode from URL detection.
fn write_managed_provider_entry(
    root: &mut serde_norway::Mapping,
    name: &str,
    base_url: &str,
    key_env: &str,
    transport: &str,
) {
    let mut providers = match root.remove(YamlValue::String(HERMES_PROVIDERS_KEY.to_owned())) {
        Some(YamlValue::Mapping(existing)) => existing,
        // A scalar `providers:` left by a user is superseded by the managed map.
        _ => serde_norway::Mapping::new(),
    };
    let mut entry = serde_norway::Mapping::new();
    for (key, value) in [
        ("name", name),
        ("base_url", base_url),
        ("key_env", key_env),
        ("transport", transport),
    ] {
        entry.insert(
            YamlValue::String(key.to_owned()),
            YamlValue::String(value.to_owned()),
        );
    }
    providers.insert(
        YamlValue::String(HERMES_MANAGED_ENTRY_KEY.to_owned()),
        YamlValue::Mapping(entry),
    );
    root.insert(
        YamlValue::String(HERMES_PROVIDERS_KEY.to_owned()),
        YamlValue::Mapping(providers),
    );
}

/// Removes the managed entry, dropping an emptied `providers` map so cleanup leaves no residue.
fn remove_managed_provider_entry(root: &mut serde_norway::Mapping) -> bool {
    let Some(YamlValue::Mapping(mut providers)) =
        root.remove(YamlValue::String(HERMES_PROVIDERS_KEY.to_owned()))
    else {
        return false;
    };
    let removed = providers
        .remove(YamlValue::String(HERMES_MANAGED_ENTRY_KEY.to_owned()))
        .is_some();
    if !providers.is_empty() {
        root.insert(
            YamlValue::String(HERMES_PROVIDERS_KEY.to_owned()),
            YamlValue::Mapping(providers),
        );
    }
    removed
}

/// The wire transport the managed entry declares. OpenCode Zen/Go route different models over different wires and their `/v1/models` listings carry no wire metadata, so the per-model table in data/endpoints.toml breaks the tie before the provider-level default.
fn hermes_managed_transport(
    path: &Path,
    provider_id: &str,
    configured_model: Option<&str>,
) -> Result<&'static str> {
    if let Some(model) = configured_model
        && let Some(wire) = model_wire(provider_id, model)
    {
        return match wire {
            ModelWire::ChatCompletions => Ok("chat_completions"),
            ModelWire::AnthropicMessages => Ok("anthropic_messages"),
            ModelWire::Responses => Ok("codex_responses"),
            // Hermes' custom-provider lane has no Google-native transport, so
            // a Zen/Go Gemini model cannot ride a managed endpoint.
            ModelWire::Google => Err(StackError::AgentConfigProvision {
                path: path.to_path_buf(),
                reason: format!(
                    "hermes model `{model}` on provider `{provider_id}` speaks the Google-native wire, which the managed endpoint lane cannot carry; select a different model or clear the endpoint override"
                ),
            }),
        };
    }
    hermes_api_mode_for_provider_id(provider_id).ok_or_else(|| {
        StackError::AgentConfigProvision {
            path: path.to_path_buf(),
            reason: format!(
                "hermes provider `{provider_id}` declares no hermes api_mode in provider/env mapping, so it cannot carry an endpoint override"
            ),
        }
    })
}

pub(super) fn provision_hermes_config(
    config: &Config,
    home: &Path,
    endpoint: Option<&crate::secrets::ProviderEndpointOverride>,
) -> Result<Vec<PathBuf>> {
    let path = hermes_config_path(home);
    let mut written = Vec::new();
    let Some(provider) = config.agent.provider.as_ref() else {
        return Ok(written);
    };
    let provider_id = provider.id.as_str();
    let base_url_override = super::endpoint_base_url_for(endpoint, provider_id);
    let api_key_ref = require_agent_env_for_provider(config, provider_id, &path)?;

    let mut root = read_yaml_mapping(&path)?;
    let mut model = match root.remove(YamlValue::String(HERMES_MODEL_KEY.to_owned())) {
        Some(YamlValue::Mapping(existing)) => existing,
        // A scalar `model:` left by a user is superseded by the managed block.
        _ => serde_norway::Mapping::new(),
    };

    // `model.base_url` is never written and always removed: upstream honors it unevenly across
    // native lanes (the anthropic lane silently falls back to api.anthropic.com for endpoints
    // outside its allowlist). Every endpoint lives on the managed named entry instead.
    model.remove(YamlValue::String(HERMES_MODEL_BASE_URL_KEY.to_owned()));

    if let Some(custom) = provider.custom.as_ref() {
        model.insert(
            YamlValue::String(HERMES_MODEL_PROVIDER_KEY.to_owned()),
            YamlValue::String(HERMES_MANAGED_PROVIDER_REF.to_owned()),
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
        // The override wins over the custom provider's own base URL.
        write_managed_provider_entry(
            &mut root,
            &custom.name,
            base_url_override.unwrap_or(&custom.base_url),
            api_key_ref,
            custom.api.as_hermes_api_mode(),
        );
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

    let written_provider_id = match base_url_override {
        Some(base_url) => {
            let configured_model = configured_provider_model(config)
                .map(|configured| hermes_native_model(agent_provider_id, configured));
            let transport = hermes_managed_transport(&path, provider_id, configured_model)?;
            let name = provider_name_for_provider_id(provider_id).unwrap_or(provider_id);
            write_managed_provider_entry(
                &mut root,
                &format!("{name} (managed endpoint)"),
                base_url,
                native_ref,
                transport,
            );
            HERMES_MANAGED_PROVIDER_REF
        }
        None => {
            // Drop a managed entry left by a cleared override so its endpoint cannot shadow the native one.
            remove_managed_provider_entry(&mut root);
            agent_provider_id
        }
    };
    model.insert(
        YamlValue::String(HERMES_MODEL_PROVIDER_KEY.to_owned()),
        YamlValue::String(written_provider_id.to_owned()),
    );
    // Drop a stale `model.default` so the launched Hermes process cannot keep using it under the new provider.
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
    let mut changed = remove_managed_provider_entry(&mut root);
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
    fn hermes_custom_provider_rides_the_managed_named_lane() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let config =
            custom_provider_config("hermes", crate::config::CustomProviderApi::ChatCompletions);

        provision_agent_headless_config(&config, tempdir.path()).expect("provision");

        let path = tempdir.path().join(".hermes").join("config.yaml");
        let value: serde_norway::Value = serde_norway::from_str(
            &std::fs::read_to_string(&path).expect("hermes config should be readable"),
        )
        .expect("hermes config yaml parses");
        assert_eq!(value["model"]["provider"], "custom:acps-managed");
        assert_eq!(value["model"]["default"], "my-model");
        assert!(
            value["model"]
                .as_mapping()
                .is_some_and(|map| !map.contains_key(YamlValue::String("base_url".to_owned()))),
            "model.base_url must never be written; the endpoint lives on the managed entry",
        );
        let entry = &value["providers"]["acps-managed"];
        assert_eq!(entry["name"], "My Provider");
        assert_eq!(entry["base_url"], "https://api.myprovider.example/v1");
        assert_eq!(entry["key_env"], "CUSTOM_API_KEY");
        assert_eq!(entry["transport"], "chat_completions");
        assert!(entry["default_model"].is_null(), "{entry:?}");
    }

    #[test]
    fn hermes_custom_to_native_switch_removes_stale_base_url() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = tempdir.path().join(".hermes").join("config.yaml");
        std::fs::create_dir_all(path.parent().expect("path has parent")).expect("create parent");
        std::fs::write(
            &path,
            "model:\n  provider: custom\n  base_url: https://api.myprovider.example/v1\n  default: custom:my-model\nproviders:\n  acps-managed:\n    name: My Provider (managed endpoint)\n    base_url: https://api.myprovider.example/v1\n    key_env: CUSTOM_API_KEY\n    transport: chat_completions\n",
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
        assert!(
            value["providers"].is_null(),
            "the emptied managed providers map must be dropped: {value:?}",
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
        assert_eq!(value["model"]["provider"], "custom:acps-managed");
        assert_eq!(value["model"]["default"], "my-model");
        let entry = &value["providers"]["acps-managed"];
        assert_eq!(entry["base_url"], "https://api.myprovider.example/v1");
        assert_eq!(entry["key_env"], "CUSTOM_API_KEY");
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
        assert_eq!(value["model"]["context_length"], 128000);
        assert_eq!(value["skills_dir"], "keep");
    }

    fn endpoint(provider_id: &str) -> crate::secrets::ProviderEndpointOverride {
        crate::secrets::ProviderEndpointOverride {
            provider_id: provider_id.to_owned(),
            base_url: "http://127.0.0.1:3129/openrouter".to_owned(),
        }
    }

    fn hermes_openrouter_config() -> Config {
        let mut config = config_with_agent("hermes", &["OPENROUTER_API_KEY"]);
        config.agent.provider = Some(crate::config::AgentProviderConfig {
            id: "openrouter".to_owned(),
            model: Some("deepseek/deepseek-v4-flash".to_owned()),
            api_key_ref: Some("OPENROUTER_API_KEY".to_owned()),
            custom: None,
        });
        config
    }

    fn hermes_config_value(home: &Path) -> serde_norway::Value {
        serde_norway::from_str(
            &std::fs::read_to_string(hermes_config_path(home)).expect("hermes config readable"),
        )
        .expect("hermes config yaml parses")
    }

    #[test]
    fn hermes_mapped_provider_endpoint_rides_the_managed_named_lane_and_is_restored() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let config = hermes_openrouter_config();

        provision_hermes_config(&config, tempdir.path(), Some(&endpoint("openrouter")))
            .expect("provision with override");
        let value = hermes_config_value(tempdir.path());
        assert_eq!(value["model"]["provider"], "custom:acps-managed");
        assert!(
            value["model"]
                .as_mapping()
                .is_some_and(|map| !map.contains_key(YamlValue::String("base_url".to_owned()))),
            "{value:?}"
        );
        assert_eq!(value["model"]["default"], "deepseek/deepseek-v4-flash");
        let entry = &value["providers"]["acps-managed"];
        assert_eq!(entry["name"], "OpenRouter (managed endpoint)");
        assert_eq!(entry["base_url"], "http://127.0.0.1:3129/openrouter");
        assert_eq!(entry["key_env"], "OPENROUTER_API_KEY");
        assert_eq!(entry["transport"], "chat_completions");
        assert!(entry["default_model"].is_null(), "{entry:?}");

        // Clearing the override restores the mapped lane and drops the emptied providers map.
        provision_hermes_config(&config, tempdir.path(), None).expect("provision without");
        let value = hermes_config_value(tempdir.path());
        assert_eq!(value["model"]["provider"], "openrouter");
        assert_eq!(value["model"]["default"], "deepseek/deepseek-v4-flash");
        assert!(
            value["model"]
                .as_mapping()
                .is_some_and(|map| !map.contains_key(YamlValue::String("base_url".to_owned()))),
            "{value:?}"
        );
        assert!(value["providers"].is_null(), "{value:?}");
    }

    #[test]
    fn hermes_override_provisions_anthropic_messages_transport() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let mut config = config_with_agent("hermes", &["ANTHROPIC_API_KEY"]);
        config.agent.provider = Some(crate::config::AgentProviderConfig {
            id: "anthropic".to_owned(),
            model: Some("claude-fable-5".to_owned()),
            api_key_ref: Some("ANTHROPIC_API_KEY".to_owned()),
            custom: None,
        });

        provision_hermes_config(&config, tempdir.path(), Some(&endpoint("anthropic")))
            .expect("provision with override");

        let value = hermes_config_value(tempdir.path());
        assert_eq!(value["model"]["provider"], "custom:acps-managed");
        assert_eq!(value["model"]["default"], "claude-fable-5");
        let entry = &value["providers"]["acps-managed"];
        assert_eq!(entry["name"], "Anthropic (managed endpoint)");
        assert_eq!(entry["key_env"], "ANTHROPIC_API_KEY");
        assert_eq!(entry["transport"], "anthropic_messages");
    }

    #[test]
    fn hermes_override_provisions_codex_responses_transport() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let mut config = config_with_agent("hermes", &["OPENAI_API_KEY"]);
        config.agent.provider = Some(crate::config::AgentProviderConfig {
            id: "openai".to_owned(),
            model: Some("gpt-5.3".to_owned()),
            api_key_ref: Some("OPENAI_API_KEY".to_owned()),
            custom: None,
        });

        provision_hermes_config(&config, tempdir.path(), Some(&endpoint("openai")))
            .expect("provision with override");

        let value = hermes_config_value(tempdir.path());
        assert_eq!(value["model"]["provider"], "custom:acps-managed");
        let entry = &value["providers"]["acps-managed"];
        assert_eq!(entry["name"], "OpenAI (managed endpoint)");
        assert_eq!(entry["key_env"], "OPENAI_API_KEY");
        assert_eq!(entry["transport"], "codex_responses");
    }

    fn hermes_opencode_config(model: Option<&str>) -> Config {
        let mut config = config_with_agent("hermes", &["OPENCODE_ZEN_API_KEY"]);
        config.agent.provider = Some(crate::config::AgentProviderConfig {
            id: "opencode".to_owned(),
            model: model.map(str::to_owned),
            api_key_ref: Some("OPENCODE_ZEN_API_KEY".to_owned()),
            custom: None,
        });
        config
    }

    #[test]
    fn hermes_override_zen_anthropic_model_declares_anthropic_messages() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let config = hermes_opencode_config(Some("opencode:claude-opus-5"));

        provision_hermes_config(&config, tempdir.path(), Some(&endpoint("opencode")))
            .expect("provision with override");

        let value = hermes_config_value(tempdir.path());
        assert_eq!(value["model"]["default"], "claude-opus-5");
        assert_eq!(
            value["providers"]["acps-managed"]["transport"],
            "anthropic_messages"
        );
    }

    #[test]
    fn hermes_override_zen_responses_model_declares_codex_responses() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let config = hermes_opencode_config(Some("gpt-5.5"));

        provision_hermes_config(&config, tempdir.path(), Some(&endpoint("opencode")))
            .expect("provision with override");

        let value = hermes_config_value(tempdir.path());
        assert_eq!(
            value["providers"]["acps-managed"]["transport"],
            "codex_responses"
        );
    }

    #[test]
    fn hermes_override_zen_google_wire_model_is_rejected() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let config = hermes_opencode_config(Some("gemini-3-flash"));

        let err = provision_hermes_config(&config, tempdir.path(), Some(&endpoint("opencode")))
            .expect_err("Google-wire model cannot ride a managed endpoint");

        assert!(
            matches!(err, StackError::AgentConfigProvision { .. }),
            "expected AgentConfigProvision, got {err:?}"
        );
        assert!(err.to_string().contains("Google-native wire"), "{err}");
    }

    #[test]
    fn hermes_override_unlisted_model_falls_back_to_provider_default() {
        for model in ["glm-5.2", "brand-new-model"] {
            let tempdir = tempfile::tempdir().expect("tempdir");
            let config = hermes_opencode_config(Some(model));

            provision_hermes_config(&config, tempdir.path(), Some(&endpoint("opencode")))
                .expect("provision with override");

            let value = hermes_config_value(tempdir.path());
            assert_eq!(
                value["providers"]["acps-managed"]["transport"], "chat_completions",
                "unlisted model `{model}` should fall back to the provider default",
            );
        }
    }

    #[test]
    fn hermes_override_without_configured_model_uses_provider_default() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let config = hermes_opencode_config(None);

        provision_hermes_config(&config, tempdir.path(), Some(&endpoint("opencode")))
            .expect("provision with override");

        let value = hermes_config_value(tempdir.path());
        assert_eq!(
            value["providers"]["acps-managed"]["transport"],
            "chat_completions"
        );
        assert!(
            value["model"]
                .as_mapping()
                .is_some_and(|map| !map.contains_key(YamlValue::String("default".to_owned()))),
            "{value:?}"
        );
    }

    #[test]
    fn hermes_custom_provider_transport_mirrors_the_declared_api() {
        for (api, transport) in [
            (
                crate::config::CustomProviderApi::Responses,
                "codex_responses",
            ),
            (
                crate::config::CustomProviderApi::AnthropicMessages,
                "anthropic_messages",
            ),
        ] {
            let tempdir = tempfile::tempdir().expect("tempdir");
            let config = custom_provider_config("hermes", api);

            provision_hermes_config(&config, tempdir.path(), None).expect("provision");

            let value = hermes_config_value(tempdir.path());
            assert_eq!(
                value["providers"]["acps-managed"]["transport"], transport,
                "api {api:?} should map to {transport}",
            );
        }
    }

    #[test]
    fn hermes_override_reprovision_is_byte_identical() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let config = hermes_openrouter_config();
        let path = hermes_config_path(tempdir.path());

        provision_hermes_config(&config, tempdir.path(), Some(&endpoint("openrouter")))
            .expect("first provision");
        let first = std::fs::read(&path).expect("first config readable");
        provision_hermes_config(&config, tempdir.path(), Some(&endpoint("openrouter")))
            .expect("second provision");
        let second = std::fs::read(&path).expect("second config readable");

        assert_eq!(first, second, "re-provision must be a fixed point");
    }

    #[test]
    fn hermes_override_preserves_user_providers_and_model_keys() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = hermes_config_path(tempdir.path());
        std::fs::create_dir_all(path.parent().expect("path has parent")).expect("create parent");
        std::fs::write(
            &path,
            "model:\n  provider: openai\n  default: openai:gpt-5\n  context_length: 128000\nskills_dir: keep\nproviders:\n  other:\n    name: Other Provider\n    base_url: https://api.other.example/v1\n    key_env: OTHER_API_KEY\n    transport: chat_completions\n",
        )
        .expect("write existing config");
        let config = hermes_openrouter_config();

        provision_hermes_config(&config, tempdir.path(), Some(&endpoint("openrouter")))
            .expect("provision with override");

        let value = hermes_config_value(tempdir.path());
        assert_eq!(value["model"]["provider"], "custom:acps-managed");
        assert_eq!(value["model"]["context_length"], 128000);
        assert_eq!(value["skills_dir"], "keep");
        assert_eq!(value["providers"]["other"]["name"], "Other Provider");
        assert_eq!(
            value["providers"]["other"]["base_url"],
            "https://api.other.example/v1"
        );
        assert_eq!(
            value["providers"]["acps-managed"]["transport"],
            "chat_completions"
        );
    }

    #[test]
    fn hermes_override_strips_the_advertised_prefix_with_the_mapped_provider_id() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let mut config = hermes_openrouter_config();
        config
            .agent
            .provider
            .as_mut()
            .expect("provider")
            .model
            .replace("openrouter:deepseek/deepseek-v4-flash".to_owned());

        provision_hermes_config(&config, tempdir.path(), Some(&endpoint("openrouter")))
            .expect("provision with override");

        let value = hermes_config_value(tempdir.path());
        assert_eq!(value["model"]["provider"], "custom:acps-managed");
        assert_eq!(value["model"]["default"], "deepseek/deepseek-v4-flash");
    }

    #[test]
    fn hermes_endpoint_for_another_provider_is_ignored() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let config = hermes_openrouter_config();

        provision_hermes_config(&config, tempdir.path(), Some(&endpoint("anthropic")))
            .expect("provision");

        let value = hermes_config_value(tempdir.path());
        assert_eq!(value["model"]["provider"], "openrouter");
        assert!(
            value["model"]
                .as_mapping()
                .is_some_and(|map| !map.contains_key(YamlValue::String("base_url".to_owned()))),
            "{value:?}"
        );
        assert!(value["providers"].is_null(), "{value:?}");
    }

    #[test]
    fn hermes_custom_provider_endpoint_overrides_the_declared_base_url() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let config =
            custom_provider_config("hermes", crate::config::CustomProviderApi::ChatCompletions);
        let provider_id = config
            .agent
            .provider
            .as_ref()
            .expect("custom provider")
            .id
            .clone();

        provision_hermes_config(&config, tempdir.path(), Some(&endpoint(&provider_id)))
            .expect("provision");

        let value = hermes_config_value(tempdir.path());
        assert_eq!(value["model"]["provider"], "custom:acps-managed");
        assert_eq!(
            value["providers"]["acps-managed"]["base_url"],
            "http://127.0.0.1:3129/openrouter"
        );
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
    fn hermes_cleanup_removes_only_managed_keys() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let config = config_with_agent("hermes", &["OPENROUTER_API_KEY"]);
        let path = tempdir.path().join(".hermes").join("config.yaml");
        std::fs::create_dir_all(path.parent().expect("path has parent")).expect("create parent");
        std::fs::write(
            &path,
            "model:\n  provider: custom:acps-managed\n  default: deepseek/deepseek-v4-flash\n  context_length: 128000\nkeep_me: yes\nproviders:\n  acps-managed:\n    name: OpenRouter (managed endpoint)\n    base_url: http://127.0.0.1:3129/openrouter\n    key_env: OPENROUTER_API_KEY\n    transport: chat_completions\n  other:\n    name: Other Provider\n    base_url: https://api.other.example/v1\n",
        )
        .expect("write hermes config");

        let cleaned = cleanup_agent_headless_config(&config, tempdir.path()).expect("cleanup");

        assert_eq!(cleaned.len(), 1);
        let value: serde_norway::Value =
            serde_norway::from_str(&std::fs::read_to_string(&path).expect("hermes readable"))
                .expect("hermes yaml parses");
        assert_eq!(value["keep_me"], "yes");
        assert_eq!(value["model"]["context_length"], 128000);
        assert!(value["providers"]["acps-managed"].is_null(), "{value:?}");
        assert_eq!(value["providers"]["other"]["name"], "Other Provider");
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
        std::fs::write(
            &path,
            "model:\n  provider: custom:acps-managed\nproviders:\n  acps-managed:\n    name: OpenRouter (managed endpoint)\n    base_url: http://127.0.0.1:3129/openrouter\n    key_env: OPENROUTER_API_KEY\n    transport: chat_completions\n",
        )
        .expect("write hermes config");

        let cleaned = cleanup_agent_headless_config(&config, tempdir.path()).expect("cleanup");

        assert_eq!(cleaned.len(), 1);
        assert!(!path.exists());
    }
}
