use super::*;

use crate::runtime::agent::provider_model_catalog::cached_models;

const CLAUDE_CODE_API_KEY_HELPER_PREFIX: &str = "printenv ";

pub(super) fn provision_claude_code_config(
    config: &Config,
    home: &Path,
    endpoint: Option<&crate::secrets::ProviderEndpointOverride>,
) -> Result<Vec<PathBuf>> {
    let mut written = Vec::new();
    let Some(provider) = config.agent.provider.as_ref() else {
        // A provider-less config still must not leave a previous provider's
        // model allowlist behind — it would silently constrain every session.
        let settings_path = home.join(".claude").join("settings.json");
        if settings_path.exists() {
            // Unreadable settings degrade to "nothing to strip": this path
            // was a no-op before the allowlist existed, and a hand-broken
            // file must not block a provider-less provision.
            match read_json_object(&settings_path) {
                Ok(mut settings) => {
                    if settings.remove("availableModels").is_some() {
                        write_json_object(&settings_path, settings)?;
                        written.push(settings_path);
                    }
                }
                Err(error) => {
                    tracing::warn!(path = %settings_path.display(), error = %error, "unreadable Claude Code settings; leaving availableModels in place");
                }
            }
        }
        return Ok(written);
    };
    let settings_path = home.join(".claude").join("settings.json");
    let onboarding_path = home.join(".claude.json");
    let mut settings = read_json_object(&settings_path)?;
    let remove_env = {
        let env = ensure_object_field(&mut settings, "env", &settings_path)?;
        remove_claude_managed_env(env);
        write_claude_provider_env(config, provider, env, &settings_path, endpoint)?;
        env.is_empty()
    };
    if remove_env {
        settings.remove("env");
    }
    write_claude_api_key_helper(config, provider, &mut settings, &settings_path)?;
    write_claude_available_models(provider, &mut settings, home);
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
    endpoint: Option<&crate::secrets::ProviderEndpointOverride>,
) -> Result<Vec<CleanedAgentConfig>> {
    let mut cleaned = Vec::new();
    let Some(provider) = config.agent.provider.as_ref() else {
        return Ok(cleaned);
    };
    let settings_path = home.join(".claude").join("settings.json");
    // The expected env must be rendered with the same override that wrote it,
    // or the endpoint key fails the value match and survives the cleanup.
    let expected_env = claude_provider_env_for_config(config, provider, &settings_path, endpoint)?;
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
        changed |= settings.remove("availableModels").is_some();
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
    endpoint: Option<&crate::secrets::ProviderEndpointOverride>,
) -> Result<Map<String, serde_json::Value>> {
    let mut env = Map::new();
    write_claude_provider_env(config, provider, &mut env, path, endpoint)?;
    Ok(env)
}

fn write_claude_provider_env(
    config: &Config,
    provider: &AgentProviderConfig,
    env: &mut Map<String, serde_json::Value>,
    path: &Path,
    endpoint: Option<&crate::secrets::ProviderEndpointOverride>,
) -> Result<()> {
    write_claude_provider_env_inner(config, provider, env, path)?;
    // Last write wins over both the custom provider's own base URL and the
    // profile default; `ANTHROPIC_BASE_URL` is already an acps-managed key, so
    // no new file surface is involved.
    if let Some(base_url) = super::endpoint_base_url_for(endpoint, &provider.id) {
        env.insert("ANTHROPIC_BASE_URL".to_owned(), json!(base_url));
    }
    Ok(())
}

fn write_claude_provider_env_inner(
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

    let Some(profile) = claude_code_profile_for_provider_id(&provider.id) else {
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
        // Profile env keys such as DeepSeek's CLAUDE_CODE_SUBAGENT_MODEL stay
        // in effect under an explicit model pin so the provider's recommended
        // cheap subagent routing is preserved; set_subagent_model profiles
        // still re-point the subagent at the pinned model below.
        insert_claude_model_env(env, model, profile.set_subagent_model);
    } else {
        insert_claude_profile_default_model_env(env, profile);
    }
    Ok(())
}

/// Env keys whose values Claude Code resolves as model ids and must therefore
/// survive the `availableModels` allowlist. `insert_claude_model_env` writes
/// from this same list so a new role key can never be pinned without also
/// being unioned into the allowlist.
const CLAUDE_MODEL_ENV_KEYS: &[&str] = &[
    "ANTHROPIC_MODEL",
    "ANTHROPIC_DEFAULT_FABLE_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    CLAUDE_SUBAGENT_MODEL_ENV_KEY,
];
const CLAUDE_SUBAGENT_MODEL_ENV_KEY: &str = "CLAUDE_CODE_SUBAGENT_MODEL";

/// Surface the provider's live model catalog to the claude-agent-acp adapter.
///
/// Entries in settings.json `availableModels` are advertised verbatim as ACP
/// model values, which is the only channel through which a third-party
/// provider's models become selectable — the CLI itself never queries the
/// provider. The key is removed rather than left stale whenever there is no
/// catalog for the active provider (first-party Anthropic, native-auth lanes,
/// custom providers, or an offline fetch), degrading to the builtin aliases
/// plus the env-pinned model.
///
/// Claude Code treats `availableModels` as an allowlist: a pinned or role
/// model outside it is silently dropped, not merely hidden from the picker.
/// Profile pins use alias forms the provider's listing never returns (e.g.
/// `kimi-k3[1m]`), so every model value the same provisioning run put into
/// `env` is unioned in ahead of the catalog.
fn write_claude_available_models(
    provider: &AgentProviderConfig,
    settings: &mut Map<String, serde_json::Value>,
    home: &Path,
) {
    let catalog = if provider.custom.is_some() {
        None
    } else {
        claude_code_profile_for_provider_id(&provider.id)
            .filter(|profile| profile.base_url.is_some() && !profile.agent_native_auth)
            .and_then(|_| cached_models(home, &provider.id))
    };
    match catalog {
        Some(models) => {
            let mut values: Vec<String> = Vec::new();
            if let Some(env) = settings.get("env").and_then(serde_json::Value::as_object) {
                for key in CLAUDE_MODEL_ENV_KEYS {
                    if let Some(model) = env.get(*key).and_then(serde_json::Value::as_str)
                        && !model.trim().is_empty()
                        && !values.iter().any(|existing| existing == model)
                    {
                        values.push(model.to_owned());
                    }
                }
            }
            for model in models {
                if !values.contains(&model.value) {
                    values.push(model.value);
                }
            }
            settings.insert("availableModels".to_owned(), json!(values));
        }
        None => {
            settings.remove("availableModels");
        }
    }
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
    let Some(profile) = claude_code_profile_for_provider_id(&provider.id) else {
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
    for key in CLAUDE_MODEL_ENV_KEYS {
        if *key == CLAUDE_SUBAGENT_MODEL_ENV_KEY && !set_subagent_model {
            continue;
        }
        env.insert((*key).to_owned(), json!(model));
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

    fn claude_endpoint(provider_id: &str) -> crate::secrets::ProviderEndpointOverride {
        crate::secrets::ProviderEndpointOverride {
            provider_id: provider_id.to_owned(),
            base_url: "http://127.0.0.1:3129/anthropic".to_owned(),
        }
    }

    fn claude_settings_value(home: &Path) -> Value {
        let path = home.join(".claude").join("settings.json");
        serde_json::from_str(&std::fs::read_to_string(path).expect("settings should be readable"))
            .expect("settings json parses")
    }

    fn claude_moonshot_config() -> Config {
        let mut config = config_with_agent("claude-code", &["MOONSHOT_API_KEY"]);
        config.agent.provider = Some(crate::config::AgentProviderConfig {
            id: "moonshotai".to_owned(),
            model: None,
            api_key_ref: Some("MOONSHOT_API_KEY".to_owned()),
            custom: None,
        });
        config
    }

    #[test]
    fn claude_code_endpoint_overrides_the_profile_base_url_and_restores_it() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let config = claude_moonshot_config();

        provision_claude_code_config(
            &config,
            tempdir.path(),
            Some(&claude_endpoint("moonshotai")),
        )
        .expect("provision with override");
        assert_eq!(
            claude_settings_value(tempdir.path())["env"]["ANTHROPIC_BASE_URL"],
            "http://127.0.0.1:3129/anthropic"
        );

        provision_claude_code_config(&config, tempdir.path(), None).expect("provision without");
        assert_eq!(
            claude_settings_value(tempdir.path())["env"]["ANTHROPIC_BASE_URL"],
            "https://api.moonshot.ai/anthropic"
        );
    }

    #[test]
    fn claude_code_endpoint_for_another_provider_is_ignored() {
        let tempdir = tempfile::tempdir().expect("tempdir");

        provision_claude_code_config(
            &claude_moonshot_config(),
            tempdir.path(),
            Some(&claude_endpoint("anthropic")),
        )
        .expect("provision");

        assert_eq!(
            claude_settings_value(tempdir.path())["env"]["ANTHROPIC_BASE_URL"],
            "https://api.moonshot.ai/anthropic"
        );
    }

    #[test]
    fn claude_code_cleanup_removes_the_overridden_endpoint() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let config = claude_moonshot_config();
        let endpoint = claude_endpoint("moonshotai");
        provision_claude_code_config(&config, tempdir.path(), Some(&endpoint))
            .expect("provision with override");

        cleanup_claude_code_config(&config, tempdir.path(), Some(&endpoint)).expect("cleanup");

        // The managed env was the file's only content, so cleanup removes the
        // file outright rather than leaving an empty settings object.
        let settings_path = tempdir.path().join(".claude").join("settings.json");
        let settings: Option<Value> = std::fs::read_to_string(&settings_path)
            .ok()
            .map(|text| serde_json::from_str(&text).expect("settings json parses"));
        assert!(
            settings
                .as_ref()
                .is_none_or(|settings| settings["env"]["ANTHROPIC_BASE_URL"].is_null()),
            "{settings:?}"
        );
    }

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
        assert_eq!(settings["env"]["ANTHROPIC_MODEL"], "kimi-k3[1m]");
        assert_eq!(
            settings["env"]["ANTHROPIC_DEFAULT_FABLE_MODEL"],
            "kimi-k3[1m]"
        );
        assert_eq!(
            settings["env"]["ANTHROPIC_DEFAULT_OPUS_MODEL"],
            "kimi-k3[1m]"
        );
        assert_eq!(
            settings["env"]["ANTHROPIC_DEFAULT_SONNET_MODEL"],
            "kimi-k3[1m]"
        );
        assert_eq!(
            settings["env"]["ANTHROPIC_DEFAULT_HAIKU_MODEL"],
            "kimi-k3[1m]"
        );
        assert_eq!(settings["env"]["CLAUDE_CODE_SUBAGENT_MODEL"], "kimi-k3[1m]");
        assert_eq!(settings["env"]["ENABLE_TOOL_SEARCH"], "false");
        assert_eq!(
            settings["env"]["CLAUDE_CODE_AUTO_COMPACT_WINDOW"],
            "1048576"
        );
        assert_eq!(settings["env"]["CLAUDE_CODE_EFFORT_LEVEL"], "max");
        assert_eq!(settings["apiKeyHelper"], "printenv MOONSHOT_API_KEY");
        assert!(!settings.to_string().contains("sk-"));

        let onboarding_path = tempdir.path().join(".claude.json");
        let onboarding: Value = serde_json::from_str(
            &std::fs::read_to_string(onboarding_path).expect("onboarding should be readable"),
        )
        .expect("onboarding json parses");
        assert_eq!(onboarding["hasCompletedOnboarding"], true);
    }

    fn seed_provider_model_cache(home: &Path, provider_id: &str, models: &[&str]) {
        let path = crate::runtime::agent::provider_model_catalog::cache_path(home);
        std::fs::create_dir_all(path.parent().expect("cache parent")).expect("mkdir");
        let entries: Vec<serde_json::Value> = models
            .iter()
            .map(|value| json!({ "value": value }))
            .collect();
        let body = json!({
            "version": 1,
            "providers": { provider_id: { "fetched_at": 0, "models": entries } }
        });
        std::fs::write(&path, body.to_string()).expect("write cache");
    }

    fn read_settings(home: &Path) -> Value {
        let settings_path = home.join(".claude").join("settings.json");
        serde_json::from_str(
            &std::fs::read_to_string(&settings_path).expect("settings should be readable"),
        )
        .expect("settings json parses")
    }

    #[test]
    fn claude_code_profiled_provider_writes_available_models_from_cache() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        seed_provider_model_cache(
            tempdir.path(),
            "moonshotai",
            &["kimi-k3", "kimi-k3[1m]", "kimi-k2.7-code"],
        );
        let mut config = config_with_agent("claude-code", &["MOONSHOT_API_KEY"]);
        config.agent.provider = Some(crate::config::AgentProviderConfig {
            id: "moonshotai".to_owned(),
            model: None,
            api_key_ref: Some("MOONSHOT_API_KEY".to_owned()),
            custom: None,
        });

        provision_agent_headless_config(&config, tempdir.path()).expect("provision");

        // The profile pins kimi-k3[1m] into env; env pins lead the list so
        // the allowlist can never drop them, then the catalog follows.
        let settings = read_settings(tempdir.path());
        assert_eq!(
            settings["availableModels"],
            json!(["kimi-k3[1m]", "kimi-k3", "kimi-k2.7-code"])
        );
    }

    #[test]
    fn claude_code_available_models_unions_env_pins_absent_from_catalog() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        // DeepSeek's profile pins [1m]-suffixed aliases the provider's
        // listing endpoint never returns; they must survive the allowlist.
        seed_provider_model_cache(
            tempdir.path(),
            "deepseek",
            &["deepseek-v4-pro", "deepseek-v4-flash"],
        );
        let mut config = config_with_agent("claude-code", &["DEEPSEEK_API_KEY"]);
        config.agent.provider = Some(crate::config::AgentProviderConfig {
            id: "deepseek".to_owned(),
            model: None,
            api_key_ref: Some("DEEPSEEK_API_KEY".to_owned()),
            custom: None,
        });

        provision_agent_headless_config(&config, tempdir.path()).expect("provision");

        let settings = read_settings(tempdir.path());
        assert_eq!(
            settings["availableModels"],
            json!([
                "deepseek-v4-pro[1m]",
                "deepseek-v4-flash",
                "deepseek-v4-pro"
            ])
        );
    }

    #[test]
    fn claude_code_provider_removal_drops_available_models() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        seed_provider_model_cache(tempdir.path(), "moonshotai", &["kimi-k3"]);
        let mut config = config_with_agent("claude-code", &["MOONSHOT_API_KEY"]);
        config.agent.provider = Some(crate::config::AgentProviderConfig {
            id: "moonshotai".to_owned(),
            model: None,
            api_key_ref: Some("MOONSHOT_API_KEY".to_owned()),
            custom: None,
        });
        provision_agent_headless_config(&config, tempdir.path()).expect("provision");
        assert!(
            read_settings(tempdir.path())
                .get("availableModels")
                .is_some()
        );

        config.agent.provider = None;
        provision_agent_headless_config(&config, tempdir.path()).expect("reprovision");

        assert!(
            read_settings(tempdir.path())
                .get("availableModels")
                .is_none()
        );
    }

    #[test]
    fn claude_code_without_cache_omits_available_models() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let mut config = config_with_agent("claude-code", &["MOONSHOT_API_KEY"]);
        config.agent.provider = Some(crate::config::AgentProviderConfig {
            id: "moonshotai".to_owned(),
            model: None,
            api_key_ref: Some("MOONSHOT_API_KEY".to_owned()),
            custom: None,
        });

        provision_agent_headless_config(&config, tempdir.path()).expect("provision");

        assert!(
            read_settings(tempdir.path())
                .get("availableModels")
                .is_none()
        );
    }

    #[test]
    fn claude_code_anthropic_first_party_never_writes_available_models() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        seed_provider_model_cache(tempdir.path(), "anthropic", &["should-not-appear"]);
        let mut config = config_with_agent("claude-code", &["ANTHROPIC_API_KEY"]);
        config.agent.provider = Some(crate::config::AgentProviderConfig {
            id: "anthropic".to_owned(),
            model: None,
            api_key_ref: Some("ANTHROPIC_API_KEY".to_owned()),
            custom: None,
        });

        provision_agent_headless_config(&config, tempdir.path()).expect("provision");

        assert!(
            read_settings(tempdir.path())
                .get("availableModels")
                .is_none()
        );
    }

    #[test]
    fn claude_code_reprovision_after_provider_change_drops_stale_available_models() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        seed_provider_model_cache(tempdir.path(), "moonshotai", &["kimi-k3"]);
        let mut config = config_with_agent("claude-code", &["MOONSHOT_API_KEY"]);
        config.agent.provider = Some(crate::config::AgentProviderConfig {
            id: "moonshotai".to_owned(),
            model: None,
            api_key_ref: Some("MOONSHOT_API_KEY".to_owned()),
            custom: None,
        });
        provision_agent_headless_config(&config, tempdir.path()).expect("provision");
        assert_eq!(
            read_settings(tempdir.path())["availableModels"],
            json!(["kimi-k3[1m]", "kimi-k3"])
        );

        // Switch to a provider with no cache entry: the stale moonshot list
        // must not survive.
        config.agent.env = vec!["DEEPSEEK_API_KEY".to_owned()];
        config.agent.provider = Some(crate::config::AgentProviderConfig {
            id: "deepseek".to_owned(),
            model: None,
            api_key_ref: Some("DEEPSEEK_API_KEY".to_owned()),
            custom: None,
        });
        provision_agent_headless_config(&config, tempdir.path()).expect("reprovision");

        assert!(
            read_settings(tempdir.path())
                .get("availableModels")
                .is_none()
        );
    }

    #[test]
    fn claude_code_cleanup_removes_available_models() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        seed_provider_model_cache(tempdir.path(), "moonshotai", &["kimi-k3"]);
        let mut config = config_with_agent("claude-code", &["MOONSHOT_API_KEY"]);
        config.agent.provider = Some(crate::config::AgentProviderConfig {
            id: "moonshotai".to_owned(),
            model: None,
            api_key_ref: Some("MOONSHOT_API_KEY".to_owned()),
            custom: None,
        });
        provision_agent_headless_config(&config, tempdir.path()).expect("provision");

        cleanup_claude_code_config(&config, tempdir.path(), None).expect("cleanup");

        let settings_path = tempdir.path().join(".claude").join("settings.json");
        if settings_path.exists() {
            assert!(
                read_settings(tempdir.path())
                    .get("availableModels")
                    .is_none()
            );
        }
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
        assert_eq!(settings["env"]["ANTHROPIC_MODEL"], "glm-5.3[1m]");
        assert_eq!(
            settings["env"]["ANTHROPIC_DEFAULT_FABLE_MODEL"],
            "glm-5.3[1m]"
        );
        assert_eq!(
            settings["env"]["ANTHROPIC_DEFAULT_OPUS_MODEL"],
            "glm-5.3[1m]"
        );
        assert_eq!(
            settings["env"]["ANTHROPIC_DEFAULT_SONNET_MODEL"],
            "glm-5.3[1m]"
        );
        assert_eq!(settings["env"]["ANTHROPIC_DEFAULT_HAIKU_MODEL"], "glm-4.7");
        assert_eq!(
            settings["env"]["CLAUDE_CODE_AUTO_COMPACT_WINDOW"],
            "1000000"
        );
        assert_eq!(settings["env"]["API_TIMEOUT_MS"], "3000000");
        assert_eq!(
            settings["env"]["CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC"],
            "1"
        );
        assert_eq!(settings["apiKeyHelper"], "printenv ZAI_API_KEY");
    }

    #[test]
    fn claude_code_zhipuai_writes_china_endpoint_and_zhipu_key() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let mut config = config_with_agent("claude-code", &["ZHIPU_API_KEY"]);
        config.agent.provider = Some(crate::config::AgentProviderConfig {
            id: "zhipuai".to_owned(),
            model: None,
            api_key_ref: Some("ZHIPU_API_KEY".to_owned()),
            custom: None,
        });

        provision_agent_headless_config(&config, tempdir.path()).expect("provision");

        let settings: Value = serde_json::from_str(
            &std::fs::read_to_string(tempdir.path().join(".claude/settings.json"))
                .expect("settings"),
        )
        .expect("settings parse");
        assert_eq!(
            settings["env"]["ANTHROPIC_BASE_URL"],
            "https://open.bigmodel.cn/api/anthropic"
        );
        assert_eq!(settings["env"]["ANTHROPIC_MODEL"], "glm-5.3[1m]");
        assert_eq!(settings["env"]["ANTHROPIC_DEFAULT_HAIKU_MODEL"], "glm-4.7");
        assert_eq!(
            settings["env"]["CLAUDE_CODE_AUTO_COMPACT_WINDOW"],
            "1000000"
        );
        assert_eq!(settings["apiKeyHelper"], "printenv ZHIPU_API_KEY");
    }

    #[test]
    fn claude_code_deepseek_uses_flash_for_haiku_and_subagents() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let mut config = config_with_agent("claude-code", &["DEEPSEEK_API_KEY"]);
        config.agent.provider = Some(crate::config::AgentProviderConfig {
            id: "deepseek".to_owned(),
            model: None,
            api_key_ref: Some("DEEPSEEK_API_KEY".to_owned()),
            custom: None,
        });

        provision_agent_headless_config(&config, tempdir.path()).expect("provision");

        let settings: Value = serde_json::from_str(
            &std::fs::read_to_string(tempdir.path().join(".claude/settings.json"))
                .expect("settings"),
        )
        .expect("settings parse");
        assert_eq!(settings["env"]["ANTHROPIC_MODEL"], "deepseek-v4-pro[1m]");
        assert_eq!(
            settings["env"]["ANTHROPIC_DEFAULT_HAIKU_MODEL"],
            "deepseek-v4-flash"
        );
        assert_eq!(
            settings["env"]["CLAUDE_CODE_SUBAGENT_MODEL"],
            "deepseek-v4-flash"
        );
        assert_eq!(settings["env"]["CLAUDE_CODE_EFFORT_LEVEL"], "max");
        assert_eq!(settings["env"]["CLAUDE_CODE_AUTO_COMPACT_WINDOW"], "786432");

        config.agent.provider.as_mut().expect("provider").model =
            Some("deepseek-v4-pro[1m]".to_owned());
        provision_agent_headless_config(&config, tempdir.path()).expect("reprovision");
        let settings: Value = serde_json::from_str(
            &std::fs::read_to_string(tempdir.path().join(".claude/settings.json"))
                .expect("settings"),
        )
        .expect("settings parse");
        assert_eq!(
            settings["env"]["CLAUDE_CODE_SUBAGENT_MODEL"],
            "deepseek-v4-flash"
        );
    }

    #[test]
    fn claude_code_kimi_for_coding_uses_coding_endpoint_and_kimi_key() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let mut config = config_with_agent("claude-code", &["KIMI_API_KEY"]);
        config.agent.provider = Some(crate::config::AgentProviderConfig {
            id: "kimi-coding".to_owned(),
            model: None,
            api_key_ref: Some("KIMI_API_KEY".to_owned()),
            custom: None,
        });

        provision_agent_headless_config(&config, tempdir.path()).expect("provision");

        let settings: Value = serde_json::from_str(
            &std::fs::read_to_string(tempdir.path().join(".claude/settings.json"))
                .expect("settings"),
        )
        .expect("settings parse");
        assert_eq!(
            settings["env"]["ANTHROPIC_BASE_URL"],
            "https://api.kimi.com/coding/"
        );
        assert_eq!(settings["env"]["ANTHROPIC_MODEL"], "kimi-for-coding");
        assert_eq!(
            settings["env"]["CLAUDE_CODE_SUBAGENT_MODEL"],
            "kimi-for-coding"
        );
        assert_eq!(settings["env"]["CLAUDE_CODE_EFFORT_LEVEL"], "high");
        assert_eq!(settings["env"]["CLAUDE_CODE_AUTO_COMPACT_WINDOW"], "262144");
        assert_eq!(settings["env"]["CLAUDE_CODE_MAX_CONTEXT_TOKENS"], "262144");
        assert_eq!(settings["apiKeyHelper"], "printenv KIMI_API_KEY");
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
