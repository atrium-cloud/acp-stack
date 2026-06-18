//! Agent-specific headless config provisioning.
//!
//! `acp-stack` owns secret delivery through `[agent].env`, but some harnesses
//! need a config file that tells them how to consume those environment
//! variables. Keep that mapping explicit here so "supported" means a configured
//! agent can start headlessly after init.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::{Map, json};
use serde_yaml::Value as YamlValue;
use toml::{Value as TomlValue, map::Map as TomlMap};

use crate::config::{AgentCustomProviderConfig, AgentProviderConfig, Config, CustomProviderApi};
use crate::error::{Result, StackError};
use crate::fs_util::parent_dir;
use crate::runtime::agent::claude_code_provider_profiles::{
    CLAUDE_CODE_AGENT_ID, ClaudeCodeProviderProfile, profile_for_provider_id,
};
use crate::runtime::agent::config_io::{
    ensure_object_field, ensure_toml_table_field, insert_if_missing, read_json_object,
    read_toml_table, read_yaml_mapping, write_json_object, write_toml_table, write_yaml_mapping,
};
use crate::runtime::agent::provider_keys::{
    agent_provider_id_for_provider_id, env_var_for_agent_provider_id, provider_name_for_provider_id,
};

const CODEX_OPENROUTER_PROVIDER_ID: &str = "openrouter";
// Codex uses OpenRouter's Responses-compatible endpoint instead of the chat
// completions endpoint most OpenRouter clients configure by default.
const CODEX_OPENROUTER_RESPONSES_BASE_URL: &str = "https://openrouter.ai/api/v1/responses";
pub(crate) const OPENCODE_AGENT_ID: &str = "opencode";
// OpenCode treats an empty `small_model` as unset and still falls back to its
// implicit small model. This invalid id is the verified no-call sentinel.
pub(crate) const OPENCODE_DISABLED_SMALL_MODEL: &str = "invalid/model";
const CLAUDE_CODE_API_KEY_HELPER_PREFIX: &str = "printenv ";
const CLAUDE_CODE_MANAGED_ENV_KEYS: &[&str] = &[
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_MODEL",
    "ANTHROPIC_DEFAULT_FABLE_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    "CLAUDE_CODE_SUBAGENT_MODEL",
    "ENABLE_TOOL_SEARCH",
    "CLAUDE_CODE_AUTO_COMPACT_WINDOW",
    "API_TIMEOUT_MS",
    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvisionedAgentConfig {
    pub label: &'static str,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanedAgentConfig {
    pub label: &'static str,
    pub path: PathBuf,
}

pub fn provision_agent_headless_config(
    config: &Config,
    home: &Path,
) -> Result<Vec<ProvisionedAgentConfig>> {
    match config.agent.id.as_str() {
        "goose" => provision_goose_config(config, home).map(|paths| {
            paths
                .into_iter()
                .map(|path| ProvisionedAgentConfig {
                    label: "Goose config",
                    path,
                })
                .collect()
        }),
        OPENCODE_AGENT_ID => provision_opencode_config(config, home).map(|path| {
            path.into_iter()
                .map(|path| ProvisionedAgentConfig {
                    label: "OpenCode config",
                    path,
                })
                .collect()
        }),
        "codex" => provision_codex_config(config, home).map(|paths| {
            paths
                .into_iter()
                .map(|path| ProvisionedAgentConfig {
                    label: "Codex config",
                    path,
                })
                .collect()
        }),
        CLAUDE_CODE_AGENT_ID => provision_claude_code_config(config, home).map(|paths| {
            paths
                .into_iter()
                .map(|path| ProvisionedAgentConfig {
                    label: "Claude Code config",
                    path,
                })
                .collect()
        }),
        "pi" => provision_pi_config(config, home).map(|path| {
            path.into_iter()
                .map(|path| ProvisionedAgentConfig {
                    label: "Pi settings",
                    path,
                })
                .collect()
        }),
        _ => Ok(Vec::new()),
    }
}

pub fn cleanup_agent_headless_config(
    config: &Config,
    home: &Path,
) -> Result<Vec<CleanedAgentConfig>> {
    match config.agent.id.as_str() {
        "goose" => cleanup_goose_config(config, home),
        OPENCODE_AGENT_ID => cleanup_opencode_config(config, home),
        "codex" => cleanup_codex_config(config, home),
        CLAUDE_CODE_AGENT_ID => cleanup_claude_code_config(config, home),
        "pi" => cleanup_pi_config(config, home),
        _ => Ok(Vec::new()),
    }
}

fn provision_goose_config(config: &Config, home: &Path) -> Result<Vec<PathBuf>> {
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
    // Mirror the canonical config: if no provider model is configured,
    // drop any stale `GOOSE_MODEL` from a prior run so the launched
    // Goose process doesn't keep using it under the new provider.
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

fn cleanup_goose_config(config: &Config, home: &Path) -> Result<Vec<CleanedAgentConfig>> {
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

fn provision_opencode_config(config: &Config, home: &Path) -> Result<Option<PathBuf>> {
    let path = home.join(".config").join("opencode").join("opencode.json");
    let Some(provider) = config.agent.provider.as_ref() else {
        return Ok(None);
    };
    let subagent_disabled = configured_subagent_disabled(config);
    let subagent_provider = configured_subagent_provider(config);
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
    let provider_key = write_opencode_provider_config(config, providers, provider, &path)?;
    enabled_providers.insert(provider_key);
    if !subagent_disabled
        && let Some(subagent_provider) = subagent_provider
        && !same_provider_config(provider, subagent_provider)
    {
        let provider_key =
            write_opencode_provider_config(config, providers, subagent_provider, &path)?;
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

fn cleanup_opencode_config(config: &Config, home: &Path) -> Result<Vec<CleanedAgentConfig>> {
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
    if let Some(provider) = config.agent.provider.as_ref() {
        provider_keys.insert(opencode_provider_config_key(config, provider).to_owned());
    }
    if let Some(provider) = configured_subagent_provider(config) {
        provider_keys.insert(opencode_provider_config_key(config, provider).to_owned());
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

fn provision_pi_config(config: &Config, home: &Path) -> Result<Option<PathBuf>> {
    let path = home.join(".pi").join("agent").join("settings.json");
    let Some(provider) = config.agent.provider.as_ref() else {
        return Ok(None);
    };
    if let Some(custom) = provider.custom.as_ref() {
        let models_path = home.join(".pi").join("agent").join("models.json");
        let api_key_ref = require_agent_env_for_provider(config, &provider.id, &models_path)?;
        write_pi_custom_models_json(&models_path, provider, custom, api_key_ref)?;
    }
    let Some(model) = configured_provider_model(config) else {
        // No provider model in canonical config — clear any stale
        // `enabledModels` so the launched Pi process doesn't keep
        // using a prior selection under the new provider lane. When
        // there's no existing file, there's nothing to do.
        if !path.exists() {
            return Ok(None);
        }
        let mut root = read_json_object(&path)?;
        if root.remove("enabledModels").is_some() {
            write_json_object(&path, root)?;
            return Ok(Some(path));
        }
        return Ok(None);
    };
    let mut root = read_json_object(&path)?;

    root.insert("enabledModels".to_owned(), json!([model]));

    write_json_object(&path, root)?;
    Ok(Some(path))
}

fn cleanup_pi_config(config: &Config, home: &Path) -> Result<Vec<CleanedAgentConfig>> {
    let mut cleaned = Vec::new();
    let settings_path = home.join(".pi").join("agent").join("settings.json");
    if settings_path.exists() {
        let mut root = read_json_object(&settings_path)?;
        if root.remove("enabledModels").is_some() {
            write_or_remove_json_object(&settings_path, root)?;
            cleaned.push(CleanedAgentConfig {
                label: "Pi settings",
                path: settings_path,
            });
        }
    }
    if let Some(provider) = config.agent.provider.as_ref()
        && provider.custom.is_some()
    {
        let models_path = home.join(".pi").join("agent").join("models.json");
        if models_path.exists() {
            let mut root = read_json_object(&models_path)?;
            let mut changed = false;
            let mut remove_providers_object = false;
            if let Some(providers) = root
                .get_mut("providers")
                .and_then(serde_json::Value::as_object_mut)
            {
                changed |= providers.remove(&provider.id).is_some();
                remove_providers_object = providers.is_empty();
            }
            if remove_providers_object {
                root.remove("providers");
            }
            if changed {
                write_or_remove_json_object(&models_path, root)?;
                cleaned.push(CleanedAgentConfig {
                    label: "Pi custom models",
                    path: models_path,
                });
            }
        }
    }
    Ok(cleaned)
}

fn write_pi_custom_models_json(
    path: &Path,
    provider: &crate::config::AgentProviderConfig,
    custom: &AgentCustomProviderConfig,
    api_key_ref: &str,
) -> Result<()> {
    let mut root = read_json_object(path)?;
    let providers = ensure_object_field(&mut root, "providers", path)?;
    providers.insert(
        provider.id.clone(),
        json!({
            "baseUrl": custom.base_url.clone(),
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
    write_json_object(path, root)
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

fn provision_codex_config(config: &Config, home: &Path) -> Result<Vec<PathBuf>> {
    let mut written = Vec::new();
    if let Some(path) = provision_codex_main_config(config, home)? {
        written.push(path);
    }
    Ok(written)
}

fn provision_claude_code_config(config: &Config, home: &Path) -> Result<Vec<PathBuf>> {
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

fn cleanup_claude_code_config(config: &Config, home: &Path) -> Result<Vec<CleanedAgentConfig>> {
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

fn cleanup_codex_config(config: &Config, home: &Path) -> Result<Vec<CleanedAgentConfig>> {
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

fn provision_codex_main_config(config: &Config, home: &Path) -> Result<Option<PathBuf>> {
    let path = home.join(".codex").join("config.toml");
    let Some(provider) = config.agent.provider.as_ref() else {
        return Ok(None);
    };
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
        )?;
        write_toml_table(&path, root)?;
        return Ok(Some(path));
    }
    if provider.id == "openai" {
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

    let mut root = read_toml_table(&path)?;
    // Always settle the OpenRouter provider table even when no model
    // is selected yet — the L87 provider-only init path relies on
    // ~/.codex/config.toml advertising the new provider so the
    // provisional discovery spawn picks it up; a half-written
    // `model_provider = "openrouter"` with no matching provider
    // table would otherwise leave the launched harness unable to
    // resolve auth.
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
        TomlValue::String(CODEX_OPENROUTER_RESPONSES_BASE_URL.to_owned()),
    );
    openrouter.insert(
        "env_key".to_owned(),
        TomlValue::String(native_ref.to_owned()),
    );
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
        TomlValue::String(custom.base_url.clone()),
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
        // Provider switched to openai without a model selection. If a
        // prior run wrote a model into ~/.codex/config.toml, clear it
        // so the launched harness does not silently keep using the
        // stale model under the new provider lane. When there's no
        // existing file we simply have nothing to do.
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

fn write_or_remove_json_object(path: &Path, root: Map<String, serde_json::Value>) -> Result<()> {
    if root.is_empty() {
        remove_file(path)?;
    } else {
        write_json_object(path, root)?;
    }
    Ok(())
}

fn write_or_remove_yaml_mapping(path: &Path, root: serde_yaml::Mapping) -> Result<()> {
    if root.is_empty() {
        remove_file(path)?;
    } else {
        write_yaml_mapping(path, root)?;
    }
    Ok(())
}

fn write_or_remove_toml_table(path: &Path, root: TomlMap<String, TomlValue>) -> Result<()> {
    if root.is_empty() {
        remove_file(path)?;
    } else {
        write_toml_table(path, root)?;
    }
    Ok(())
}

fn remove_file_if_exists(path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    remove_file(path)?;
    Ok(true)
}

fn remove_file(path: &Path) -> Result<()> {
    std::fs::remove_file(path).map_err(|source| StackError::FileRemove {
        path: path.to_path_buf(),
        source,
    })
}

fn configured_provider_model(config: &Config) -> Option<&str> {
    config
        .agent
        .provider
        .as_ref()
        .and_then(|provider| provider.model.as_deref())
        .filter(|model| !model.trim().is_empty())
}

fn configured_subagent_provider(config: &Config) -> Option<&AgentProviderConfig> {
    config
        .agent
        .subagent
        .as_ref()
        .filter(|subagent| !subagent.disabled)
        .and_then(|subagent| subagent.provider.as_ref())
}

fn configured_subagent_disabled(config: &Config) -> bool {
    config
        .agent
        .subagent
        .as_ref()
        .is_some_and(|subagent| subagent.disabled)
}

fn configured_subagent_provider_model(config: &Config) -> Option<&str> {
    configured_subagent_provider(config)
        .and_then(|provider| provider.model.as_deref())
        .filter(|model| !model.trim().is_empty())
}

fn same_provider_config(left: &AgentProviderConfig, right: &AgentProviderConfig) -> bool {
    left.id == right.id && left.api_key_ref == right.api_key_ref && left.custom == right.custom
}

fn require_agent_env_for_provider<'a>(
    config: &'a Config,
    provider_id: &str,
    path: &Path,
) -> Result<&'a str> {
    let Some(provider) = config.agent.provider.as_ref() else {
        return Err(StackError::AgentConfigProvision {
            path: path.to_path_buf(),
            reason: format!(
                "{} provider `{provider_id}` requires [agent.provider].api_key_ref to generate agent config",
                config.agent.id
            ),
        });
    };
    require_agent_env_for_provider_config(config, provider, provider_id, path)
}

fn require_agent_env_for_provider_config<'a>(
    config: &'a Config,
    provider: &'a AgentProviderConfig,
    provider_id: &str,
    path: &Path,
) -> Result<&'a str> {
    if let Some(api_key_ref) = provider.api_key_ref.as_deref() {
        if config.agent.env.iter().any(|name| name == api_key_ref) {
            return Ok(api_key_ref);
        }
        return Err(StackError::AgentConfigProvision {
            path: path.to_path_buf(),
            reason: format!(
                "{} provider `{provider_id}` references `{api_key_ref}`, but it is missing from [agent].env",
                config.agent.id
            ),
        });
    }

    Err(StackError::AgentConfigProvision {
        path: path.to_path_buf(),
        reason: format!(
            "{} provider `{provider_id}` requires api_key_ref to generate agent config",
            config.agent.id
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::load_config_from_str;
    use serde_json::Value;

    fn config_with_agent(id: &str, env: &[&str]) -> Config {
        let env_toml = env
            .iter()
            .map(|name| format!("{name:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        load_config_from_str(&format!(
            r#"
[api]
bind = "127.0.0.1:7700"
public_url = "http://127.0.0.1:7700"
max_request_bytes = 104857600

[security.http]
max_request_bytes = 104857600
rate_limit_per_minute = 120
burst = 30
auth_failures_per_minute = 5
auth_block_duration = "15m"
allowed_origins = []
trust_proxy_headers = false

[workspace]
root = "/workspace"
uploads = "/workspace/uploads"
default_shell = "/bin/bash"
runtime_user = "acp"
max_file_bytes = 8388608

[logging]
level = "info"
local_retention_days = 30

[logging.supabase]
enabled = false
url = "https://example.supabase.co"
api_key_ref = "SUPABASE_SECRET_KEY"
schema = "acp_stack"

[agent]
id = "{id}"
name = "Test Agent"
command = "{id}"
args = []
cwd = "/workspace"
env = [{env_toml}]
restart = "on-crash"
"#
        ))
        .expect("config parses")
    }

    fn custom_provider_config(agent_id: &str, api: crate::config::CustomProviderApi) -> Config {
        let mut config = config_with_agent(agent_id, &["CUSTOM_API_KEY"]);
        config.agent.provider = Some(crate::config::AgentProviderConfig {
            id: "myprovider".to_owned(),
            model: Some("my-model".to_owned()),
            api_key_ref: Some("CUSTOM_API_KEY".to_owned()),
            custom: Some(crate::config::AgentCustomProviderConfig {
                name: "My Provider".to_owned(),
                base_url: "https://api.myprovider.example/v1".to_owned(),
                api,
                model_name: Some("My Model".to_owned()),
                context: 200_000,
                output_max_tokens: 65_536,
            }),
        });
        config
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
        let goose: serde_yaml::Value = serde_yaml::from_str(
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
        let value: serde_yaml::Value = serde_yaml::from_str(
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

        let value: serde_yaml::Value = serde_yaml::from_str(
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
        // New provider, NO model selected — mirrors the L87 init path
        // where the operator picks a provider but skips model setup.
        config.agent.provider = Some(crate::config::AgentProviderConfig {
            id: "cerebras".to_owned(),
            model: None,
            api_key_ref: Some("CEREBRAS_API_KEY".to_owned()),
            custom: None,
        });

        provision_agent_headless_config(&config, tempdir.path()).expect("provision");

        let value: serde_yaml::Value =
            serde_yaml::from_str(&std::fs::read_to_string(&path).expect("goose readable"))
                .expect("goose yaml parses");
        assert_eq!(value["GOOSE_PROVIDER"], "cerebras");
        assert!(
            value.as_mapping().is_some_and(|map| {
                !map.contains_key(serde_yaml::Value::String("GOOSE_MODEL".to_owned()))
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
            Some("https://openrouter.ai/api/v1/responses")
        );
        assert_eq!(
            value["model_providers"]["openrouter"]["name"].as_str(),
            Some("OpenRouter")
        );
        assert_eq!(
            value["model_providers"]["openrouter"]["env_key"].as_str(),
            Some("OPENROUTER_API_KEY")
        );
        assert_eq!(
            value["model_providers"]["openrouter"]["wire_api"].as_str(),
            Some("responses")
        );
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
        let value: serde_yaml::Value =
            serde_yaml::from_str(&std::fs::read_to_string(&goose_path).expect("goose readable"))
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
                    !map.contains_key(serde_yaml::Value::String(key.to_owned()))
                }),
                "{key} should be removed"
            );
        }
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
        assert_eq!(
            value["enabledModels"],
            json!(["opencode-go/deepseek-v4-flash"])
        );
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

    #[test]
    fn unsupported_agent_has_no_generated_config() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let config = config_with_agent("cursor", &["CURSOR_API_KEY"]);

        let provisioned =
            provision_agent_headless_config(&config, tempdir.path()).expect("provision");

        assert!(provisioned.is_empty());
    }
}
