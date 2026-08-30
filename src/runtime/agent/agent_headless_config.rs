//! Agent-specific headless config provisioning: the native config files a harness needs in order
//! to consume the environment variables `acp-stack` delivers through `[agent].env`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::{Map, json};
use serde_norway::Value as YamlValue;
use toml::{Value as TomlValue, map::Map as TomlMap};

use crate::config::{AgentCustomProviderConfig, AgentProviderConfig, Config, CustomProviderApi};
use crate::error::{Result, StackError};
use crate::fs_util::parent_dir;
use crate::runtime::agent::config_io::{
    ensure_object_field, ensure_toml_table_field, insert_if_missing, read_json_object,
    read_toml_table, read_yaml_mapping, write_json_object, write_toml_table, write_yaml_mapping,
};
use crate::runtime::agent::model_wire::{ModelWire, model_wire};
use crate::runtime::agent::provider_keys::{
    CLAUDE_CODE_AGENT_ID, CODEX_OPENAI_PROVIDER_ID, ClaudeCodeProviderProfile,
    agent_provider_id_for_provider_id, claude_code_profile_for_provider_id,
    effective_active_provider_ids, env_var_for_agent_provider_id, hermes_api_mode_for_provider_id,
    provider_name_for_provider_id, vendor_base_url_for_agent_provider_id,
};

mod antigravity;
mod claude_code;
mod codex;
mod goose;
mod hermes;
mod kilo;
mod opencode;
mod pi;

use self::antigravity::*;
use self::claude_code::*;
use self::codex::*;
use self::goose::*;
use self::hermes::*;
use self::kilo::*;
use self::opencode::*;
use self::pi::*;

pub(crate) use self::codex::CODEX_OPENROUTER_PROVIDER_ID;
pub(crate) use self::opencode::{OPENCODE_AGENT_ID, OPENCODE_DISABLED_SMALL_MODEL};
pub(crate) use crate::runtime::agent::provider_keys::{HERMES_AGENT_ID, KILO_AGENT_ID};

pub(crate) const CLAUDE_CODE_MANAGED_ENV_KEYS: &[&str] = &[
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
    "CLAUDE_CODE_MAX_CONTEXT_TOKENS",
    "CLAUDE_CODE_EFFORT_LEVEL",
    "API_TIMEOUT_MS",
    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
];
pub(crate) const CLAUDE_CODE_CREDENTIAL_ENV_KEYS: &[&str] = &[
    "OTEL_EXPORTER_OTLP_HEADERS",
    "OTEL_EXPORTER_OTLP_LOGS_HEADERS",
    "OTEL_EXPORTER_OTLP_METRICS_HEADERS",
    "OTEL_EXPORTER_OTLP_TRACES_HEADERS",
];
pub(crate) const CLAUDE_CODE_EXECUTABLE_COMMAND_ROOTS: &[&str] =
    &["fileSuggestion", "otelHeadersHelper", "statusLine"];

/// Native paths owned by acps config or policy: import strips them, then this provisioner
/// regenerates the supported subset from canonical config.
pub(crate) const CLAUDE_CODE_PERMISSION_ROOTS: &[&str] = &[
    "autoMode",
    "defaultMode",
    "disableAutoMode",
    "disableBypassPermissionsMode",
    "skipDangerousModePermissionPrompt",
    "permission",
    "permissions",
    "sandbox",
    "allowedTools",
    "disallowedTools",
];
pub(crate) const CLAUDE_CODE_AUTH_ROOTS: &[&str] = &[
    "forceLoginGatewayUrl",
    "forceLoginMethod",
    "forceLoginOrgUUID",
];
pub(crate) const CLAUDE_CODE_CREDENTIAL_ROOTS: &[&str] = &[
    "apiKeyHelper",
    "awsAuthRefresh",
    "awsCredentialExport",
    "gcpAuthRefresh",
];
pub(crate) const CLAUDE_CODE_POLICY_ROOTS: &[&str] = &[
    "allowAllClaudeAiMcps",
    "allowManagedHooksOnly",
    "allowManagedMcpServersOnly",
    "allowManagedPermissionRulesOnly",
    "allowedChannelPlugins",
    "allowedHttpHookUrls",
    "allowedMcpServers",
    "blockedMarketplaces",
    "deniedMcpServers",
    "disableClaudeAiConnectors",
    "disabledMcpjsonServers",
    "disableSideloadFlags",
    "disableSkillShellExecution",
    "enableAllProjectMcpServers",
    "enabledMcpjsonServers",
    "enforceAvailableModels",
    "forceRemoteSettingsRefresh",
    "httpHookAllowedEnvVars",
    "policyHelper",
    "strictKnownMarketplaces",
    "strictPluginOnlyCustomization",
];
pub(crate) const CLAUDE_CODE_MANAGED_UNSUPPORTED_ROOTS: &[&str] = &[
    "advisorModel",
    "agent",
    "agents",
    "availableModels",
    "effortLevel",
    "fallbackModel",
    "instructions",
    "modelOverrides",
];
pub(crate) const CODEX_PERMISSION_ROOTS: &[&str] = &[
    "approval_policy",
    "default_permissions",
    "permissions",
    "sandbox_mode",
    "sandbox_workspace_write",
    "shell_environment_policy",
    "tools",
    "web_search",
];
pub(crate) const CODEX_AUTH_ROOTS: &[&str] = &[
    "cli_auth_credentials_store",
    "forced_chatgpt_workspace_id",
    "forced_login_method",
    "mcp_oauth_credentials_store",
    "projects",
];
pub(crate) const CODEX_MANAGED_UNSUPPORTED_ROOTS: &[&str] = &[
    "agents",
    "developer_instructions",
    "instructions",
    "model_instructions_file",
    "profile",
    "profiles",
];
pub(crate) const OPENCODE_PERMISSION_ROOTS: &[&str] =
    &["permission", "permissions", "sandbox", "tools"];
pub(crate) const OPENCODE_POLICY_ROOTS: &[&str] = &["share"];
pub(crate) const OPENCODE_MANAGED_UNSUPPORTED_ROOTS: &[&str] = &[
    "agent",
    "disabled_providers",
    "enabled_providers",
    "instructions",
    "provider",
    "small_model",
];

/// Amp `settings.json` uses flat dotted keys, so these match as literal top-level object keys,
/// not nested paths. They gate which shell commands Amp runs unprompted, so acps owns them.
pub(crate) const AMP_PERMISSION_ROOTS: &[&str] = &[
    "amp.commands.allowlist",
    "amp.commands.strict",
    "amp.dangerouslyAllowAll",
    "amp.guardedFiles.allowlist",
    "amp.mcpPermissions",
    "amp.permissions",
];
/// Tool enable/disable filters; dropping one would silently re-enable a tool the user turned off.
pub(crate) const AMP_POLICY_ROOTS: &[&str] = &["amp.tools.disable"];

/// Pi trust roots: `defaultProjectTrust` decides whether Pi auto-approves tool calls.
pub(crate) const PI_PERMISSION_ROOTS: &[&str] = &["defaultProjectTrust"];
/// Pi keys that invoke a shell or executable, so importing them is command execution.
pub(crate) const PI_EXECUTABLE_COMMAND_ROOTS: &[&str] =
    &["shellPath", "shellCommandPrefix", "npmCommand"];
/// Pi resource-source roots; importing them would pull in unvetted third-party code.
pub(crate) const PI_EXECUTABLE_PLUGIN_ROOTS: &[&str] =
    &["packages", "extensions", "skills", "prompts", "themes"];

/// Goose approval roots: `GOOSE_MODE` gates unprompted tool calls and `GOOSE_ALLOWLIST` names
/// a remote URL of loadable extensions.
pub(crate) const GOOSE_PERMISSION_ROOTS: &[&str] = &["GOOSE_MODE", "GOOSE_ALLOWLIST"];
/// Goose planner-model roots. Planning Mode selects a second model+provider lane that acps
/// cannot express or credential, so blocking them keeps Goose off an unprovisioned provider.
pub(crate) const GOOSE_MANAGED_UNSUPPORTED_ROOTS: &[&str] = &[
    "GOOSE_PLANNER_CONTEXT_LIMIT",
    "GOOSE_PLANNER_MODEL",
    "GOOSE_PLANNER_PROVIDER",
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
    provision_agent_headless_config_with_previous_pi_model(config, home, None)
}

pub fn provision_agent_headless_config_transition(
    previous: &Config,
    config: &Config,
    home: &Path,
) -> Result<Vec<ProvisionedAgentConfig>> {
    let previous_pi_model = (previous.agent.id == "pi")
        .then(|| configured_provider_model(previous))
        .flatten();
    provision_agent_headless_config_with_previous_pi_model(config, home, previous_pi_model)
}

/// The provider endpoint override in force for `home`, resolved from the secret store here so
/// every re-provisioning path observes the same one without carrying it.
fn resolved_endpoint_override(
    home: &Path,
) -> Result<Option<crate::secrets::ProviderEndpointOverride>> {
    crate::secrets::managed_provider_endpoint_override_for_home(home)
}

/// The override origin that applies to `provider_id`, or `None` when a different
/// provider is the rerouted one.
pub(super) fn endpoint_origin_for<'a>(
    endpoint: Option<&'a crate::secrets::ProviderEndpointOverride>,
    provider_id: &str,
) -> Option<&'a str> {
    endpoint
        .filter(|endpoint| endpoint.provider_id == provider_id)
        .map(|endpoint| endpoint.base_url.as_str())
}

/// `vendor_base_url` with its scheme, host, and port replaced by `origin`'s. The vendor path
/// stays verbatim (a trailing slash is significant to some agents); a bare vendor root yields
/// the bare origin.
pub(crate) fn reroute_base_url(origin: &str, vendor_base_url: &str) -> Result<String> {
    let origin = endpoint_origin(origin)?;
    let vendor_url =
        reqwest::Url::parse(vendor_base_url).map_err(|_| StackError::InvalidParam {
            field: "base_url",
            reason: format!("vendor base URL `{vendor_base_url}` is not a valid URL"),
        })?;
    let path = match vendor_url.path() {
        "/" => "",
        path => path,
    };
    Ok(format!("{origin}{path}"))
}

/// The stored override as `scheme://host[:port]`, refusing any value that carries a path.
pub(crate) fn endpoint_origin(origin: &str) -> Result<String> {
    let origin_url = reqwest::Url::parse(origin).map_err(|_| StackError::InvalidParam {
        field: "base_url",
        reason: format!("endpoint override `{origin}` is not a valid URL"),
    })?;
    let Some(host) = origin_url.host_str() else {
        return Err(StackError::InvalidParam {
            field: "base_url",
            reason: format!("endpoint override `{origin}` has no host"),
        });
    };
    // A stored value from before the origin-only contract would otherwise be silently
    // truncated to its origin and provision a different URL than it did before.
    if origin_url.path() != "/" {
        return Err(StackError::InvalidParam {
            field: "base_url",
            reason: format!(
                "endpoint override `{origin}` carries a path; the override must be an origin"
            ),
        });
    }
    let port = origin_url
        .port()
        .map(|port| format!(":{port}"))
        .unwrap_or_default();
    Ok(format!("{}://{host}{port}", origin_url.scheme()))
}

/// The rerouted base for `provider_id` when the override names it: `vendor_base_url` behind the
/// override origin.
pub(super) fn rerouted_base_url_for(
    endpoint: Option<&crate::secrets::ProviderEndpointOverride>,
    provider_id: &str,
    vendor_base_url: &str,
) -> Result<Option<String>> {
    endpoint_origin_for(endpoint, provider_id)
        .map(|origin| reroute_base_url(origin, vendor_base_url))
        .transpose()
}

/// The vendor base a mapped provider uses under `agent_id`, required once an override names it.
fn require_vendor_base_url(agent_id: &str, provider_id: &str, path: &Path) -> Result<&'static str> {
    vendor_base_url_for_agent_provider_id(agent_id, provider_id).ok_or_else(|| {
        StackError::AgentConfigProvision {
            path: path.to_path_buf(),
            reason: format!(
                "{agent_id} provider `{provider_id}` declares no vendor base URL in the provider \
                 mapping, so its endpoint override cannot be composed"
            ),
        }
    })
}

/// The rerouted base for a mapped provider under `agent_id`, or `None` without an override.
fn rerouted_mapped_base_url_for(
    endpoint: Option<&crate::secrets::ProviderEndpointOverride>,
    agent_id: &str,
    provider_id: &str,
    path: &Path,
) -> Result<Option<String>> {
    let Some(endpoint) = endpoint.filter(|endpoint| endpoint.provider_id == provider_id) else {
        return Ok(None);
    };
    let vendor_base_url = require_vendor_base_url(agent_id, provider_id, path)?;
    let vendor_base_url = crate::runtime::agent::provider_keys::resolve_base_url_template(
        vendor_base_url,
        &endpoint.companion_values,
    )?;
    reroute_base_url(&endpoint.base_url, &vendor_base_url).map(Some)
}

fn provision_agent_headless_config_with_previous_pi_model(
    config: &Config,
    home: &Path,
    previous_pi_model: Option<&str>,
) -> Result<Vec<ProvisionedAgentConfig>> {
    let endpoint = resolved_endpoint_override(home)?;
    let endpoint = endpoint.as_ref();
    match config.agent.id.as_str() {
        "goose" => provision_goose_config(config, home, endpoint).map(|paths| {
            paths
                .into_iter()
                .map(|path| ProvisionedAgentConfig {
                    label: "Goose config",
                    path,
                })
                .collect()
        }),
        KILO_AGENT_ID => provision_kilo_config(home, endpoint).map(|paths| {
            paths
                .into_iter()
                .map(|path| ProvisionedAgentConfig {
                    label: "Kilo config",
                    path,
                })
                .collect()
        }),
        OPENCODE_AGENT_ID => provision_opencode_config(config, home, endpoint).map(|path| {
            path.into_iter()
                .map(|path| ProvisionedAgentConfig {
                    label: "OpenCode config",
                    path,
                })
                .collect()
        }),
        "codex" => provision_codex_config(config, home, endpoint).map(|paths| {
            paths
                .into_iter()
                .map(|path| ProvisionedAgentConfig {
                    label: "Codex config",
                    path,
                })
                .collect()
        }),
        CLAUDE_CODE_AGENT_ID => provision_claude_code_config(config, home, endpoint).map(|paths| {
            paths
                .into_iter()
                .map(|path| ProvisionedAgentConfig {
                    label: "Claude Code config",
                    path,
                })
                .collect()
        }),
        "pi" => provision_pi_config(config, home, previous_pi_model, endpoint).map(|path| {
            path.into_iter()
                .map(|path| ProvisionedAgentConfig {
                    label: "Pi settings",
                    path,
                })
                .collect()
        }),
        HERMES_AGENT_ID => provision_hermes_config(config, home, endpoint).map(|paths| {
            paths
                .into_iter()
                .map(|path| ProvisionedAgentConfig {
                    label: "Hermes config",
                    path,
                })
                .collect()
        }),
        ANTIGRAVITY_AGENT_ID => provision_antigravity_config(config, home).map(|paths| {
            paths
                .into_iter()
                .map(|path| ProvisionedAgentConfig {
                    label: "Antigravity settings",
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
    let endpoint = resolved_endpoint_override(home)?;
    let endpoint = endpoint.as_ref();
    match config.agent.id.as_str() {
        "goose" => cleanup_goose_config(config, home),
        KILO_AGENT_ID => cleanup_kilo_config(home),
        OPENCODE_AGENT_ID => cleanup_opencode_config(config, home),
        "codex" => cleanup_codex_config(config, home),
        CLAUDE_CODE_AGENT_ID => cleanup_claude_code_config(config, home, endpoint),
        "pi" => cleanup_pi_config(config, home, endpoint),
        HERMES_AGENT_ID => cleanup_hermes_config(config, home),
        ANTIGRAVITY_AGENT_ID => cleanup_antigravity_config(config, home),
        _ => Ok(Vec::new()),
    }
}

fn write_or_remove_json_object(path: &Path, root: Map<String, serde_json::Value>) -> Result<()> {
    if root.is_empty() {
        remove_file(path)?;
    } else {
        write_json_object(path, root)?;
    }
    Ok(())
}

fn write_or_remove_yaml_mapping(path: &Path, root: serde_norway::Mapping) -> Result<()> {
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

fn configured_active_provider_configs(config: &Config) -> Vec<AgentProviderConfig> {
    effective_active_provider_ids(&config.agent)
        .into_iter()
        .map(|provider_id| {
            config
                .agent
                .provider
                .as_ref()
                .filter(|provider| provider.id == provider_id)
                .or_else(|| {
                    configured_subagent_provider(config)
                        .filter(|provider| provider.id == provider_id)
                })
                .cloned()
                .unwrap_or(AgentProviderConfig {
                    id: provider_id,
                    model: None,
                    api_key_ref: None,
                    custom: None,
                })
        })
        .collect()
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
        if crate::config::agent_env_declares(&config.agent.env, api_key_ref) {
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

    if provider.custom.is_none()
        && let Some(api_key_ref) = env_var_for_agent_provider_id(&config.agent.id, provider_id)
    {
        return Ok(api_key_ref);
    }

    Err(StackError::AgentConfigProvision {
        path: path.to_path_buf(),
        reason: format!(
            "{} provider `{provider_id}` requires api_key_ref to generate agent config",
            config.agent.id
        ),
    })
}

/// Shared test fixture, `pub(super)` so each sibling's `mod tests` reaches it via `use super::*`.
#[cfg(test)]
pub(super) fn config_with_agent(id: &str, env: &[&str]) -> Config {
    use crate::config::load_config_from_str;

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

#[cfg(test)]
pub(super) fn custom_provider_config(
    agent_id: &str,
    api: crate::config::CustomProviderApi,
) -> Config {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reroute_keeps_the_vendor_path_and_swaps_the_origin() {
        assert_eq!(
            reroute_base_url("http://127.0.0.1:3129", "https://api.moonshot.ai/anthropic")
                .expect("rerouted"),
            "http://127.0.0.1:3129/anthropic"
        );
        // A trailing slash is significant to Claude Code's kimi lane.
        assert_eq!(
            reroute_base_url("http://127.0.0.1:3129", "https://api.kimi.com/coding/")
                .expect("rerouted"),
            "http://127.0.0.1:3129/coding/"
        );
        // A bare vendor root yields the bare origin.
        assert_eq!(
            reroute_base_url("https://relay.example", "https://api.anthropic.com")
                .expect("rerouted"),
            "https://relay.example"
        );
        assert_eq!(
            reroute_base_url("http://[::1]:3129", "https://openrouter.ai/api/v1")
                .expect("rerouted"),
            "http://[::1]:3129/api/v1"
        );
        assert_eq!(
            reroute_base_url("http://localhost:3129/", "https://api.openai.com/v1")
                .expect("rerouted"),
            "http://localhost:3129/v1"
        );
    }

    #[test]
    fn reroute_rejects_an_origin_with_a_path() {
        let error = reroute_base_url(
            "http://127.0.0.1:3129/anthropic",
            "https://api.anthropic.com",
        )
        .expect_err("path must be refused");
        assert!(error.to_string().contains("carries a path"), "{error}");
    }

    #[test]
    fn unsupported_agent_has_no_generated_config() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let config = config_with_agent("amp", &["AMP_API_KEY"]);

        let provisioned =
            provision_agent_headless_config(&config, tempdir.path()).expect("provision");

        assert!(provisioned.is_empty());
    }
}
