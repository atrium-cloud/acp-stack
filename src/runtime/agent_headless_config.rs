//! Agent-specific headless config provisioning.
//!
//! `acp-stack` owns secret delivery through `[agent].env`, but some harnesses
//! need a config file that tells them how to consume those environment
//! variables. Keep that mapping explicit here so "supported" means a configured
//! agent can start headlessly after init.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::config::Config;
use crate::error::{Result, StackError};
use crate::fs_util::{atomic_write_owner_only, create_dir_owner_only, parent_dir};

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
        "opencode" => provision_opencode_config(config, home).map(|path| {
            path.into_iter()
                .map(|path| ProvisionedAgentConfig {
                    label: "OpenCode config",
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

fn read_json_object(path: &Path) -> Result<Map<String, Value>> {
    if !path.exists() {
        return Ok(Map::new());
    }

    let content = std::fs::read_to_string(path).map_err(|source| StackError::ConfigRead {
        path: path.to_path_buf(),
        source,
    })?;
    let value: Value =
        serde_json::from_str(&content).map_err(|source| StackError::AgentConfigProvision {
            path: path.to_path_buf(),
            reason: format!("existing JSON is invalid: {source}"),
        })?;
    match value {
        Value::Object(object) => Ok(object),
        _ => Err(StackError::AgentConfigProvision {
            path: path.to_path_buf(),
            reason: "existing JSON root must be an object".to_owned(),
        }),
    }
}

fn write_json_object(path: &Path, object: Map<String, Value>) -> Result<()> {
    let parent = parent_dir(path)?;
    create_dir_owner_only(parent)?;
    let content = serde_json::to_vec_pretty(&Value::Object(object)).map_err(|source| {
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
    object: &'a mut Map<String, Value>,
    key: &str,
    path: &Path,
) -> Result<&'a mut Map<String, Value>> {
    if !object.contains_key(key) {
        object.insert(key.to_owned(), json!({}));
    }
    object
        .get_mut(key)
        .and_then(Value::as_object_mut)
        .ok_or_else(|| StackError::AgentConfigProvision {
            path: path.to_path_buf(),
            reason: format!("`{key}` must be an object when present"),
        })
}

fn insert_if_missing(
    object: &mut Map<String, Value>,
    key: &str,
    value: Value,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::load_config_from_str;

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
