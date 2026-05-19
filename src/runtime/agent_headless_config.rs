//! Agent-specific headless config provisioning.
//!
//! `acp-stack` owns secret delivery through `[agent].env`, but some harnesses
//! need a config file that tells them how to consume those environment
//! variables. Keep that mapping explicit here so "supported" means a configured
//! agent can start headlessly after init.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value as JsonValue, json};
use serde_yaml::{Mapping as YamlMapping, Value as YamlValue};
use toml::{Value as TomlValue, map::Map as TomlMap};

use crate::config::Config;
use crate::error::{Result, StackError};
use crate::fs_util::{atomic_write_owner_only, create_dir_owner_only, parent_dir};
use crate::runtime::provider_keys::{env_var_for_agent_provider_id, provider_name_for_provider_id};

const CODEX_OPENROUTER_PROVIDER_ID: &str = "openrouter";
// Codex uses OpenRouter's Responses-compatible endpoint instead of the chat
// completions endpoint most OpenRouter clients configure by default.
const CODEX_OPENROUTER_RESPONSES_BASE_URL: &str = "https://openrouter.ai/api/v1/responses";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvisionedAgentConfig {
    pub label: &'static str,
    pub path: PathBuf,
}

pub fn provision_agent_headless_config(
    config: &Config,
    home: &Path,
) -> Result<Vec<ProvisionedAgentConfig>> {
    match config.agent.id.as_str() {
        "goose" => provision_goose_config(config, home).map(|path| {
            path.into_iter()
                .map(|path| ProvisionedAgentConfig {
                    label: "Goose config",
                    path,
                })
                .collect()
        }),
        "opencode" => provision_opencode_config(config, home).map(|path| {
            path.into_iter()
                .map(|path| ProvisionedAgentConfig {
                    label: "OpenCode config",
                    path,
                })
                .collect()
        }),
        "codex" => provision_codex_config(config, home).map(|path| {
            path.into_iter()
                .map(|path| ProvisionedAgentConfig {
                    label: "Codex config",
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

fn provision_goose_config(config: &Config, home: &Path) -> Result<Option<PathBuf>> {
    let path = home.join(".config").join("goose").join("config.yaml");
    let Some(provider) = config.agent.provider.as_ref() else {
        return Ok(None);
    };
    let provider_id = provider.id.as_str();
    let api_key_ref = require_agent_env_for_provider(config, provider_id, &path)?;
    let Some(native_ref) = env_var_for_agent_provider_id(&config.agent.id, provider_id) else {
        return Err(StackError::AgentConfigProvision {
            path: path.clone(),
            reason: format!(
                "goose provider `{provider_id}` has no API-key env mapping in data/mapping.toml"
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

    write_yaml_mapping(&path, root)?;
    Ok(Some(path))
}

fn provision_opencode_config(config: &Config, home: &Path) -> Result<Option<PathBuf>> {
    let path = home.join(".config").join("opencode").join("opencode.json");
    let Some(provider) = config.agent.provider.as_ref() else {
        return Ok(None);
    };
    let provider_id = provider.id.as_str();
    let api_key_ref = require_agent_env_for_provider(config, provider_id, &path)?;
    let mut root = read_json_object(&path)?;
    insert_if_missing(
        &mut root,
        "$schema",
        json!("https://opencode.ai/config.json"),
        &path,
    )?;
    if let Some(model) = configured_provider_model(config) {
        root.insert("model".to_owned(), json!(model));
    }

    let provider = ensure_object_field(&mut root, "provider", &path)?;
    let provider_config = ensure_object_field(provider, provider_id, &path)?;
    insert_if_missing(provider_config, "models", json!({}), &path)?;
    let options = ensure_object_field(provider_config, "options", &path)?;
    let api_key_value = json!(format!("{{env:{api_key_ref}}}"));
    options.insert("apiKey".to_owned(), api_key_value);

    write_json_object(&path, root)?;
    Ok(Some(path))
}

fn provision_pi_config(config: &Config, home: &Path) -> Result<Option<PathBuf>> {
    let path = home.join(".pi").join("agent").join("settings.json");
    let Some(_) = config.agent.provider.as_ref() else {
        return Ok(None);
    };
    let Some(model) = configured_provider_model(config) else {
        return Ok(None);
    };
    let mut root = read_json_object(&path)?;

    root.insert("enabledModels".to_owned(), json!([model]));

    write_json_object(&path, root)?;
    Ok(Some(path))
}

fn provision_codex_config(config: &Config, home: &Path) -> Result<Option<PathBuf>> {
    let path = home.join(".codex").join("config.toml");
    let Some(provider) = config.agent.provider.as_ref() else {
        return Ok(None);
    };
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
    let Some(model) = configured_provider_model(config) else {
        return Ok(None);
    };
    let api_key_ref = require_agent_env_for_provider(config, CODEX_OPENROUTER_PROVIDER_ID, &path)?;
    let Some(native_ref) =
        env_var_for_agent_provider_id(&config.agent.id, CODEX_OPENROUTER_PROVIDER_ID)
    else {
        return Err(StackError::AgentConfigProvision {
            path: path.clone(),
            reason: "codex OpenRouter has no API-key env mapping in data/mapping.toml".to_owned(),
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
    root.insert("model".to_owned(), TomlValue::String(model.to_owned()));
    root.insert(
        "model_provider".to_owned(),
        TomlValue::String(CODEX_OPENROUTER_PROVIDER_ID.to_owned()),
    );
    let Some(provider_name) = provider_name_for_provider_id(CODEX_OPENROUTER_PROVIDER_ID) else {
        return Err(StackError::AgentConfigProvision {
            path: path.clone(),
            reason: "codex OpenRouter has no provider metadata in data/mapping.toml".to_owned(),
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

fn provision_codex_openai_config(config: &Config, path: &Path) -> Result<Option<PathBuf>> {
    let Some(model) = configured_provider_model(config) else {
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

fn configured_provider_model(config: &Config) -> Option<&str> {
    config
        .agent
        .provider
        .as_ref()
        .and_then(|provider| provider.model.as_deref())
        .filter(|model| !model.trim().is_empty())
}

fn require_agent_env_for_provider<'a>(
    config: &'a Config,
    provider_id: &str,
    path: &Path,
) -> Result<&'a str> {
    if let Some(api_key_ref) = config
        .agent
        .provider
        .as_ref()
        .and_then(|provider| provider.api_key_ref.as_deref())
    {
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
            "{} provider `{provider_id}` requires [agent.provider].api_key_ref to generate agent config",
            config.agent.id
        ),
    })
}

fn read_json_object(path: &Path) -> Result<Map<String, JsonValue>> {
    if !path.exists() {
        return Ok(Map::new());
    }

    let content = std::fs::read_to_string(path).map_err(|source| StackError::ConfigRead {
        path: path.to_path_buf(),
        source,
    })?;
    let value: JsonValue =
        serde_json::from_str(&content).map_err(|source| StackError::AgentConfigProvision {
            path: path.to_path_buf(),
            reason: format!("existing JSON is invalid: {source}"),
        })?;
    match value {
        JsonValue::Object(object) => Ok(object),
        _ => Err(StackError::AgentConfigProvision {
            path: path.to_path_buf(),
            reason: "existing JSON root must be an object".to_owned(),
        }),
    }
}

fn write_json_object(path: &Path, object: Map<String, JsonValue>) -> Result<()> {
    let parent = parent_dir(path)?;
    create_dir_owner_only(parent)?;
    let content = serde_json::to_vec_pretty(&JsonValue::Object(object)).map_err(|source| {
        StackError::AgentConfigProvision {
            path: path.to_path_buf(),
            reason: format!("failed to serialize JSON: {source}"),
        }
    })?;
    let mut with_newline = content;
    with_newline.push(b'\n');
    atomic_write_owner_only(path, &with_newline)
}

fn ensure_object_field<'a>(
    object: &'a mut Map<String, JsonValue>,
    key: &str,
    path: &Path,
) -> Result<&'a mut Map<String, JsonValue>> {
    if !object.contains_key(key) {
        object.insert(key.to_owned(), json!({}));
    }
    object
        .get_mut(key)
        .and_then(JsonValue::as_object_mut)
        .ok_or_else(|| StackError::AgentConfigProvision {
            path: path.to_path_buf(),
            reason: format!("`{key}` must be an object when present"),
        })
}

fn insert_if_missing(
    object: &mut Map<String, JsonValue>,
    key: &str,
    value: JsonValue,
    path: &Path,
) -> Result<()> {
    if let Some(existing) = object.get(key) {
        if existing.is_null() {
            return Err(StackError::AgentConfigProvision {
                path: path.to_path_buf(),
                reason: format!("`{key}` must not be null when present"),
            });
        }
        return Ok(());
    }
    object.insert(key.to_owned(), value);
    Ok(())
}

fn read_yaml_mapping(path: &Path) -> Result<YamlMapping> {
    if !path.exists() {
        return Ok(YamlMapping::new());
    }

    let content = std::fs::read_to_string(path).map_err(|source| StackError::ConfigRead {
        path: path.to_path_buf(),
        source,
    })?;
    if content.trim().is_empty() {
        return Ok(YamlMapping::new());
    }
    let value: YamlValue =
        serde_yaml::from_str(&content).map_err(|source| StackError::AgentConfigProvision {
            path: path.to_path_buf(),
            reason: format!("existing YAML is invalid: {source}"),
        })?;
    match value {
        YamlValue::Mapping(mapping) => Ok(mapping),
        _ => Err(StackError::AgentConfigProvision {
            path: path.to_path_buf(),
            reason: "existing YAML root must be a mapping".to_owned(),
        }),
    }
}

fn write_yaml_mapping(path: &Path, mapping: YamlMapping) -> Result<()> {
    let parent = parent_dir(path)?;
    create_dir_owner_only(parent)?;
    let content = serde_yaml::to_string(&YamlValue::Mapping(mapping)).map_err(|source| {
        StackError::AgentConfigProvision {
            path: path.to_path_buf(),
            reason: format!("failed to serialize YAML: {source}"),
        }
    })?;
    let mut bytes = content.into_bytes();
    if !bytes.ends_with(b"\n") {
        bytes.push(b'\n');
    }
    atomic_write_owner_only(path, &bytes)
}

fn read_toml_table(path: &Path) -> Result<TomlMap<String, TomlValue>> {
    if !path.exists() {
        return Ok(TomlMap::new());
    }

    let content = std::fs::read_to_string(path).map_err(|source| StackError::ConfigRead {
        path: path.to_path_buf(),
        source,
    })?;
    if content.trim().is_empty() {
        return Ok(TomlMap::new());
    }
    let value: TomlValue =
        toml::from_str(&content).map_err(|source| StackError::AgentConfigProvision {
            path: path.to_path_buf(),
            reason: format!("existing TOML is invalid: {source}"),
        })?;
    match value {
        TomlValue::Table(table) => Ok(table),
        _ => Err(StackError::AgentConfigProvision {
            path: path.to_path_buf(),
            reason: "existing TOML root must be a table".to_owned(),
        }),
    }
}

fn write_toml_table(path: &Path, table: TomlMap<String, TomlValue>) -> Result<()> {
    let parent = parent_dir(path)?;
    create_dir_owner_only(parent)?;
    let content = toml::to_string_pretty(&TomlValue::Table(table)).map_err(|source| {
        StackError::AgentConfigProvision {
            path: path.to_path_buf(),
            reason: format!("failed to serialize TOML: {source}"),
        }
    })?;
    let mut bytes = content.into_bytes();
    if !bytes.ends_with(b"\n") {
        bytes.push(b'\n');
    }
    atomic_write_owner_only(path, &bytes)
}

fn ensure_toml_table_field<'a>(
    table: &'a mut TomlMap<String, TomlValue>,
    key: &str,
    path: &Path,
) -> Result<&'a mut TomlMap<String, TomlValue>> {
    if !table.contains_key(key) {
        table.insert(key.to_owned(), TomlValue::Table(TomlMap::new()));
    }
    table
        .get_mut(key)
        .and_then(TomlValue::as_table_mut)
        .ok_or_else(|| StackError::AgentConfigProvision {
            path: path.to_path_buf(),
            reason: format!("`{key}` must be a table when present"),
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

[auth]
session_key_ref = "ACP_STACK_SESSION_KEY"
admin_key_ref = "ACP_STACK_ADMIN_KEY"

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

[workspace.source]
type = "none"

[logging]
level = "info"
local_retention_days = 30

[logging.supabase]
enabled = false
url = "https://example.supabase.co"
service_role_key_ref = "SUPABASE_SERVICE_ROLE_KEY"
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

    #[test]
    fn goose_config_is_skipped_without_configured_provider() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let config = config_with_agent("goose", &["OPENROUTER_API_KEY"]);

        let provisioned =
            provision_agent_headless_config(&config, tempdir.path()).expect("provision");

        assert!(provisioned.is_empty());
    }

    #[test]
    fn goose_config_references_provider_native_env() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let mut config = config_with_agent("goose", &["OPENROUTER_API_KEY"]);
        config.agent.provider = Some(crate::config::AgentProviderConfig {
            id: "openrouter".to_owned(),
            model: Some("deepseek/deepseek-v4-flash".to_owned()),
            api_key_ref: Some("OPENROUTER_API_KEY".to_owned()),
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
        assert_eq!(value["GOOSE_MODEL"], serde_yaml::Value::Null);
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
        });

        provision_agent_headless_config(&config, tempdir.path()).expect("provision");

        let value: serde_yaml::Value = serde_yaml::from_str(
            &std::fs::read_to_string(&path).expect("goose config should be readable"),
        )
        .expect("goose config yaml parses");
        assert_eq!(value["GOOSE_PROVIDER"], "cerebras");
        assert_eq!(value["GOOSE_MODEL"], "old/model");
        assert_eq!(value["CUSTOM_SETTING"], "keep");
    }

    #[test]
    fn goose_rejects_non_native_api_key_ref() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let mut config = config_with_agent("goose", &["CUSTOM_OPENROUTER_KEY"]);
        config.agent.provider = Some(crate::config::AgentProviderConfig {
            id: "openrouter".to_owned(),
            model: Some("deepseek/deepseek-v4-flash".to_owned()),
            api_key_ref: Some("CUSTOM_OPENROUTER_KEY".to_owned()),
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
        });

        provision_agent_headless_config(&config, tempdir.path()).expect("provision");

        let value: Value = serde_json::from_str(
            &std::fs::read_to_string(&path).expect("opencode config should be readable"),
        )
        .expect("opencode config json parses");
        assert_eq!(value["model"], "openai/gpt-5.5");
        assert_eq!(
            value["provider"]["openai"]["options"]["apiKey"],
            "{env:OPENAI_API_KEY}"
        );
        assert_eq!(value["provider"]["openai"]["options"]["timeout"], 600000);
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
    fn unsupported_agent_has_no_generated_config() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let config = config_with_agent("cursor", &["CURSOR_API_KEY"]);

        let provisioned =
            provision_agent_headless_config(&config, tempdir.path()).expect("provision");

        assert!(provisioned.is_empty());
    }
}
