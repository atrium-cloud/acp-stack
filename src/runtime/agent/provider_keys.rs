//! Reusable provider/API-key compatibility mapping, parsed and queried from embedded TOML data.

mod resolve;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::LazyLock;

use serde::Deserialize;

use crate::config::{AgentConfig, AgentProviderConfig, Config};
use crate::error::{Result, StackError};
use crate::secrets::SecretStore;

pub use self::resolve::{
    ResolvedAgentEnvironment, ResolvedProviderSnapshot, apply_catalog_mapped_agent_provider,
    apply_mapped_agent_provider, catalog_covers_env_ref, configured_custom_provider_api_key_ref,
    effective_active_provider_ids, env_ref_is_satisfiable, env_ref_is_satisfiable_for_config,
    push_delivers_env_ref_for_config, reconcile_kimi_lane_env_declarations,
    resolve_agent_environment, resolve_agent_environment_without_secrets, target_uses_provider,
};

const EMBEDDED_ENV_VARS: &str = include_str!("../../../data/env_vars.toml");
const EMBEDDED_PROVIDERS: &str = include_str!("../../../data/providers.toml");
pub const CLAUDE_CODE_AGENT_ID: &str = "claude-code";
pub const CODEX_AGENT_ID: &str = "codex";
pub const HERMES_AGENT_ID: &str = "hermes";
pub const KILO_AGENT_ID: &str = "kilo";
/// Codex reserves `openai` for its own built-in provider definition, whose replacement table
/// shape is version-dependent, so this pair cannot carry an endpoint override.
pub const CODEX_OPENAI_PROVIDER_ID: &str = "openai";

/// Wire transports Hermes accepts on a named `providers:` entry.
const HERMES_API_MODES: [&str; 3] = ["chat_completions", "anthropic_messages", "codex_responses"];

static PROVIDER_KEY_MAPPING: LazyLock<ProviderKeyMapping> = LazyLock::new(|| {
    ProviderKeyMapping::from_toml_parts(EMBEDDED_ENV_VARS, EMBEDDED_PROVIDERS)
        .expect("valid provider mapping")
});

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ApiKeyProviderMapping {
    pub env_var: String,
    #[serde(default)]
    pub provider_ids: Vec<String>,
    #[serde(default)]
    pub agent_ids: Vec<String>,
    #[serde(default)]
    pub companion_env_vars: Vec<String>,
    #[serde(default)]
    pub optional_env_vars: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ProviderEnvMapping {
    pub id: Vec<String>,
    pub name: String,
    pub agents: Vec<String>,
    #[serde(default)]
    pub api_key_env_vars: BTreeMap<String, String>,
    #[serde(default)]
    pub provider_ids: BTreeMap<String, String>,
    #[serde(default)]
    pub companion_env_vars: Vec<String>,
    #[serde(default)]
    pub optional_env_vars: Vec<String>,
    /// OpenAI-compatible `GET /models` endpoint for live model-catalog fetches.
    #[serde(default)]
    pub models_url: Option<String>,
    #[serde(default)]
    pub claude_code: Option<ClaudeCodeProviderProfile>,
    #[serde(default)]
    pub hermes: Option<HermesProviderProfile>,
}

/// Claude Code-specific headless provisioning metadata for one provider. The env-var lists are
/// `Option` so an omitted list (fall back to provider-level) differs from an explicitly empty one.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ClaudeCodeProviderProfile {
    #[serde(default)]
    pub agent_native_auth: bool,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub default_model: Option<String>,
    #[serde(default)]
    pub default_opus_model: Option<String>,
    #[serde(default)]
    pub default_sonnet_model: Option<String>,
    #[serde(default)]
    pub default_haiku_model: Option<String>,
    #[serde(default)]
    pub set_subagent_model: bool,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub companion_env_vars: Option<Vec<String>>,
    #[serde(default)]
    pub optional_env_vars: Option<Vec<String>>,
}

/// Hermes-specific headless provisioning metadata for one provider; a `None` `api_mode` marks
/// the pair as unable to carry an endpoint override.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct HermesProviderProfile {
    #[serde(default)]
    pub api_mode: Option<String>,
}

impl ProviderEnvMapping {
    pub fn ids(&self) -> &[String] {
        &self.id
    }

    fn primary_id(&self) -> &str {
        self.id
            .first()
            .expect("provider mapping validated with at least one id")
    }

    fn contains_id(&self, provider_id: &str) -> bool {
        self.id.iter().any(|id| id == provider_id)
    }

    fn agent_native_provider_id(&self, agent_id: &str) -> Option<&str> {
        if !self.agents.iter().any(|agent| agent == agent_id) {
            return None;
        }
        self.provider_ids
            .iter()
            .find_map(|(provider_id, mapped_agent_id)| {
                (mapped_agent_id == agent_id).then_some(provider_id.as_str())
            })
            .or_else(|| Some(self.primary_id()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderKeyMapping {
    api_keys: Vec<ApiKeyProviderMapping>,
    providers: Vec<ProviderEnvMapping>,
}

#[derive(Debug, Deserialize)]
struct RawProviderKeyMapping {
    #[serde(default)]
    api_keys: Vec<ApiKeyProviderMapping>,
    #[serde(default)]
    providers: Vec<ProviderEnvMapping>,
}

#[derive(Debug, Deserialize)]
struct RawEnvVarMapping {
    #[serde(default)]
    api_keys: Vec<ApiKeyProviderMapping>,
}

#[derive(Debug, Deserialize)]
struct RawProviderMapping {
    #[serde(default)]
    providers: Vec<ProviderEnvMapping>,
}

impl ProviderKeyMapping {
    pub fn load_embedded() -> &'static Self {
        &PROVIDER_KEY_MAPPING
    }

    pub fn from_toml(body: &str) -> Result<Self> {
        let raw: RawProviderKeyMapping =
            toml::from_str(body).map_err(|source| StackError::RegistryLoad {
                reason: format!("provider mapping TOML is invalid: {source}"),
            })?;
        let mapping = Self {
            api_keys: raw.api_keys,
            providers: raw.providers,
        };
        mapping.validate()?;
        Ok(mapping)
    }

    pub fn from_toml_parts(env_vars_body: &str, providers_body: &str) -> Result<Self> {
        let env_vars: RawEnvVarMapping =
            toml::from_str(env_vars_body).map_err(|source| StackError::RegistryLoad {
                reason: format!("env var mapping TOML is invalid: {source}"),
            })?;
        let providers: RawProviderMapping =
            toml::from_str(providers_body).map_err(|source| StackError::RegistryLoad {
                reason: format!("provider mapping TOML is invalid: {source}"),
            })?;
        let mapping = Self {
            api_keys: env_vars.api_keys,
            providers: providers.providers,
        };
        mapping.validate()?;
        Ok(mapping)
    }

    pub fn api_keys(&self) -> &[ApiKeyProviderMapping] {
        &self.api_keys
    }

    pub fn providers(&self) -> &[ProviderEnvMapping] {
        &self.providers
    }

    fn mapping_for_env_var(&self, env_var: &str) -> Option<&ApiKeyProviderMapping> {
        self.api_keys
            .iter()
            .find(|mapping| mapping.env_var == env_var)
    }

    fn mapping_for_provider_id(&self, provider_id: &str) -> Option<&ApiKeyProviderMapping> {
        self.api_keys
            .iter()
            .find(|mapping| mapping.provider_ids.iter().any(|id| id == provider_id))
            .or_else(|| {
                let provider = self.provider_mapping(provider_id)?;
                self.api_keys.iter().find(|mapping| {
                    mapping
                        .provider_ids
                        .iter()
                        .any(|api_key_provider_id| provider.contains_id(api_key_provider_id))
                })
            })
    }

    fn provider_mapping(&self, provider_id: &str) -> Option<&ProviderEnvMapping> {
        self.providers
            .iter()
            .find(|mapping| mapping.contains_id(provider_id))
    }

    fn validate(&self) -> Result<()> {
        let mut env_vars = HashSet::new();
        let mut provider_ids = HashSet::new();
        for mapping in &self.api_keys {
            validate_token("api_keys.env_var", &mapping.env_var)?;
            if !env_vars.insert(mapping.env_var.as_str()) {
                return provider_mapping_error(format!(
                    "duplicate API-key env var `{}`",
                    mapping.env_var
                ));
            }
            if mapping.provider_ids.is_empty() && mapping.agent_ids.is_empty() {
                return provider_mapping_error(format!(
                    "API-key env var `{}` has no provider ids or agent ids",
                    mapping.env_var
                ));
            }
            validate_tokens(
                format!("api_keys.{}.provider_ids", mapping.env_var),
                &mapping.provider_ids,
            )?;
            validate_tokens(
                format!("api_keys.{}.agent_ids", mapping.env_var),
                &mapping.agent_ids,
            )?;
            validate_tokens(
                format!("api_keys.{}.companion_env_vars", mapping.env_var),
                &mapping.companion_env_vars,
            )?;
            validate_tokens(
                format!("api_keys.{}.optional_env_vars", mapping.env_var),
                &mapping.optional_env_vars,
            )?;
            for provider_id in &mapping.provider_ids {
                if !provider_ids.insert(provider_id.as_str()) {
                    return provider_mapping_error(format!(
                        "duplicate provider id `{provider_id}` in API-key mapping"
                    ));
                }
            }
        }

        let mut provider_overrides = HashSet::new();
        for mapping in &self.providers {
            if mapping.id.is_empty() {
                return provider_mapping_error("providers.id must not be empty".to_owned());
            }
            validate_tokens("providers.id".to_owned(), &mapping.id)?;
            let primary_id = mapping.primary_id();
            validate_token(&format!("providers.{primary_id}.name"), &mapping.name)?;
            if mapping.agents.is_empty() {
                return provider_mapping_error(format!(
                    "provider `{primary_id}` has no supported agents"
                ));
            }
            validate_tokens(format!("providers.{primary_id}.agents"), &mapping.agents)?;
            for agent in &mapping.agents {
                if !is_supported_agent_id(agent) {
                    return provider_mapping_error(format!(
                        "provider `{primary_id}` references unsupported agent `{agent}`"
                    ));
                }
            }
            for (agent, env_var) in &mapping.api_key_env_vars {
                validate_token(&format!("providers.{primary_id}.api_key_env_vars"), agent)?;
                validate_token(
                    &format!("providers.{primary_id}.api_key_env_vars.{agent}"),
                    env_var,
                )?;
                if !is_supported_agent_id(agent) {
                    return provider_mapping_error(format!(
                        "provider `{primary_id}` references unsupported API-key agent `{agent}`"
                    ));
                }
                if !mapping.agents.iter().any(|supported| supported == agent) {
                    return provider_mapping_error(format!(
                        "provider `{primary_id}` has API-key env var for unsupported agent `{agent}`"
                    ));
                }
            }
            let mut mapped_agents = HashSet::new();
            for (provider_id, agent_id) in &mapping.provider_ids {
                validate_token(&format!("providers.{primary_id}.provider_ids"), provider_id)?;
                validate_token(
                    &format!("providers.{primary_id}.provider_ids.{provider_id}"),
                    agent_id,
                )?;
                if !mapping.contains_id(provider_id) {
                    return provider_mapping_error(format!(
                        "provider `{primary_id}` maps unknown native provider id `{provider_id}`"
                    ));
                }
                if !mapping.agents.iter().any(|agent| agent == agent_id) {
                    return provider_mapping_error(format!(
                        "provider `{primary_id}` maps native provider id `{provider_id}` to unsupported agent `{agent_id}`"
                    ));
                }
                if !mapped_agents.insert(agent_id.as_str()) {
                    return provider_mapping_error(format!(
                        "provider `{primary_id}` has multiple native provider ids for agent `{agent_id}`"
                    ));
                }
            }
            if !mapping.provider_ids.is_empty() {
                for agent_id in &mapping.agents {
                    if !mapping
                        .provider_ids
                        .values()
                        .any(|mapped| mapped == agent_id)
                    {
                        return provider_mapping_error(format!(
                            "provider `{primary_id}` has no native provider id for agent `{agent_id}`"
                        ));
                    }
                }
            }
            for provider_id in &mapping.id {
                if !provider_overrides.insert(provider_id.as_str()) {
                    return provider_mapping_error(format!(
                        "duplicate provider env mapping `{provider_id}`"
                    ));
                }
            }
            validate_tokens(
                format!("providers.{primary_id}.companion_env_vars"),
                &mapping.companion_env_vars,
            )?;
            validate_tokens(
                format!("providers.{primary_id}.optional_env_vars"),
                &mapping.optional_env_vars,
            )?;
            if let Some(models_url) = mapping.models_url.as_deref() {
                validate_token(&format!("providers.{primary_id}.models_url"), models_url)?;
                // The fetch sends the operator's API key as a bearer token, so plaintext
                // endpoints are rejected outright.
                if !models_url.starts_with("https://") {
                    return provider_mapping_error(format!(
                        "provider `{primary_id}` models_url must be an HTTPS URL"
                    ));
                }
            }
            if let Some(profile) = &mapping.claude_code {
                self.validate_claude_code_profile(mapping, profile)?;
            }
            if let Some(profile) = &mapping.hermes {
                self.validate_hermes_profile(mapping, profile)?;
            } else if mapping.agents.iter().any(|agent| agent == HERMES_AGENT_ID) {
                return provider_mapping_error(format!(
                    "provider `{primary_id}` supports `{HERMES_AGENT_ID}` but declares no [providers.hermes] profile"
                ));
            }
        }

        for mapping in &self.api_keys {
            for provider_id in &mapping.provider_ids {
                if self.provider_mapping(provider_id).is_none() {
                    return provider_mapping_error(format!(
                        "provider id `{provider_id}` has no provider metadata entry"
                    ));
                }
            }
        }

        Ok(())
    }

    fn validate_claude_code_profile(
        &self,
        mapping: &ProviderEnvMapping,
        profile: &ClaudeCodeProviderProfile,
    ) -> Result<()> {
        let primary_id = mapping.primary_id();
        if !mapping
            .agents
            .iter()
            .any(|agent| agent == CLAUDE_CODE_AGENT_ID)
        {
            return provider_mapping_error(format!(
                "provider `{primary_id}` declares a Claude Code profile but does not support `{CLAUDE_CODE_AGENT_ID}`"
            ));
        }
        if let Some(base_url) = profile.base_url.as_deref() {
            validate_token(
                &format!("providers.{primary_id}.claude_code.base_url"),
                base_url,
            )?;
            if !(base_url.starts_with("https://") || base_url.starts_with("http://")) {
                return provider_mapping_error(format!(
                    "provider `{primary_id}` claude_code.base_url must be an HTTP(S) URL"
                ));
            }
        }
        if let Some(default_model) = profile.default_model.as_deref() {
            validate_token(
                &format!("providers.{primary_id}.claude_code.default_model"),
                default_model,
            )?;
        }
        let role_models = [
            ("default_opus_model", profile.default_opus_model.as_deref()),
            (
                "default_sonnet_model",
                profile.default_sonnet_model.as_deref(),
            ),
            (
                "default_haiku_model",
                profile.default_haiku_model.as_deref(),
            ),
        ];
        if profile.default_model.is_none() && role_models.iter().any(|(_, model)| model.is_some()) {
            return provider_mapping_error(format!(
                "provider `{primary_id}` declares Claude Code role model defaults without default_model"
            ));
        }
        for (field, model) in role_models {
            if let Some(model) = model {
                validate_token(
                    &format!("providers.{primary_id}.claude_code.{field}"),
                    model,
                )?;
            }
        }
        for (key, value) in &profile.env {
            validate_token(
                &format!("providers.{primary_id}.claude_code.env.key `{key}`"),
                key,
            )?;
            validate_token(
                &format!("providers.{primary_id}.claude_code.env.{key}"),
                value,
            )?;
        }
        if let Some(companions) = &profile.companion_env_vars {
            validate_tokens(
                format!("providers.{primary_id}.claude_code.companion_env_vars"),
                companions,
            )?;
        }
        if let Some(optional) = &profile.optional_env_vars {
            validate_tokens(
                format!("providers.{primary_id}.claude_code.optional_env_vars"),
                optional,
            )?;
        }
        if profile.agent_native_auth {
            if mapping.api_key_env_vars.contains_key(CLAUDE_CODE_AGENT_ID) {
                return provider_mapping_error(format!(
                    "provider `{primary_id}` uses Claude Code native auth but declares a claude-code API-key env var"
                ));
            }
            for provider_id in &mapping.id {
                if self.mapping_for_provider_id(provider_id).is_some() {
                    return provider_mapping_error(format!(
                        "provider `{primary_id}` uses Claude Code native auth but has an API-key mapping"
                    ));
                }
            }
        }
        Ok(())
    }

    fn validate_hermes_profile(
        &self,
        mapping: &ProviderEnvMapping,
        profile: &HermesProviderProfile,
    ) -> Result<()> {
        let primary_id = mapping.primary_id();
        if !mapping.agents.iter().any(|agent| agent == HERMES_AGENT_ID) {
            return provider_mapping_error(format!(
                "provider `{primary_id}` declares a Hermes profile but does not support `{HERMES_AGENT_ID}`"
            ));
        }
        if let Some(api_mode) = profile.api_mode.as_deref() {
            validate_token(&format!("providers.{primary_id}.hermes.api_mode"), api_mode)?;
            if !HERMES_API_MODES.contains(&api_mode) {
                return provider_mapping_error(format!(
                    "provider `{primary_id}` hermes.api_mode must be one of {}, got `{api_mode}`",
                    HERMES_API_MODES.join(", ")
                ));
            }
        }
        Ok(())
    }
}

pub fn mapping_for_env_var(env_var: &str) -> Option<&'static ApiKeyProviderMapping> {
    ProviderKeyMapping::load_embedded().mapping_for_env_var(env_var)
}

pub fn mapping_for_provider_id(provider_id: &str) -> Option<&'static ApiKeyProviderMapping> {
    ProviderKeyMapping::load_embedded().mapping_for_provider_id(provider_id)
}

pub fn env_var_for_provider_id(provider_id: &str) -> Option<&'static str> {
    mapping_for_provider_id(provider_id).map(|mapping| mapping.env_var.as_str())
}

pub fn env_var_for_agent_provider_id(agent_id: &str, provider_id: &str) -> Option<&'static str> {
    let mapping = ProviderKeyMapping::load_embedded();
    mapping.provider_mapping(provider_id).and_then(|provider| {
        if !provider.agents.iter().any(|id| id == agent_id) {
            return None;
        }
        provider
            .api_key_env_vars
            .get(agent_id)
            .map(String::as_str)
            .or_else(|| {
                mapping
                    .mapping_for_provider_id(provider_id)
                    .map(|key| key.env_var.as_str())
            })
    })
}

pub fn api_key_ref_can_migrate_for_provider(
    provider_id: &str,
    from_ref: &str,
    to_ref: &str,
) -> bool {
    let mapping = ProviderKeyMapping::load_embedded();
    let Some(provider) = mapping.provider_mapping(provider_id) else {
        return false;
    };

    let mut refs = BTreeSet::new();
    if let Some(key_mapping) = mapping.mapping_for_provider_id(provider_id) {
        refs.insert(key_mapping.env_var.as_str());
    }
    refs.extend(provider.api_key_env_vars.values().map(String::as_str));
    refs.contains(from_ref) && refs.contains(to_ref)
}

pub fn env_refs_for_agent_id(agent_id: &str) -> Vec<&'static str> {
    ProviderKeyMapping::load_embedded()
        .api_keys
        .iter()
        .filter(|mapping| mapping.agent_ids.iter().any(|id| id == agent_id))
        .map(|mapping| mapping.env_var.as_str())
        .collect()
}

pub fn provider_id_is_known(provider_id: &str) -> bool {
    ProviderKeyMapping::load_embedded()
        .provider_mapping(provider_id)
        .is_some()
}

pub fn provider_id_supports_agent(provider_id: &str, agent_id: &str) -> bool {
    ProviderKeyMapping::load_embedded()
        .provider_mapping(provider_id)
        .is_some_and(|provider| provider.agents.iter().any(|agent| agent == agent_id))
}

pub fn agent_provider_id_for_provider_id(
    agent_id: &str,
    provider_id: &str,
) -> Option<&'static str> {
    ProviderKeyMapping::load_embedded()
        .provider_mapping(provider_id)
        .and_then(|provider| provider.agent_native_provider_id(agent_id))
}

pub fn canonical_provider_id_for_agent_native_id(
    agent_id: &str,
    native_provider_id: &str,
) -> Option<&'static str> {
    if provider_id_supports_agent(native_provider_id, agent_id) {
        return providers_for_agent(agent_id)
            .into_iter()
            .find(|provider| provider.id == native_provider_id)
            .map(|provider| provider.id);
    }
    providers_for_agent(agent_id)
        .into_iter()
        .find(|provider| provider.agent_provider_id.unwrap_or(provider.id) == native_provider_id)
        .map(|provider| provider.id)
}

pub fn claude_code_profile_for_provider_id(
    provider_id: &str,
) -> Option<&'static ClaudeCodeProviderProfile> {
    ProviderKeyMapping::load_embedded()
        .provider_mapping(provider_id)
        .and_then(|provider| provider.claude_code.as_ref())
}

pub fn is_claude_code_profiled_provider(provider_id: &str) -> bool {
    claude_code_profile_for_provider_id(provider_id).is_some()
}

/// The wire transport the managed Hermes named-provider entry declares for this provider.
pub fn hermes_api_mode_for_provider_id(provider_id: &str) -> Option<&'static str> {
    ProviderKeyMapping::load_embedded()
        .provider_mapping(provider_id)
        .and_then(|provider| provider.hermes.as_ref())
        .and_then(|profile| profile.api_mode.as_deref())
}

pub fn provider_uses_agent_native_auth(agent_id: &str, provider_id: &str) -> bool {
    agent_id == CLAUDE_CODE_AGENT_ID
        && claude_code_profile_for_provider_id(provider_id)
            .is_some_and(|profile| profile.agent_native_auth)
}

/// Whether an (agent, provider) pair can carry an operator-supplied endpoint. Codex plus the
/// built-in `openai` id and Hermes pairs with no declared api_mode cannot; unknown ids can.
pub fn agent_provider_accepts_endpoint_override(agent_id: &str, provider_id: &str) -> bool {
    if agent_id == CODEX_AGENT_ID && provider_id == CODEX_OPENAI_PROVIDER_ID {
        return false;
    }
    if agent_id != HERMES_AGENT_ID {
        return true;
    }
    let mapping = ProviderKeyMapping::load_embedded();
    match mapping.provider_mapping(provider_id) {
        Some(provider) if provider.agents.iter().any(|agent| agent == HERMES_AGENT_ID) => provider
            .hermes
            .as_ref()
            .is_some_and(|profile| profile.api_mode.is_some()),
        _ => true,
    }
}

pub fn models_url_for_provider_id(provider_id: &str) -> Option<&'static str> {
    ProviderKeyMapping::load_embedded()
        .provider_mapping(provider_id)
        .and_then(|provider| provider.models_url.as_deref())
}

pub fn provider_name_for_provider_id(provider_id: &str) -> Option<&'static str> {
    ProviderKeyMapping::load_embedded()
        .provider_mapping(provider_id)
        .map(|provider| provider.name.as_str())
}

/// Compact summary of one provider available to a given agent, as served by `/v1/providers`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentProviderSummary {
    /// Operator-facing provider id (the value passed as `--provider`).
    pub id: &'static str,
    /// Human-readable name pulled from the provider mapping.
    pub name: &'static str,
    /// Agent-native provider id, set only when it differs from the operator-facing id.
    pub agent_provider_id: Option<&'static str>,
    /// Default API-key env var ref for this pair; `None` for custom or agent-native-auth setups.
    pub default_api_key_ref: Option<&'static str>,
    /// Required companion env vars beyond the API key.
    pub companion_env_refs: Vec<&'static str>,
    /// Optional env vars the operator may set for this provider.
    pub optional_env_refs: Vec<&'static str>,
}

/// Every operator-facing provider id supported for `agent_id`, in embedded-mapping order.
pub fn providers_for_agent(agent_id: &str) -> Vec<AgentProviderSummary> {
    let mapping = ProviderKeyMapping::load_embedded();
    let mut seen: BTreeSet<&'static str> = BTreeSet::new();
    let mut summaries = Vec::new();
    for provider in &mapping.providers {
        if !provider.agents.iter().any(|agent| agent == agent_id) {
            continue;
        }
        for id in &provider.id {
            // Alias ids each become their own entry, mirroring what `--provider <id>` accepts.
            if !seen.insert(static_str(id)) {
                continue;
            }
            let id_static = static_str(id);
            let mut default = env_var_for_agent_provider_id(agent_id, id_static);
            // A native-auth pair takes no API key; advertising a default would let a client
            // write a config the CLI then rejects.
            if provider_uses_agent_native_auth(agent_id, id_static) {
                default = None;
            }
            let native = provider.agent_native_provider_id(agent_id).map(static_str);
            let agent_provider_id = match native {
                Some(value) if value == id_static => None,
                other => other,
            };
            summaries.push(AgentProviderSummary {
                id: id_static,
                name: static_str(&provider.name),
                agent_provider_id,
                default_api_key_ref: default,
                companion_env_refs: companion_env_refs_for_agent_provider_id(agent_id, id_static),
                optional_env_refs: optional_env_refs_for_agent_provider_id(agent_id, id_static),
            });
        }
    }
    summaries
}

/// Re-borrow an embedded `String` as a `'static` `&str`. Callers MUST pass a string borrowed
/// from `PROVIDER_KEY_MAPPING`; any other origin makes the lifetime extension unsound.
fn static_str(value: &str) -> &'static str {
    // SAFETY: `PROVIDER_KEY_MAPPING` is a `LazyLock` that is never dropped, so its allocations
    // outlive the program.
    unsafe { std::mem::transmute::<&str, &'static str>(value) }
}

pub fn required_env_refs_for_provider_id(provider_id: &str, api_key_ref: &str) -> Vec<String> {
    let mut refs = vec![api_key_ref.to_owned()];
    refs.extend(
        companion_env_refs_for_provider_id(provider_id)
            .into_iter()
            .map(str::to_owned),
    );
    refs
}

pub fn required_env_refs_for_agent_provider_id(
    agent_id: &str,
    provider_id: &str,
    api_key_ref: Option<&str>,
) -> Vec<String> {
    if agent_id == CLAUDE_CODE_AGENT_ID
        && let Some(profile) = claude_code_profile_for_provider_id(provider_id)
    {
        let mut refs = Vec::new();
        if let Some(api_key_ref) = api_key_ref {
            refs.push(api_key_ref.to_owned());
        }
        match &profile.companion_env_vars {
            Some(companions) => refs.extend(companions.iter().cloned()),
            None => refs.extend(
                companion_env_refs_for_provider_id(provider_id)
                    .into_iter()
                    .map(str::to_owned),
            ),
        }
        return refs;
    }
    api_key_ref
        .map(|api_key_ref| required_env_refs_for_provider_id(provider_id, api_key_ref))
        .unwrap_or_default()
}

pub fn companion_env_refs_for_provider_id(provider_id: &str) -> Vec<&'static str> {
    let mapping = ProviderKeyMapping::load_embedded();
    let mut refs: Vec<_> = mapping
        .mapping_for_provider_id(provider_id)
        .into_iter()
        .flat_map(|mapping| mapping.companion_env_vars.iter().map(String::as_str))
        .collect();
    if let Some(provider) = mapping.provider_mapping(provider_id) {
        refs.extend(provider.companion_env_vars.iter().map(String::as_str));
    }
    dedupe_refs(refs)
}

pub fn companion_env_refs_for_agent_provider_id(
    agent_id: &str,
    provider_id: &str,
) -> Vec<&'static str> {
    if agent_id == CLAUDE_CODE_AGENT_ID
        && let Some(profile) = claude_code_profile_for_provider_id(provider_id)
        && let Some(companions) = &profile.companion_env_vars
    {
        return dedupe_refs(companions.iter().map(|value| static_str(value)).collect());
    }
    companion_env_refs_for_provider_id(provider_id)
}

pub fn optional_env_refs_for_provider_id(provider_id: &str) -> Vec<&'static str> {
    let mapping = ProviderKeyMapping::load_embedded();
    let mut refs: Vec<_> = mapping
        .mapping_for_provider_id(provider_id)
        .into_iter()
        .flat_map(|mapping| mapping.optional_env_vars.iter().map(String::as_str))
        .collect();
    if let Some(provider) = mapping.provider_mapping(provider_id) {
        refs.extend(provider.optional_env_vars.iter().map(String::as_str));
    }
    dedupe_refs(refs)
}

pub fn optional_env_refs_for_agent_provider_id(
    agent_id: &str,
    provider_id: &str,
) -> Vec<&'static str> {
    if agent_id == CLAUDE_CODE_AGENT_ID
        && let Some(profile) = claude_code_profile_for_provider_id(provider_id)
        && let Some(optional) = &profile.optional_env_vars
    {
        return dedupe_refs(optional.iter().map(|value| static_str(value)).collect());
    }
    optional_env_refs_for_provider_id(provider_id)
}

pub fn provider_ids_for_env_refs<'a>(
    env_refs: impl IntoIterator<Item = &'a str>,
) -> BTreeSet<&'static str> {
    let mapping = ProviderKeyMapping::load_embedded();
    let mut provider_ids = BTreeSet::new();
    for env_ref in env_refs {
        if let Some(key_mapping) = mapping.mapping_for_env_var(env_ref) {
            for provider_id in &key_mapping.provider_ids {
                if let Some(provider) = mapping.provider_mapping(provider_id) {
                    provider_ids.extend(provider.id.iter().map(String::as_str));
                } else {
                    provider_ids.insert(provider_id.as_str());
                }
            }
        }
        provider_ids.extend(
            mapping
                .providers
                .iter()
                .filter(|provider| {
                    provider
                        .api_key_env_vars
                        .values()
                        .any(|env_var| env_var == env_ref)
                })
                .flat_map(|provider| provider.id.iter().map(String::as_str)),
        );
    }
    provider_ids
}

pub fn env_ref_allows_provider(env_var: &str, provider_id: &str) -> bool {
    let mapping = ProviderKeyMapping::load_embedded();
    mapping_for_env_var(env_var).is_some_and(|key_mapping| {
        key_mapping.provider_ids.iter().any(|id| id == provider_id)
            || mapping
                .provider_mapping(provider_id)
                .is_some_and(|provider| {
                    key_mapping
                        .provider_ids
                        .iter()
                        .any(|key_provider_id| provider.contains_id(key_provider_id))
                })
    }) || mapping
        .provider_mapping(provider_id)
        .is_some_and(|provider| {
            provider
                .api_key_env_vars
                .values()
                .any(|mapped_env_var| mapped_env_var == env_var)
        })
}

/// Validate env-keyed credential values against a provider's canonical env-var contract: the
/// API-key var and every companion are required, optional vars are allowed, anything else rejected.
pub fn validate_env_keyed_credential_values(
    provider_id: &str,
    values: &BTreeMap<String, String>,
    field: &'static str,
) -> Result<()> {
    let Some(primary_env) = env_var_for_provider_id(provider_id) else {
        return Err(StackError::InvalidParam {
            field,
            reason: format!(
                "provider `{provider_id}` has no canonical API-key env var; env-keyed credential values cannot be applied to it"
            ),
        });
    };
    let companions = companion_env_refs_for_provider_id(provider_id);
    let optional = optional_env_refs_for_provider_id(provider_id);
    for required in std::iter::once(primary_env).chain(companions.iter().copied()) {
        if !values.contains_key(required) {
            return Err(StackError::InvalidParam {
                field,
                reason: format!(
                    "provider `{provider_id}` requires env var `{required}`; it is missing from the supplied values"
                ),
            });
        }
    }
    for (name, value) in values {
        let allowed = name == primary_env
            || companions.iter().any(|companion| companion == name)
            || optional.iter().any(|optional_ref| optional_ref == name);
        if !allowed {
            return Err(StackError::InvalidParam {
                field,
                reason: format!(
                    "env var `{name}` is not part of provider `{provider_id}`'s credential contract"
                ),
            });
        }
        if value.is_empty() {
            return Err(StackError::InvalidParam {
                field,
                reason: format!("value for env var `{name}` must not be empty"),
            });
        }
    }
    Ok(())
}

/// Validate env-keyed credential values for a custom provider: exactly the configured
/// `api_key_ref` env var, non-empty.
pub fn validate_custom_provider_credential_values(
    provider_id: &str,
    api_key_ref: &str,
    values: &BTreeMap<String, String>,
    field: &'static str,
) -> Result<()> {
    if !values.contains_key(api_key_ref) {
        return Err(StackError::InvalidParam {
            field,
            reason: format!(
                "custom provider `{provider_id}` requires env var `{api_key_ref}`; it is missing from the supplied values"
            ),
        });
    }
    for (name, value) in values {
        if name != api_key_ref {
            return Err(StackError::InvalidParam {
                field,
                reason: format!(
                    "env var `{name}` is not part of provider `{provider_id}`'s credential contract"
                ),
            });
        }
        if value.is_empty() {
            return Err(StackError::InvalidParam {
                field,
                reason: format!("value for env var `{name}` must not be empty"),
            });
        }
    }
    Ok(())
}

fn dedupe_refs(refs: Vec<&'static str>) -> Vec<&'static str> {
    let mut seen = HashSet::new();
    refs.into_iter().filter(|name| seen.insert(*name)).collect()
}

fn validate_tokens(field: String, values: &[String]) -> Result<()> {
    let mut seen = HashSet::new();
    for value in values {
        validate_token(&field, value)?;
        if !seen.insert(value.as_str()) {
            return provider_mapping_error(format!("duplicate value `{value}` in `{field}`"));
        }
    }
    Ok(())
}

fn validate_token(field: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        return provider_mapping_error(format!("`{field}` must not be empty"));
    }
    if value.trim() != value {
        return provider_mapping_error(format!(
            "`{field}` value `{value}` has surrounding whitespace"
        ));
    }
    Ok(())
}

fn is_supported_agent_id(agent_id: &str) -> bool {
    matches!(
        agent_id,
        "amp"
            | "antigravity"
            | "claude-code"
            | "codex"
            | "goose"
            | "hermes"
            | "kilo"
            | "kimi"
            | "opencode"
            | "pi"
    )
}

fn provider_mapping_error<T>(reason: String) -> Result<T> {
    Err(StackError::RegistryLoad { reason })
}

#[cfg(test)]
mod tests;
