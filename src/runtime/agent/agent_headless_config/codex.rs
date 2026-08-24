use super::*;

pub(crate) const CODEX_OPENROUTER_PROVIDER_ID: &str = "openrouter";
// Codex uses OpenRouter's Responses-compatible endpoint instead of the chat
// completions endpoint most OpenRouter clients configure by default.
const CODEX_OPENROUTER_RESPONSES_BASE_URL: &str = "https://openrouter.ai/api/v1";
/// Routing prefix some clients carry in front of an OpenRouter slug. OpenRouter
/// itself expects the provider-native `vendor/model` form.
const CODEX_OPENROUTER_SLUG_PREFIX: &str = "openrouter/";

pub(super) fn provision_codex_config(
    config: &Config,
    home: &Path,
    endpoint: Option<&crate::secrets::ProviderEndpointOverride>,
) -> Result<Vec<PathBuf>> {
    let mut written = Vec::new();
    if let Some(path) = provision_codex_main_config(config, home, endpoint)? {
        written.push(path);
    }
    Ok(written)
}

pub(super) fn cleanup_codex_config(
    config: &Config,
    home: &Path,
) -> Result<Vec<CleanedAgentConfig>> {
    let path = home.join(".codex").join("config.toml");
    if !path.exists() {
        return Ok(Vec::new());
    }
    let mut root = read_toml_table(&path)?;
    let mut changed = root.remove("model").is_some();
    if let Some(provider_key) = codex_provider_config_key(config) {
        if root.get("model_provider").and_then(TomlValue::as_str) == Some(provider_key.as_str()) {
            root.remove("model_provider");
            changed = true;
        }
        if provider_key != "openai" {
            let mut remove_providers_table = false;
            if let Some(providers) = root
                .get_mut("model_providers")
                .and_then(TomlValue::as_table_mut)
            {
                changed |= providers.remove(&provider_key).is_some();
                remove_providers_table = providers.is_empty();
            }
            if remove_providers_table {
                root.remove("model_providers");
            }
        }
    }
    if !changed {
        return Ok(Vec::new());
    }
    write_or_remove_toml_table(&path, root)?;
    Ok(vec![CleanedAgentConfig {
        label: "Codex config",
        path,
    }])
}

fn codex_provider_config_key(config: &Config) -> Option<String> {
    let provider = config.agent.provider.as_ref()?;
    if provider.id == CODEX_OPENROUTER_PROVIDER_ID || provider.id == "openai" {
        return Some(provider.id.clone());
    }
    provider.custom.as_ref().map(|_| provider.id.clone())
}

fn provision_codex_main_config(
    config: &Config,
    home: &Path,
    endpoint: Option<&crate::secrets::ProviderEndpointOverride>,
) -> Result<Option<PathBuf>> {
    let path = home.join(".codex").join("config.toml");
    let Some(provider) = config.agent.provider.as_ref() else {
        return Ok(None);
    };
    let base_url_override = super::endpoint_base_url_for(endpoint, &provider.id);
    if let Some(custom) = provider.custom.as_ref() {
        if custom.api != CustomProviderApi::Responses {
            return Err(StackError::AgentConfigProvision {
                path,
                reason: "codex custom providers only support responses".to_owned(),
            });
        }
        let Some(model) = configured_provider_model(config) else {
            return Ok(None);
        };
        let api_key_ref = require_agent_env_for_provider(config, &provider.id, &path)?;
        let mut root = read_toml_table(&path)?;
        write_codex_custom_provider_selection(
            &mut root,
            &provider.id,
            model,
            custom,
            api_key_ref,
            &path,
            base_url_override,
        )?;
        write_toml_table(&path, root)?;
        return Ok(Some(path));
    }
    if provider.id == CODEX_OPENAI_PROVIDER_ID {
        // Codex reserves `openai` for its built-in definition, whose required
        // shape is version-dependent; a synthesized table would fail at request
        // time instead of here.
        if base_url_override.is_some() {
            return Err(StackError::AgentConfigProvision {
                path,
                reason: "codex cannot route the built-in `openai` provider through a custom \
                         endpoint; select `openrouter` or a custom provider instead"
                    .to_owned(),
            });
        }
        return provision_codex_openai_config(config, &path);
    }
    if provider.id != CODEX_OPENROUTER_PROVIDER_ID {
        return Err(StackError::AgentConfigProvision {
            path,
            reason: format!(
                "codex provider `{}` is not supported; use `openai` or `openrouter`",
                provider.id
            ),
        });
    }
    let model_opt = configured_provider_model(config).map(str::to_owned);
    let api_key_ref = require_agent_env_for_provider(config, CODEX_OPENROUTER_PROVIDER_ID, &path)?;
    let Some(native_ref) =
        env_var_for_agent_provider_id(&config.agent.id, CODEX_OPENROUTER_PROVIDER_ID)
    else {
        return Err(StackError::AgentConfigProvision {
            path: path.clone(),
            reason: "codex OpenRouter has no API-key env mapping in provider/env mapping"
                .to_owned(),
        });
    };
    if api_key_ref != native_ref {
        return Err(StackError::AgentConfigProvision {
            path: path.clone(),
            reason: format!(
                "codex OpenRouter requires provider-native env ref `{native_ref}`, got `{api_key_ref}`"
            ),
        });
    }

    // OpenRouter answers an unknown slug with an empty turn instead of an
    // error, so a routing-prefixed slug fails invisibly at prompt time. Only
    // the double-qualified shape is rejectable: `openrouter/auto` and friends
    // are genuine catalog ids under OpenRouter's own vendor.
    if let Some(model) = model_opt.as_deref().filter(|model| {
        model
            .strip_prefix(CODEX_OPENROUTER_SLUG_PREFIX)
            .is_some_and(|remainder| remainder.contains('/'))
    }) {
        return Err(StackError::AgentConfigProvision {
            path,
            reason: format!(
                "codex OpenRouter model `{model}` must be the provider-native slug \
                 (e.g. `deepseek/deepseek-v4-flash-0731`), not `{CODEX_OPENROUTER_SLUG_PREFIX}`-prefixed"
            ),
        });
    }

    let mut root = read_toml_table(&path)?;
    // Settle the provider table even with no model selected: a
    // `model_provider = "openrouter"` without a matching table leaves the
    // launched harness unable to resolve auth.
    match model_opt.as_deref() {
        Some(model) => {
            root.insert("model".to_owned(), TomlValue::String(model.to_owned()));
        }
        None => {
            root.remove("model");
        }
    }
    root.insert(
        "model_provider".to_owned(),
        TomlValue::String(CODEX_OPENROUTER_PROVIDER_ID.to_owned()),
    );
    let Some(provider_name) = provider_name_for_provider_id(CODEX_OPENROUTER_PROVIDER_ID) else {
        return Err(StackError::AgentConfigProvision {
            path: path.clone(),
            reason: "codex OpenRouter has no provider metadata in provider/env mapping".to_owned(),
        });
    };
    let providers = ensure_toml_table_field(&mut root, "model_providers", &path)?;
    let openrouter = ensure_toml_table_field(providers, CODEX_OPENROUTER_PROVIDER_ID, &path)?;
    openrouter.insert(
        "name".to_owned(),
        TomlValue::String(provider_name.to_owned()),
    );
    openrouter.insert(
        "base_url".to_owned(),
        TomlValue::String(
            base_url_override
                .unwrap_or(CODEX_OPENROUTER_RESPONSES_BASE_URL)
                .to_owned(),
        ),
    );
    // Command-based auth, not env_key: a plain env_key authenticates but skips
    // Codex's model-catalog refresh, leaving fallback metadata behind.
    openrouter.remove("env_key");
    let mut auth = TomlMap::new();
    auth.insert("command".to_owned(), TomlValue::String("sh".to_owned()));
    auth.insert(
        "args".to_owned(),
        TomlValue::Array(vec![
            TomlValue::String("-c".to_owned()),
            TomlValue::String(format!("echo ${native_ref}")),
        ]),
    );
    openrouter.insert("auth".to_owned(), TomlValue::Table(auth));
    openrouter.insert(
        "wire_api".to_owned(),
        TomlValue::String("responses".to_owned()),
    );

    write_toml_table(&path, root)?;
    Ok(Some(path))
}

fn write_codex_custom_provider_selection(
    root: &mut TomlMap<String, TomlValue>,
    provider_id: &str,
    model: &str,
    custom: &AgentCustomProviderConfig,
    api_key_ref: &str,
    path: &Path,
    base_url_override: Option<&str>,
) -> Result<()> {
    root.insert("model".to_owned(), TomlValue::String(model.to_owned()));
    root.insert(
        "model_provider".to_owned(),
        TomlValue::String(provider_id.to_owned()),
    );
    let providers = ensure_toml_table_field(root, "model_providers", path)?;
    let custom_provider = ensure_toml_table_field(providers, provider_id, path)?;
    custom_provider.insert("name".to_owned(), TomlValue::String(custom.name.clone()));
    custom_provider.insert(
        "base_url".to_owned(),
        TomlValue::String(base_url_override.unwrap_or(&custom.base_url).to_owned()),
    );
    custom_provider.insert(
        "env_key".to_owned(),
        TomlValue::String(api_key_ref.to_owned()),
    );
    custom_provider.insert(
        "wire_api".to_owned(),
        TomlValue::String(custom.api.as_codex_wire_api().to_owned()),
    );
    Ok(())
}

fn provision_codex_openai_config(config: &Config, path: &Path) -> Result<Option<PathBuf>> {
    let Some(model) = configured_provider_model(config) else {
        // Provider switched to openai with no model: clear any model a prior
        // run wrote so the harness does not keep using it under the new lane.
        if !path.exists() {
            return Ok(None);
        }
        let mut root = read_toml_table(path)?;
        let removed_model = root.remove("model").is_some();
        let prior_provider = root
            .get("model_provider")
            .and_then(TomlValue::as_str)
            .map(str::to_owned);
        let provider_changed = prior_provider
            .as_deref()
            .is_some_and(|prior| prior != "openai");
        if provider_changed {
            root.insert(
                "model_provider".to_owned(),
                TomlValue::String("openai".to_owned()),
            );
        }
        if removed_model || provider_changed {
            write_toml_table(path, root)?;
            return Ok(Some(path.to_path_buf()));
        }
        return Ok(None);
    };
    let mut root = read_toml_table(path)?;
    if let Some(provider_id) = codex_custom_provider_to_remove(&root) {
        backup_codex_config(path, &provider_id)?;
        if let Some(providers) = root
            .get_mut("model_providers")
            .and_then(TomlValue::as_table_mut)
        {
            providers.remove(&provider_id);
            if providers.is_empty() {
                root.remove("model_providers");
            }
        }
    }
    root.insert("model".to_owned(), TomlValue::String(model.to_owned()));
    root.insert(
        "model_provider".to_owned(),
        TomlValue::String("openai".to_owned()),
    );
    write_toml_table(path, root)?;
    Ok(Some(path.to_path_buf()))
}

fn codex_custom_provider_to_remove(root: &TomlMap<String, TomlValue>) -> Option<String> {
    let model_provider = root.get("model_provider").and_then(TomlValue::as_str)?;
    if model_provider == "openai" {
        return None;
    }
    let providers = root.get("model_providers").and_then(TomlValue::as_table)?;
    providers
        .contains_key(model_provider)
        .then(|| model_provider.to_owned())
}

fn backup_codex_config(path: &Path, provider_id: &str) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let parent = parent_dir(path)?;
    let backup_path = unique_codex_backup_path(parent, provider_id);
    std::fs::copy(path, &backup_path).map_err(|source| StackError::ConfigWrite {
        path: backup_path,
        source,
    })?;
    Ok(())
}

fn unique_codex_backup_path(parent: &Path, provider_id: &str) -> PathBuf {
    let first = parent.join(format!("config.{provider_id}.toml"));
    if !first.exists() {
        return first;
    }
    for index in 1.. {
        let path = parent.join(format!("config.{provider_id}-{index}.toml"));
        if !path.exists() {
            return path;
        }
    }
    unreachable!("unbounded suffix search returns a backup path")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codex_endpoint(provider_id: &str) -> crate::secrets::ProviderEndpointOverride {
        crate::secrets::ProviderEndpointOverride {
            provider_id: provider_id.to_owned(),
            base_url: "http://127.0.0.1:3129/openrouter".to_owned(),
        }
    }

    fn codex_config_value(home: &Path) -> toml::Value {
        let path = home.join(".codex").join("config.toml");
        toml::from_str(&std::fs::read_to_string(path).expect("codex config should be readable"))
            .expect("codex config toml parses")
    }

    fn codex_openrouter_config() -> Config {
        let mut config = config_with_agent("codex", &["OPENROUTER_API_KEY"]);
        config.agent.provider = Some(crate::config::AgentProviderConfig {
            id: "openrouter".to_owned(),
            model: Some("deepseek/deepseek-v4-flash".to_owned()),
            api_key_ref: Some("OPENROUTER_API_KEY".to_owned()),
            custom: None,
        });
        config
    }

    #[test]
    fn codex_openrouter_endpoint_replaces_the_provider_base_url() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let config = codex_openrouter_config();

        provision_codex_config(&config, tempdir.path(), Some(&codex_endpoint("openrouter")))
            .expect("provision with override");
        let value = codex_config_value(tempdir.path());
        assert_eq!(
            value["model_providers"]["openrouter"]["base_url"].as_str(),
            Some("http://127.0.0.1:3129/openrouter")
        );
        assert_eq!(
            value["model_providers"]["openrouter"]["wire_api"].as_str(),
            Some("responses")
        );

        provision_codex_config(&config, tempdir.path(), None).expect("provision without");
        let value = codex_config_value(tempdir.path());
        assert_eq!(
            value["model_providers"]["openrouter"]["base_url"].as_str(),
            Some("https://openrouter.ai/api/v1")
        );
    }

    #[test]
    fn codex_refuses_an_endpoint_for_the_built_in_openai_provider() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let mut config = config_with_agent("codex", &["OPENAI_API_KEY"]);
        config.agent.provider = Some(crate::config::AgentProviderConfig {
            id: "openai".to_owned(),
            model: Some("gpt-5.5".to_owned()),
            api_key_ref: Some("OPENAI_API_KEY".to_owned()),
            custom: None,
        });

        let error =
            provision_codex_config(&config, tempdir.path(), Some(&codex_endpoint("openai")))
                .expect_err("built-in openai endpoint must be refused");

        assert!(error.to_string().contains("openrouter"), "{error}");
    }

    #[test]
    fn codex_custom_provider_endpoint_overrides_the_declared_base_url() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let config = custom_provider_config("codex", crate::config::CustomProviderApi::Responses);

        provision_codex_config(&config, tempdir.path(), Some(&codex_endpoint("myprovider")))
            .expect("provision");

        let value = codex_config_value(tempdir.path());
        assert_eq!(
            value["model_providers"]["myprovider"]["base_url"].as_str(),
            Some("http://127.0.0.1:3129/openrouter")
        );
    }

    #[test]
    fn codex_openrouter_writes_responses_provider_config() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let mut config = config_with_agent("codex", &["OPENROUTER_API_KEY"]);
        config.agent.provider = Some(crate::config::AgentProviderConfig {
            id: "openrouter".to_owned(),
            model: Some("deepseek/deepseek-v4-flash".to_owned()),
            api_key_ref: Some("OPENROUTER_API_KEY".to_owned()),
            custom: None,
        });

        let provisioned =
            provision_agent_headless_config(&config, tempdir.path()).expect("provision");

        let path = tempdir.path().join(".codex").join("config.toml");
        assert_eq!(provisioned[0].path, path);
        let value: toml::Value = toml::from_str(
            &std::fs::read_to_string(&path).expect("codex config should be readable"),
        )
        .expect("codex config toml parses");
        assert_eq!(value["model"].as_str(), Some("deepseek/deepseek-v4-flash"));
        assert_eq!(value["model_provider"].as_str(), Some("openrouter"));
        assert_eq!(
            value["model_providers"]["openrouter"]["base_url"].as_str(),
            Some("https://openrouter.ai/api/v1")
        );
        assert_eq!(
            value["model_providers"]["openrouter"]["name"].as_str(),
            Some("OpenRouter")
        );
        assert!(
            value["model_providers"]["openrouter"]
                .get("env_key")
                .is_none(),
            "command-based auth replaces env_key"
        );
        assert_eq!(
            value["model_providers"]["openrouter"]["auth"]["command"].as_str(),
            Some("sh")
        );
        assert_eq!(
            value["model_providers"]["openrouter"]["auth"]["args"]
                .as_array()
                .map(|args| args
                    .iter()
                    .filter_map(TomlValue::as_str)
                    .collect::<Vec<_>>()),
            Some(vec!["-c", "echo $OPENROUTER_API_KEY"])
        );
        assert_eq!(
            value["model_providers"]["openrouter"]["wire_api"].as_str(),
            Some("responses")
        );
    }

    #[test]
    fn codex_openrouter_rejects_a_double_qualified_model_slug() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let mut config = config_with_agent("codex", &["OPENROUTER_API_KEY"]);
        config.agent.provider = Some(crate::config::AgentProviderConfig {
            id: "openrouter".to_owned(),
            model: Some("openrouter/deepseek/deepseek-v4-flash".to_owned()),
            api_key_ref: Some("OPENROUTER_API_KEY".to_owned()),
            custom: None,
        });

        let error = provision_agent_headless_config(&config, tempdir.path())
            .expect_err("double-qualified slug must be refused");

        assert!(
            error
                .to_string()
                .contains("deepseek/deepseek-v4-flash-0731"),
            "{error}"
        );
        assert!(
            !tempdir.path().join(".codex").join("config.toml").exists(),
            "a refused model must not leave a provisioned config behind"
        );
    }

    #[test]
    fn codex_openrouter_accepts_openrouter_vendor_router_models() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let mut config = config_with_agent("codex", &["OPENROUTER_API_KEY"]);
        config.agent.provider = Some(crate::config::AgentProviderConfig {
            id: "openrouter".to_owned(),
            model: Some("openrouter/auto".to_owned()),
            api_key_ref: Some("OPENROUTER_API_KEY".to_owned()),
            custom: None,
        });

        provision_agent_headless_config(&config, tempdir.path()).expect("provision");

        let written = std::fs::read_to_string(tempdir.path().join(".codex").join("config.toml"))
            .expect("config written");
        assert!(written.contains("model = \"openrouter/auto\""), "{written}");
    }

    #[test]
    fn codex_custom_provider_writes_responses_provider_config() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let config = custom_provider_config("codex", crate::config::CustomProviderApi::Responses);

        provision_agent_headless_config(&config, tempdir.path()).expect("provision");

        let path = tempdir.path().join(".codex").join("config.toml");
        let value: toml::Value = toml::from_str(
            &std::fs::read_to_string(path).expect("codex config should be readable"),
        )
        .expect("codex config toml parses");
        assert_eq!(value["model"].as_str(), Some("my-model"));
        assert_eq!(value["model_provider"].as_str(), Some("myprovider"));
        assert_eq!(
            value["model_providers"]["myprovider"]["base_url"].as_str(),
            Some("https://api.myprovider.example/v1")
        );
        assert_eq!(
            value["model_providers"]["myprovider"]["env_key"].as_str(),
            Some("CUSTOM_API_KEY")
        );
        assert_eq!(
            value["model_providers"]["myprovider"]["wire_api"].as_str(),
            Some("responses")
        );
    }

    #[test]
    fn codex_custom_provider_rejects_chat_completions() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let config =
            custom_provider_config("codex", crate::config::CustomProviderApi::ChatCompletions);

        let err = provision_agent_headless_config(&config, tempdir.path()).expect_err("fails");

        assert!(
            err.to_string()
                .contains("codex custom providers only support responses"),
            "{err}"
        );
    }

    #[test]
    fn codex_openai_model_removes_custom_provider_and_writes_backup() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let codex_dir = tempdir.path().join(".codex");
        std::fs::create_dir_all(&codex_dir).expect("create codex config dir");
        let path = codex_dir.join("config.toml");
        std::fs::write(
            &path,
            r#"model = "deepseek/deepseek-v4-flash"
model_provider = "openrouter"
preserve = "yes"

[model_providers.openrouter]
name = "OpenRouter"
base_url = "https://openrouter.ai/api/v1/responses"
env_key = "OPENROUTER_API_KEY"
wire_api = "responses"
"#,
        )
        .expect("write existing codex config");
        std::fs::write(codex_dir.join("config.openrouter.toml"), "occupied\n")
            .expect("write existing backup");
        let mut config = config_with_agent("codex", &[]);
        config.agent.provider = Some(crate::config::AgentProviderConfig {
            id: "openai".to_owned(),
            model: Some("gpt-5.5".to_owned()),
            api_key_ref: None,
            custom: None,
        });

        let provisioned =
            provision_agent_headless_config(&config, tempdir.path()).expect("provision");

        assert_eq!(provisioned[0].path, path);
        let value: toml::Value = toml::from_str(
            &std::fs::read_to_string(&path).expect("codex config should be readable"),
        )
        .expect("codex config toml parses");
        assert_eq!(value["model"].as_str(), Some("gpt-5.5"));
        assert_eq!(value["model_provider"].as_str(), Some("openai"));
        assert_eq!(value["preserve"].as_str(), Some("yes"));
        assert!(
            value.get("model_providers").is_none(),
            "openrouter provider table should be removed"
        );
        let backup = std::fs::read_to_string(codex_dir.join("config.openrouter-1.toml"))
            .expect("backup should be written with suffix");
        assert!(backup.contains(r#"model_provider = "openrouter""#));
        assert!(backup.contains("[model_providers.openrouter]"));
    }

    #[test]
    fn codex_cleanup_removes_managed_provider_and_keeps_unrelated_config() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let config = custom_provider_config("codex", crate::config::CustomProviderApi::Responses);
        let path = tempdir.path().join(".codex").join("config.toml");
        std::fs::create_dir_all(path.parent().expect("path has parent")).expect("create parent");
        std::fs::write(
            &path,
            r#"model = "my-model"
model_provider = "myprovider"
approval_policy = "on-request"

[model_providers.myprovider]
name = "My Provider"
base_url = "https://api.myprovider.example/v1"
env_key = "CUSTOM_API_KEY"
wire_api = "responses"

[model_providers.other]
name = "Other"
base_url = "https://api.other.example/v1"
env_key = "OTHER_API_KEY"
wire_api = "responses"
"#,
        )
        .expect("write codex config");

        let cleaned = cleanup_agent_headless_config(&config, tempdir.path()).expect("cleanup");

        assert_eq!(cleaned[0].path, path);
        let value: toml::Value = toml::from_str(
            &std::fs::read_to_string(&path).expect("codex config should be readable"),
        )
        .expect("codex config toml parses");
        assert_eq!(value["approval_policy"].as_str(), Some("on-request"));
        assert!(value.get("model").is_none());
        assert!(value.get("model_provider").is_none());
        assert!(value["model_providers"].get("myprovider").is_none());
        assert_eq!(
            value["model_providers"]["other"]["base_url"].as_str(),
            Some("https://api.other.example/v1")
        );
    }
}
