//! Reusable provider/API-key compatibility mapping.
//!
//! The mapping itself is embedded data, not Rust control flow. Runtime code only
//! parses, validates, and queries it.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::LazyLock;

use serde::Deserialize;

use crate::config::{AgentConfig, AgentProviderConfig, Config};
use crate::error::{Result, StackError};
use crate::runtime::agent::claude_code_provider_profiles::{
    CLAUDE_CODE_AGENT_ID, profile_for_provider_id,
};
use crate::secrets::SecretStore;

const EMBEDDED_ENV_VARS: &str = include_str!("../../../data/env_vars.toml");
const EMBEDDED_PROVIDERS: &str = include_str!("../../../data/providers.toml");
const CODEX_AGENT_ID: &str = "codex";
const CODEX_NATIVE_AUTH_PROVIDER_ID: &str = "openai";

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
    if agent_id == CLAUDE_CODE_AGENT_ID
        && let Some(profile) = profile_for_provider_id(provider_id)
        && let Some(env_var) = profile.api_key_env_var.as_deref()
    {
        return Some(env_var);
    }
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProviderSnapshot {
    pub provider_id: String,
    pub alias: Option<String>,
    pub revision: Option<String>,
    pub env_names: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAgentEnvironment {
    pub env: HashMap<String, String>,
    pub providers: Vec<ResolvedProviderSnapshot>,
}

pub fn effective_active_provider_ids(agent: &AgentConfig) -> Vec<String> {
    let mut active = agent
        .providers
        .as_ref()
        .map(|providers| providers.active.clone())
        .unwrap_or_else(|| {
            agent
                .provider
                .as_ref()
                .map(|provider| vec![provider.id.clone()])
                .unwrap_or_default()
        });
    if let Some(subagent_provider) = agent
        .subagent
        .as_ref()
        .filter(|subagent| !subagent.disabled)
        .and_then(|subagent| subagent.provider.as_ref())
        && !active
            .iter()
            .any(|provider_id| provider_id == &subagent_provider.id)
    {
        active.push(subagent_provider.id.clone());
    }
    active
}

pub fn target_uses_provider(agent: &AgentConfig, provider_id: &str) -> bool {
    effective_active_provider_ids(agent)
        .iter()
        .any(|active| active == provider_id)
        || agent
            .providers
            .as_ref()
            .is_some_and(|providers| providers.selected_aliases.contains_key(provider_id))
}

pub fn resolve_agent_environment_without_secrets(
    config: &Config,
) -> Option<ResolvedAgentEnvironment> {
    if !config.agent.env.is_empty() {
        return None;
    }
    let mut providers = Vec::new();
    for provider_id in effective_active_provider_ids(&config.agent) {
        if !provider_id_is_known(&provider_id) {
            continue;
        }
        if !provider_uses_agent_native_auth(&config.agent.id, &provider_id) {
            return None;
        }
        providers.push(ResolvedProviderSnapshot {
            provider_id,
            alias: None,
            revision: None,
            env_names: Vec::new(),
        });
    }
    providers.sort_by(|left, right| left.provider_id.cmp(&right.provider_id));
    Some(ResolvedAgentEnvironment {
        env: HashMap::new(),
        providers,
    })
}

pub fn resolve_agent_environment(
    config: &Config,
    secrets: &SecretStore,
) -> Result<ResolvedAgentEnvironment> {
    let mut env = HashMap::with_capacity(config.agent.env.len());
    let mut owners: HashMap<String, Vec<String>> = HashMap::new();
    for name in &config.agent.env {
        let value = secrets.get(name)?.to_owned();
        insert_resolved_env(&mut env, &mut owners, name, value, "[agent].env")?;
    }

    let mut snapshots = Vec::new();
    for provider_id in effective_active_provider_ids(&config.agent) {
        if !provider_id_is_known(&provider_id) {
            // Custom (BYOK) providers are absent from the catalog, but their
            // api-key ref is already injected via the `[agent].env` loop above.
            // Surface them in the snapshot (env-name references only, no
            // revision) so status/API reflects custom-provider key rotation.
            // Genuinely unknown ids are left out.
            if let Some(provider) = custom_provider_config(&config.agent, &provider_id) {
                let mut env_names: Vec<String> = provider
                    .api_key_ref
                    .iter()
                    .filter(|name| env.contains_key(name.as_str()))
                    .cloned()
                    .collect();
                env_names.sort();
                env_names.dedup();
                snapshots.push(ResolvedProviderSnapshot {
                    provider_id,
                    alias: None,
                    revision: None,
                    env_names,
                });
            }
            continue;
        }
        if provider_uses_agent_native_auth(&config.agent.id, &provider_id) {
            let mut env_names =
                required_env_refs_for_agent_provider_id(&config.agent.id, &provider_id, None);
            env_names.extend(
                optional_env_refs_for_agent_provider_id(&config.agent.id, &provider_id)
                    .into_iter()
                    .map(str::to_owned),
            );
            env_names.retain(|env_name| env.contains_key(env_name));
            env_names.sort();
            env_names.dedup();
            snapshots.push(ResolvedProviderSnapshot {
                provider_id,
                alias: None,
                revision: None,
                env_names,
            });
            continue;
        }

        if let Some(credentials) = secrets.provider_credential_set(&provider_id) {
            let selected_alias = config
                .agent
                .providers
                .as_ref()
                .and_then(|providers| providers.selected_aliases.get(&provider_id))
                .map(String::as_str);
            let Some((credential, alias)) = credentials.selected(selected_alias) else {
                let reason = if credentials.is_promoted() && selected_alias.is_none() {
                    format!(
                        "provider `{provider_id}` has backup aliases; select one with `acps agent provider credential select {provider_id} <alias>`"
                    )
                } else {
                    format!("selected credential alias for provider `{provider_id}` does not exist")
                };
                return Err(StackError::InvalidParam {
                    field: "agent.providers.selected_aliases",
                    reason,
                });
            };
            let canonical_primary =
                env_var_for_provider_id(&provider_id).ok_or_else(|| StackError::InvalidParam {
                    field: "provider",
                    reason: format!("provider `{provider_id}` has no canonical API-key env var"),
                })?;
            let emitted_primary = env_var_for_agent_provider_id(&config.agent.id, &provider_id)
                .ok_or_else(|| StackError::InvalidParam {
                    field: "provider",
                    reason: format!(
                        "provider `{provider_id}` has no API-key env mapping for agent `{}`",
                        config.agent.id
                    ),
                })?;
            let primary_value = credential.values.get(canonical_primary).ok_or_else(|| {
                StackError::SecretStorePlaintextInvalid {
                    reason: format!(
                        "provider credential `{provider_id}` is missing `{canonical_primary}`"
                    ),
                }
            })?;
            let mut env_names = Vec::new();
            insert_resolved_env(
                &mut env,
                &mut owners,
                emitted_primary,
                primary_value.clone(),
                &provider_id,
            )?;
            env_names.push(emitted_primary.to_owned());

            for env_name in companion_env_refs_for_agent_provider_id(&config.agent.id, &provider_id)
            {
                let value = credential.values.get(env_name).ok_or_else(|| {
                    StackError::SecretStorePlaintextInvalid {
                        reason: format!(
                            "provider credential `{provider_id}` is missing required companion `{env_name}`"
                        ),
                    }
                })?;
                insert_resolved_env(&mut env, &mut owners, env_name, value.clone(), &provider_id)?;
                env_names.push(env_name.to_owned());
            }
            for env_name in optional_env_refs_for_agent_provider_id(&config.agent.id, &provider_id)
            {
                if let Some(value) = credential.values.get(env_name) {
                    insert_resolved_env(
                        &mut env,
                        &mut owners,
                        env_name,
                        value.clone(),
                        &provider_id,
                    )?;
                    env_names.push(env_name.to_owned());
                }
            }
            env_names.sort();
            env_names.dedup();
            snapshots.push(ResolvedProviderSnapshot {
                provider_id,
                alias: alias.map(str::to_owned),
                revision: Some(credential.revision.clone()),
                env_names,
            });
            continue;
        }

        let provider = legacy_provider_config(&config.agent, &provider_id).ok_or_else(|| {
            StackError::InvalidParam {
                field: "provider",
                reason: format!(
                    "provider `{provider_id}` has no credential; add one with `acps agent provider credential add {provider_id}`"
                ),
            }
        })?;
        let api_key_ref = provider.api_key_ref.as_deref().ok_or_else(|| {
            StackError::InvalidParam {
                field: "agent.provider.api_key_ref",
                reason: format!(
                    "provider `{provider_id}` has no structured credential or legacy API-key ref"
                ),
            }
        })?;
        let mut env_names = required_env_refs_for_agent_provider_id(
            &config.agent.id,
            &provider_id,
            Some(api_key_ref),
        );
        for env_name in &env_names {
            if !env.contains_key(env_name) {
                return Err(StackError::InvalidParam {
                    field: "agent.env",
                    reason: format!(
                        "provider `{provider_id}` requires configured secret ref `{env_name}`"
                    ),
                });
            }
        }
        env_names.sort();
        env_names.dedup();
        snapshots.push(ResolvedProviderSnapshot {
            provider_id,
            alias: None,
            revision: None,
            env_names,
        });
    }

    snapshots.sort_by(|left, right| left.provider_id.cmp(&right.provider_id));
    Ok(ResolvedAgentEnvironment {
        env,
        providers: snapshots,
    })
}

fn legacy_provider_config<'a>(
    agent: &'a AgentConfig,
    provider_id: &str,
) -> Option<&'a AgentProviderConfig> {
    agent
        .provider
        .as_ref()
        .filter(|provider| provider.id == provider_id && provider.custom.is_none())
        .or_else(|| {
            agent
                .subagent
                .as_ref()
                .filter(|subagent| !subagent.disabled)
                .and_then(|subagent| subagent.provider.as_ref())
                .filter(|provider| provider.id == provider_id && provider.custom.is_none())
        })
}

fn custom_provider_config<'a>(
    agent: &'a AgentConfig,
    provider_id: &str,
) -> Option<&'a AgentProviderConfig> {
    agent
        .provider
        .as_ref()
        .filter(|provider| provider.id == provider_id && provider.custom.is_some())
        .or_else(|| {
            agent
                .subagent
                .as_ref()
                .filter(|subagent| !subagent.disabled)
                .and_then(|subagent| subagent.provider.as_ref())
                .filter(|provider| provider.id == provider_id && provider.custom.is_some())
        })
}

fn insert_resolved_env(
    env: &mut HashMap<String, String>,
    owners: &mut HashMap<String, Vec<String>>,
    env_name: &str,
    value: String,
    owner: &str,
) -> Result<()> {
    if let Some(existing) = env.get(env_name) {
        if existing != &value {
            let mut conflict_owners = owners.get(env_name).cloned().unwrap_or_default();
            conflict_owners.push(owner.to_owned());
            conflict_owners.sort();
            conflict_owners.dedup();
            return Err(StackError::InvalidParam {
                field: "agent.providers.active",
                reason: format!(
                    "providers {} resolve different values for shared env `{env_name}`",
                    conflict_owners.join(", ")
                ),
            });
        }
        owners
            .entry(env_name.to_owned())
            .or_default()
            .push(owner.to_owned());
        return Ok(());
    }
    env.insert(env_name.to_owned(), value);
    owners.insert(env_name.to_owned(), vec![owner.to_owned()]);
    Ok(())
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

/// Apply one mapped provider to canonical Agent config. Init and native-config
/// import share this legacy-ref mutation; provider catalog commands use the
/// structured credential path above.
pub fn apply_mapped_agent_provider(
    config: &mut Config,
    provider_id: &str,
    requested_api_key_ref: Option<String>,
) -> Result<Vec<String>> {
    if !provider_id_is_known(provider_id)
        || !provider_id_supports_agent(provider_id, &config.agent.id)
    {
        return Err(StackError::InvalidParam {
            field: "provider",
            reason: format!(
                "provider `{provider_id}` is not supported for agent `{}`",
                config.agent.id
            ),
        });
    }
    let native_auth = provider_uses_agent_native_auth(&config.agent.id, provider_id);
    if native_auth && requested_api_key_ref.is_some() {
        return Err(StackError::InvalidParam {
            field: "api-key-ref",
            reason: format!(
                "provider `{provider_id}` uses agent-native auth and does not accept an API-key ref"
            ),
        });
    }
    let api_key_ref = requested_api_key_ref.or_else(|| {
        (!native_auth)
            .then(|| env_var_for_agent_provider_id(&config.agent.id, provider_id))
            .flatten()
            .map(str::to_owned)
    });
    if api_key_ref.is_none() && !native_auth {
        return Err(StackError::InvalidParam {
            field: "provider",
            reason: format!(
                "provider `{provider_id}` has no API-key env mapping for agent `{}`",
                config.agent.id
            ),
        });
    }
    let required_env_refs = required_env_refs_for_agent_provider_id(
        &config.agent.id,
        provider_id,
        api_key_ref.as_deref(),
    );
    for env_ref in &required_env_refs {
        if !config.agent.env.iter().any(|name| name == env_ref) {
            config.agent.env.push(env_ref.clone());
        }
    }
    config.agent.model = None;
    config.agent.provider = Some(AgentProviderConfig {
        id: provider_id.to_owned(),
        model: None,
        api_key_ref,
        custom: None,
    });
    if let Some(providers) = config.agent.providers.as_mut()
        && !providers.active.iter().any(|active| active == provider_id)
    {
        providers.active.push(provider_id.to_owned());
    }
    Ok(required_env_refs)
}

/// Apply a mapped provider when credentials are supplied by the structured
/// catalog rather than legacy `[agent].env` references.
pub fn apply_catalog_mapped_agent_provider(
    agent: &mut AgentConfig,
    provider_id: &str,
    multiple_active_providers: bool,
) -> Result<Vec<String>> {
    if !provider_id_is_known(provider_id) || !provider_id_supports_agent(provider_id, &agent.id) {
        return Err(StackError::InvalidParam {
            field: "provider",
            reason: format!(
                "provider `{provider_id}` is not supported for agent `{}`",
                agent.id
            ),
        });
    }
    let native_auth = provider_uses_agent_native_auth(&agent.id, provider_id);
    if !native_auth && env_var_for_agent_provider_id(&agent.id, provider_id).is_none() {
        return Err(StackError::InvalidParam {
            field: "provider",
            reason: format!(
                "provider `{provider_id}` has no API-key env mapping for agent `{}`",
                agent.id
            ),
        });
    }
    let required_env_refs = if native_auth {
        required_env_refs_for_agent_provider_id(&agent.id, provider_id, None)
    } else {
        Vec::new()
    };
    for env_ref in &required_env_refs {
        if !agent.env.iter().any(|name| name == env_ref) {
            agent.env.push(env_ref.clone());
        }
    }
    agent.model = None;
    agent.provider = Some(AgentProviderConfig {
        id: provider_id.to_owned(),
        model: None,
        api_key_ref: None,
        custom: None,
    });
    if let Some(providers) = agent.providers.as_mut() {
        if multiple_active_providers {
            if !providers.active.iter().any(|active| active == provider_id) {
                providers.active.push(provider_id.to_owned());
            }
        } else {
            providers.active = vec![provider_id.to_owned()];
        }
    }
    Ok(required_env_refs)
}

pub fn provider_uses_agent_native_auth(agent_id: &str, provider_id: &str) -> bool {
    (agent_id == CODEX_AGENT_ID && provider_id == CODEX_NATIVE_AUTH_PROVIDER_ID)
        || (agent_id == CLAUDE_CODE_AGENT_ID
            && profile_for_provider_id(provider_id)
                .is_some_and(|profile| profile.agent_native_auth))
}

pub fn provider_name_for_provider_id(provider_id: &str) -> Option<&'static str> {
    ProviderKeyMapping::load_embedded()
        .provider_mapping(provider_id)
        .map(|provider| provider.name.as_str())
}

/// Compact summary of one provider available to a given agent. Used by
/// the `/v1/providers` API and the future operator UI to render a
/// provider picker without any further mapping logic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentProviderSummary {
    /// Operator-facing provider id (the value the operator passes as
    /// `--provider`). Always a value listed in the embedded mapping.
    pub id: &'static str,
    /// Human-readable name pulled from the provider mapping.
    pub name: &'static str,
    /// Agent-native provider id when the agent uses a different label
    /// than the operator-facing id (e.g. Codex uses `openai` natively
    /// but the operator might pass `openai-chat`). `None` when the
    /// agent uses the same id.
    pub agent_provider_id: Option<&'static str>,
    /// Default API-key env var ref for this (agent, provider) pair, if
    /// the embedded mapping declares one. `None` indicates the
    /// operator must configure a custom provider OR the provider uses
    /// agent-native auth (e.g. Codex+OpenAI).
    pub default_api_key_ref: Option<&'static str>,
    /// Required companion env vars beyond the API key.
    pub companion_env_refs: Vec<&'static str>,
    /// Optional env vars the operator may set for this provider.
    pub optional_env_refs: Vec<&'static str>,
}

/// Every operator-facing provider id supported for `agent_id`, in
/// embedded-mapping order. Empty when the agent has no provider scope.
pub fn providers_for_agent(agent_id: &str) -> Vec<AgentProviderSummary> {
    let mapping = ProviderKeyMapping::load_embedded();
    let mut seen: BTreeSet<&'static str> = BTreeSet::new();
    let mut summaries = Vec::new();
    for provider in &mapping.providers {
        if !provider.agents.iter().any(|agent| agent == agent_id) {
            continue;
        }
        for id in &provider.id {
            // Each provider mapping may list multiple alias ids
            // (e.g. `openai` + `openai-chat`). Emit each as its own
            // operator-facing entry so the API surface mirrors what
            // `acps init --provider <id>` accepts.
            if !seen.insert(static_str(id)) {
                continue;
            }
            let id_static = static_str(id);
            let mut default = env_var_for_agent_provider_id(agent_id, id_static);
            // Codex + OpenAI is special-cased throughout the CLI: it
            // uses Codex-native auth, NOT `OPENAI_API_KEY`. Advertising
            // a default here would let a UI client write a config the
            // CLI then rejects with "Codex OpenAI uses Codex-native
            // auth". Drop the default so clients see "no api_key_ref
            // required" and route through Codex's own login flow.
            if provider_uses_agent_native_auth(agent_id, id_static) {
                default = None;
            }
            let native = provider.agent_native_provider_id(agent_id).map(static_str);
            // Only surface `agent_provider_id` when it actually differs
            // from the operator-facing id. Always serializing it
            // (even when equal) made every provider look like an
            // alias, which the docs explicitly say it isn't.
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

/// Re-borrow an embedded `String` as a `'static` `&str`. The provider
/// mapping is loaded into a `LazyLock` that lives for the program's
/// lifetime, so any string borrowed from it is effectively `'static`;
/// the explicit transmute makes that promise explicit and lets the
/// summary structs hold `&'static str` for cheap cloning.
fn static_str(value: &str) -> &'static str {
    // SAFETY: `value` is borrowed from `PROVIDER_KEY_MAPPING`, a
    // `LazyLock<ProviderKeyMapping>` that is never dropped. Extending
    // the lifetime to `'static` is sound because the underlying
    // allocation outlives the program.
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
        && let Some(profile) = profile_for_provider_id(provider_id)
    {
        let mut refs = Vec::new();
        if let Some(api_key_ref) = api_key_ref {
            refs.push(api_key_ref.to_owned());
        }
        refs.extend(profile.companion_env_vars.iter().cloned());
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
        && let Some(profile) = profile_for_provider_id(provider_id)
    {
        return dedupe_refs(
            profile
                .companion_env_vars
                .iter()
                .map(|value| static_str(value))
                .collect(),
        );
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
        && let Some(profile) = profile_for_provider_id(provider_id)
    {
        return dedupe_refs(
            profile
                .optional_env_vars
                .iter()
                .map(|value| static_str(value))
                .collect(),
        );
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

/// Validate env-keyed credential values against a provider's canonical env-var
/// contract. `field` attributes rejections to the caller's input field.
///
/// The supplied keys must include the canonical API-key env var and every
/// required companion, and may include the provider's optional env vars;
/// anything else is rejected rather than guessed at, because a guessed mapping
/// would surface later as a spawn-time env resolution failure instead of a
/// clear rejection here.
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
        "amp" | "claude-code" | "codex" | "cursor" | "goose" | "kimi" | "opencode" | "pi"
    )
}

fn provider_mapping_error<T>(reason: String) -> Result<T> {
    Err(StackError::RegistryLoad { reason })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AgentProviderConfig, AgentProvidersConfig, load_config_from_str};
    use crate::secrets::{ProviderCredential, ProviderCredentialSet};
    use std::collections::BTreeMap;

    fn resolver_config(agent_id: &str) -> Config {
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
id = "{agent_id}"
name = "Test Agent"
command = "{agent_id}"
args = []
cwd = "/workspace"
env = []
restart = "on-crash"
"#
        ))
        .expect("config parses")
    }

    fn mapped_provider(provider_id: &str, api_key_ref: Option<&str>) -> AgentProviderConfig {
        AgentProviderConfig {
            id: provider_id.to_owned(),
            model: None,
            api_key_ref: api_key_ref.map(str::to_owned),
            custom: None,
        }
    }

    fn credential(env_name: &str, value: &str) -> ProviderCredential {
        ProviderCredential::new(
            BTreeMap::from([(env_name.to_owned(), value.to_owned())]),
            BTreeMap::new(),
        )
    }

    fn catalog_store(
        catalog: BTreeMap<String, ProviderCredentialSet>,
    ) -> (tempfile::TempDir, SecretStore) {
        let home = tempfile::tempdir().expect("home");
        let mut store = SecretStore::open_or_create(home.path()).expect("secret store");
        store
            .replace_provider_credentials(catalog, &[])
            .expect("catalog");
        (home, store)
    }

    #[test]
    fn structured_provider_environment_resolves_selected_aliases() {
        let mut config = resolver_config("opencode");
        config.agent.provider = Some(mapped_provider("opencode-go", None));
        config.agent.providers = Some(AgentProvidersConfig {
            active: vec!["opencode-go".to_owned(), "openrouter".to_owned()],
            selected_aliases: BTreeMap::from([("opencode-go".to_owned(), "go_2".to_owned())]),
        });
        let (_home, store) = catalog_store(BTreeMap::from([
            (
                "opencode-go".to_owned(),
                ProviderCredentialSet::promoted(BTreeMap::from([(
                    "go_2".to_owned(),
                    credential("OPENCODE_API_KEY", "go-key"),
                )])),
            ),
            (
                "openrouter".to_owned(),
                ProviderCredentialSet::aliasless(credential("OPENROUTER_API_KEY", "router-key")),
            ),
        ]));

        let resolved = resolve_agent_environment(&config, &store).expect("resolve");

        assert_eq!(resolved.env["OPENCODE_API_KEY"], "go-key");
        assert_eq!(resolved.env["OPENROUTER_API_KEY"], "router-key");
        assert_eq!(resolved.providers.len(), 2);
        assert_eq!(resolved.providers[0].alias.as_deref(), Some("go_2"));
        assert!(
            resolved
                .providers
                .iter()
                .all(|provider| provider.revision.is_some())
        );
    }

    #[test]
    fn shared_env_deduplicates_equal_values_and_rejects_different_values() {
        let mut config = resolver_config("opencode");
        config.agent.provider = Some(mapped_provider("opencode", None));
        config.agent.providers = Some(AgentProvidersConfig {
            active: vec!["opencode".to_owned(), "opencode-go".to_owned()],
            selected_aliases: BTreeMap::new(),
        });
        let (_home, store) = catalog_store(BTreeMap::from([
            (
                "opencode".to_owned(),
                ProviderCredentialSet::aliasless(credential("OPENCODE_API_KEY", "shared")),
            ),
            (
                "opencode-go".to_owned(),
                ProviderCredentialSet::aliasless(credential("OPENCODE_API_KEY", "shared")),
            ),
        ]));
        let resolved = resolve_agent_environment(&config, &store).expect("equal values resolve");
        assert_eq!(resolved.env.len(), 1);
        assert_eq!(resolved.providers.len(), 2);

        let (_home, store) = catalog_store(BTreeMap::from([
            (
                "opencode".to_owned(),
                ProviderCredentialSet::aliasless(credential("OPENCODE_API_KEY", "first")),
            ),
            (
                "opencode-go".to_owned(),
                ProviderCredentialSet::aliasless(credential("OPENCODE_API_KEY", "second")),
            ),
        ]));
        let error = resolve_agent_environment(&config, &store).expect_err("collision");
        let message = error.to_string();
        assert!(message.contains("opencode"));
        assert!(message.contains("opencode-go"));
        assert!(message.contains("OPENCODE_API_KEY"));
        assert!(!message.contains("first"));
        assert!(!message.contains("second"));
    }

    #[test]
    fn legacy_flat_ref_remains_the_implicit_single_provider() {
        let mut config = resolver_config("opencode");
        config.agent.env.push("LEGACY_GO_KEY".to_owned());
        config.agent.provider = Some(mapped_provider("opencode-go", Some("LEGACY_GO_KEY")));
        let home = tempfile::tempdir().expect("home");
        let mut store = SecretStore::open_or_create(home.path()).expect("secret store");
        store.set("LEGACY_GO_KEY", "legacy").expect("legacy secret");

        let resolved = resolve_agent_environment(&config, &store).expect("resolve");

        assert_eq!(resolved.env["LEGACY_GO_KEY"], "legacy");
        assert_eq!(resolved.providers.len(), 1);
        assert_eq!(resolved.providers[0].provider_id, "opencode-go");
        assert!(resolved.providers[0].revision.is_none());

        config.agent.env.clear();
        let error = resolve_agent_environment(&config, &store).expect_err("missing legacy ref");
        assert!(error.to_string().contains("LEGACY_GO_KEY"));
    }

    #[test]
    fn secretless_resolution_skips_store_only_for_empty_or_native_auth_envs() {
        let config = resolver_config("amp");
        let resolved =
            resolve_agent_environment_without_secrets(&config).expect("empty environment");
        assert!(resolved.env.is_empty());
        assert!(resolved.providers.is_empty());

        let mut config = resolver_config("codex");
        config.agent.provider = Some(mapped_provider("openai", None));
        let resolved = resolve_agent_environment_without_secrets(&config).expect("native auth");
        assert_eq!(resolved.providers[0].provider_id, "openai");

        let mut config = resolver_config("opencode");
        config.agent.provider = Some(mapped_provider("opencode-go", None));
        assert!(resolve_agent_environment_without_secrets(&config).is_none());
    }

    #[test]
    fn native_auth_snapshot_reports_injected_profile_environment() {
        let mut config = resolver_config("claude-code");
        config.agent.provider = Some(mapped_provider("google-vertex-anthropic", None));
        config.agent.env = vec![
            "ANTHROPIC_VERTEX_PROJECT_ID".to_owned(),
            "CLOUD_ML_REGION".to_owned(),
        ];
        let home = tempfile::tempdir().expect("home");
        let mut store = SecretStore::open_or_create(home.path()).expect("secret store");
        store
            .set_many([
                ("ANTHROPIC_VERTEX_PROJECT_ID", "project"),
                ("CLOUD_ML_REGION", "region"),
            ])
            .expect("profile secrets");

        let resolved = resolve_agent_environment(&config, &store).expect("resolve native auth");

        assert_eq!(
            resolved.providers[0].env_names,
            ["ANTHROPIC_VERTEX_PROJECT_ID", "CLOUD_ML_REGION"]
        );
    }

    #[test]
    fn embedded_mapping_loads_and_validates() {
        let mapping = ProviderKeyMapping::from_toml_parts(EMBEDDED_ENV_VARS, EMBEDDED_PROVIDERS)
            .expect("mapping parses");

        assert!(!mapping.api_keys().is_empty());
        assert!(!mapping.providers.is_empty());
        assert!(
            mapping
                .providers
                .iter()
                .all(|provider| !provider.ids().is_empty())
        );
    }

    #[test]
    fn opencode_api_key_allows_both_opencode_provider_ids() {
        assert!(env_ref_allows_provider("OPENCODE_API_KEY", "opencode"));
        assert!(env_ref_allows_provider("OPENCODE_API_KEY", "opencode-go"));
        assert!(!env_ref_allows_provider("OPENCODE_API_KEY", "openai"));
        assert_eq!(
            env_var_for_provider_id("opencode"),
            Some("OPENCODE_API_KEY")
        );
        assert_eq!(
            env_var_for_provider_id("opencode-go"),
            Some("OPENCODE_API_KEY")
        );
    }

    #[test]
    fn agent_specific_api_key_env_vars_are_data_driven() {
        assert_eq!(
            env_var_for_agent_provider_id("opencode", "cloudflare-ai-gateway"),
            Some("CLOUDFLARE_API_TOKEN")
        );
        assert_eq!(
            env_var_for_agent_provider_id("pi", "cloudflare-ai-gateway"),
            Some("CLOUDFLARE_API_KEY")
        );
        assert!(env_ref_allows_provider(
            "CLOUDFLARE_API_TOKEN",
            "cloudflare-ai-gateway"
        ));
    }

    #[test]
    fn provider_ids_collect_from_configured_secret_refs() {
        let providers = provider_ids_for_env_refs([
            "OPENAI_API_KEY",
            "CLOUDFLARE_API_KEY",
            "CLOUDFLARE_API_TOKEN",
            "AI_GATEWAY_API_KEY",
            "UNKNOWN_KEY",
        ]);

        assert_eq!(
            providers.into_iter().collect::<Vec<_>>(),
            [
                "cloudflare-ai-gateway",
                "cloudflare-workers-ai",
                "openai",
                "vercel",
                "vercel-ai-gateway"
            ]
        );
    }

    #[test]
    fn provider_ids_resolve_to_primary_api_key_env_vars() {
        assert_eq!(env_var_for_provider_id("openai"), Some("OPENAI_API_KEY"));
        assert_eq!(
            env_var_for_provider_id("cloudflare-ai-gateway"),
            Some("CLOUDFLARE_API_KEY")
        );
        assert_eq!(
            env_var_for_provider_id("vercel-ai-gateway"),
            Some("AI_GATEWAY_API_KEY")
        );
        assert_eq!(
            env_var_for_provider_id("vercel"),
            Some("AI_GATEWAY_API_KEY")
        );
        assert_eq!(
            env_var_for_provider_id("fireworks"),
            Some("FIREWORKS_API_KEY")
        );
        assert_eq!(
            env_var_for_provider_id("fireworks-ai"),
            Some("FIREWORKS_API_KEY")
        );
        assert_eq!(env_var_for_provider_id("huggingface"), Some("HF_TOKEN"));
        assert_eq!(env_var_for_provider_id("zai"), Some("ZAI_API_KEY"));
        assert_eq!(
            env_var_for_provider_id("moonshotai"),
            Some("MOONSHOT_API_KEY")
        );
        assert_eq!(
            env_var_for_provider_id("minimax-coding-plan"),
            Some("MINIMAX_API_KEY")
        );
        assert_eq!(
            env_var_for_provider_id("microsoft-foundry"),
            Some("ANTHROPIC_FOUNDRY_API_KEY")
        );
    }

    #[test]
    fn cloudflare_provider_refs_include_documented_companions() {
        assert_eq!(
            required_env_refs_for_provider_id("cloudflare-workers-ai", "CLOUDFLARE_API_KEY"),
            ["CLOUDFLARE_API_KEY", "CLOUDFLARE_ACCOUNT_ID"]
        );
        assert_eq!(
            required_env_refs_for_provider_id("cloudflare-ai-gateway", "CLOUDFLARE_API_KEY"),
            [
                "CLOUDFLARE_API_KEY",
                "CLOUDFLARE_ACCOUNT_ID",
                "CLOUDFLARE_GATEWAY_ID"
            ]
        );
        assert_eq!(
            required_env_refs_for_provider_id("cloudflare-ai-gateway", "CLOUDFLARE_API_TOKEN"),
            [
                "CLOUDFLARE_API_TOKEN",
                "CLOUDFLARE_ACCOUNT_ID",
                "CLOUDFLARE_GATEWAY_ID"
            ]
        );
        assert!(optional_env_refs_for_provider_id("cloudflare-workers-ai").is_empty());
        assert!(optional_env_refs_for_provider_id("cloudflare-ai-gateway").is_empty());
    }

    #[test]
    fn provider_metadata_includes_models_dev_display_names() {
        let mapping = ProviderKeyMapping::load_embedded();

        assert_eq!(
            mapping
                .provider_mapping("opencode-go")
                .map(|provider| provider.name.as_str()),
            Some("OpenCode Go")
        );
        assert_eq!(
            mapping
                .provider_mapping("cloudflare-ai-gateway")
                .map(|provider| provider.name.as_str()),
            Some("Cloudflare AI Gateway")
        );
    }

    #[test]
    fn provider_lookup_works_for_every_collapsed_provider_id() {
        let mapping = ProviderKeyMapping::load_embedded();

        for provider_id in [
            "vercel-ai-gateway",
            "vercel",
            "fireworks",
            "fireworks-ai",
            "together",
            "togetherai",
            "kimi-coding",
            "kimi-for-coding",
        ] {
            let provider = mapping
                .provider_mapping(provider_id)
                .expect("collapsed provider id resolves");
            assert!(provider.ids().iter().any(|id| id == provider_id));
        }
    }

    #[test]
    fn models_dev_only_providers_are_opencode_scoped_without_default_env_refs() {
        for provider_id in ["helicone", "deepinfra", "github-models", "venice"] {
            assert!(provider_id_is_known(provider_id));
            assert!(provider_id_supports_agent(provider_id, "opencode"));
            assert!(!provider_id_supports_agent(provider_id, "pi"));
            assert_eq!(env_var_for_provider_id(provider_id), None);
            assert_eq!(env_var_for_agent_provider_id("opencode", provider_id), None);
        }
    }

    #[test]
    fn azure_provider_refs_include_base_url_and_documented_options() {
        assert_eq!(
            required_env_refs_for_provider_id("azure-openai-responses", "AZURE_OPENAI_API_KEY"),
            ["AZURE_OPENAI_API_KEY", "AZURE_OPENAI_BASE_URL"]
        );
        assert_eq!(
            optional_env_refs_for_provider_id("azure-openai-responses"),
            [
                "AZURE_OPENAI_RESOURCE_NAME",
                "AZURE_OPENAI_API_VERSION",
                "AZURE_OPENAI_DEPLOYMENT_NAME_MAP"
            ]
        );
    }

    #[test]
    fn provider_metadata_scopes_supported_agents() {
        assert!(provider_id_supports_agent("fireworks", "pi"));
        assert!(provider_id_supports_agent("fireworks", "opencode"));
        assert!(provider_id_supports_agent("fireworks-ai", "opencode"));
        assert!(provider_id_supports_agent("fireworks-ai", "pi"));
        assert!(provider_id_supports_agent("openai", "pi"));
        assert!(provider_id_supports_agent("openai", "opencode"));
        assert!(provider_id_supports_agent("openai", "codex"));
        assert!(provider_id_supports_agent("openrouter", "codex"));
        assert!(!provider_id_supports_agent("anthropic", "codex"));
        assert!(provider_id_supports_agent("anthropic", "claude-code"));
        assert!(provider_id_supports_agent("amazon-bedrock", "claude-code"));
        assert!(provider_id_supports_agent(
            "google-vertex-anthropic",
            "claude-code"
        ));
        assert!(provider_id_supports_agent(
            "microsoft-foundry",
            "claude-code"
        ));
        assert!(provider_id_supports_agent("moonshotai", "claude-code"));
        assert!(provider_id_supports_agent(
            "xiaomi-token-plan-sgp",
            "claude-code"
        ));
        assert!(!provider_id_supports_agent("xai", "codex"));
        assert!(!provider_id_supports_agent("openai", "cursor"));
        assert!(provider_id_supports_agent("anthropic", "goose"));
        assert!(provider_id_supports_agent("openai", "goose"));
        assert!(provider_id_supports_agent("mistral", "goose"));
        assert!(provider_id_supports_agent("groq", "goose"));
        assert!(provider_id_supports_agent("openrouter", "goose"));
        assert!(provider_id_supports_agent("cerebras", "goose"));
        assert!(provider_id_supports_agent("xai", "goose"));
        assert!(!provider_id_supports_agent("deepseek", "goose"));
        assert_eq!(env_var_for_agent_provider_id("cursor", "openai"), None);
        assert_eq!(
            env_var_for_agent_provider_id("goose", "openrouter"),
            Some("OPENROUTER_API_KEY")
        );
        assert_eq!(
            env_var_for_agent_provider_id("codex", "openrouter"),
            Some("OPENROUTER_API_KEY")
        );
        assert_eq!(
            env_var_for_agent_provider_id("claude-code", "moonshotai"),
            Some("MOONSHOT_API_KEY")
        );
        assert_eq!(
            env_var_for_agent_provider_id("claude-code", "amazon-bedrock"),
            None
        );
    }

    #[test]
    fn agent_native_provider_ids_are_data_driven() {
        assert_eq!(
            agent_provider_id_for_provider_id("pi", "vercel"),
            Some("vercel-ai-gateway")
        );
        assert_eq!(
            agent_provider_id_for_provider_id("opencode", "vercel-ai-gateway"),
            Some("vercel")
        );
        assert_eq!(
            agent_provider_id_for_provider_id("pi", "fireworks-ai"),
            Some("fireworks")
        );
        assert_eq!(
            agent_provider_id_for_provider_id("opencode", "fireworks"),
            Some("fireworks-ai")
        );
        assert_eq!(
            agent_provider_id_for_provider_id("pi", "togetherai"),
            Some("together")
        );
        assert_eq!(
            agent_provider_id_for_provider_id("opencode", "together"),
            Some("togetherai")
        );
        assert_eq!(
            agent_provider_id_for_provider_id("pi", "kimi-for-coding"),
            Some("kimi-coding")
        );
        assert_eq!(
            agent_provider_id_for_provider_id("opencode", "kimi-coding"),
            Some("kimi-for-coding")
        );
        assert_eq!(agent_provider_id_for_provider_id("cursor", "openai"), None);
    }

    #[test]
    fn pi_native_config_provider_ids_resolve_to_canonical() {
        // `inspect_pi` maps a `defaultProvider` value through
        // `canonical_provider_id_for_agent_native_id("pi", ...)`. Pi's native
        // ids match acps canonical ids for shared providers and differ only
        // where the mapping declares an alias.
        assert_eq!(
            canonical_provider_id_for_agent_native_id("pi", "anthropic"),
            Some("anthropic")
        );
        assert_eq!(
            canonical_provider_id_for_agent_native_id("pi", "openai"),
            Some("openai")
        );
        // Pi's `vercel-ai-gateway`/`fireworks`/`together` native ids collapse
        // to the same canonical id acps stores.
        assert_eq!(
            canonical_provider_id_for_agent_native_id("pi", "vercel-ai-gateway"),
            Some("vercel-ai-gateway")
        );
        assert_eq!(
            canonical_provider_id_for_agent_native_id("pi", "fireworks"),
            Some("fireworks")
        );
        // A provider Pi lists but acps does not map for `pi` yields no
        // canonical id, so the import surfaces an incompatible candidate.
        assert_eq!(
            canonical_provider_id_for_agent_native_id("pi", "totally-unknown-provider"),
            None
        );
    }

    #[test]
    fn direct_agent_env_refs_are_data_driven() {
        assert_eq!(env_refs_for_agent_id("amp"), ["AMP_API_KEY"]);
        assert_eq!(env_refs_for_agent_id("cursor"), ["CURSOR_API_KEY"]);
        assert_eq!(env_refs_for_agent_id("kimi"), ["KIMI_API_KEY"]);
        assert!(env_refs_for_agent_id("opencode").is_empty());
    }

    #[test]
    fn cloud_provider_refs_include_documented_non_key_fields() {
        assert_eq!(
            companion_env_refs_for_provider_id("google-vertex"),
            ["GOOGLE_CLOUD_PROJECT", "GOOGLE_CLOUD_LOCATION"]
        );
        assert_eq!(
            optional_env_refs_for_provider_id("google-vertex"),
            ["GOOGLE_APPLICATION_CREDENTIALS"]
        );
        let bedrock = optional_env_refs_for_provider_id("amazon-bedrock");
        assert!(bedrock.contains(&"AWS_PROFILE"));
        assert!(bedrock.contains(&"AWS_ACCESS_KEY_ID"));
        assert!(bedrock.contains(&"AWS_CONTAINER_CREDENTIALS_RELATIVE_URI"));
        assert!(bedrock.contains(&"AWS_WEB_IDENTITY_TOKEN_FILE"));
    }

    #[test]
    fn claude_code_provider_refs_use_agent_specific_profiles() {
        assert_eq!(
            required_env_refs_for_agent_provider_id("claude-code", "google-vertex-anthropic", None),
            ["ANTHROPIC_VERTEX_PROJECT_ID", "CLOUD_ML_REGION"]
        );
        assert_eq!(
            required_env_refs_for_agent_provider_id(
                "claude-code",
                "microsoft-foundry",
                Some("ANTHROPIC_FOUNDRY_API_KEY")
            ),
            ["ANTHROPIC_FOUNDRY_API_KEY", "ANTHROPIC_FOUNDRY_BASE_URL"]
        );
        assert!(provider_uses_agent_native_auth(
            "claude-code",
            "amazon-bedrock"
        ));
        assert!(provider_uses_agent_native_auth(
            "claude-code",
            "google-vertex-anthropic"
        ));
        assert!(!provider_uses_agent_native_auth(
            "claude-code",
            "microsoft-foundry"
        ));

        let summaries = providers_for_agent("claude-code");
        let bedrock = summaries
            .iter()
            .find(|summary| summary.id == "amazon-bedrock")
            .expect("Bedrock should be listed for Claude Code");
        assert_eq!(bedrock.default_api_key_ref, None);
        assert!(bedrock.optional_env_refs.contains(&"AWS_PROFILE"));
        let foundry = summaries
            .iter()
            .find(|summary| summary.id == "microsoft-foundry")
            .expect("Foundry should be listed for Claude Code");
        assert_eq!(
            foundry.default_api_key_ref,
            Some("ANTHROPIC_FOUNDRY_API_KEY")
        );
        assert_eq!(foundry.companion_env_refs, ["ANTHROPIC_FOUNDRY_BASE_URL"]);
    }

    #[test]
    fn invalid_mapping_rejects_duplicate_provider_ids() {
        let err = ProviderKeyMapping::from_toml(
            r#"
[[api_keys]]
env_var = "FIRST_API_KEY"
provider_ids = ["same"]

[[api_keys]]
env_var = "SECOND_API_KEY"
provider_ids = ["same"]

[[providers]]
id = ["same"]
name = "Same"
agents = ["pi"]
"#,
        )
        .expect_err("duplicate provider id fails");

        assert!(err.to_string().contains("duplicate provider id `same`"));
    }

    #[test]
    fn invalid_mapping_rejects_duplicate_provider_metadata_ids() {
        let err = ProviderKeyMapping::from_toml(
            r#"
[[api_keys]]
env_var = "FIRST_API_KEY"
provider_ids = ["same"]

[[providers]]
id = ["same", "alias"]
name = "Same"
agents = ["pi"]

[[providers]]
id = ["alias"]
name = "Alias"
agents = ["opencode"]
"#,
        )
        .expect_err("duplicate provider metadata id fails");

        assert!(
            err.to_string()
                .contains("duplicate provider env mapping `alias`")
        );
    }

    #[test]
    fn invalid_mapping_rejects_native_provider_id_for_unknown_id() {
        let err = ProviderKeyMapping::from_toml(
            r#"
[[api_keys]]
env_var = "FIRST_API_KEY"
provider_ids = ["known"]

[[providers]]
id = ["known"]
name = "Known"
agents = ["pi"]

[providers.provider_ids]
missing = "pi"
"#,
        )
        .expect_err("unknown native provider id fails");

        assert!(
            err.to_string()
                .contains("maps unknown native provider id `missing`")
        );
    }

    #[test]
    fn invalid_mapping_rejects_duplicate_native_agent_mapping() {
        let err = ProviderKeyMapping::from_toml(
            r#"
[[api_keys]]
env_var = "FIRST_API_KEY"
provider_ids = ["known"]

[[providers]]
id = ["known", "alias"]
name = "Known"
agents = ["pi"]

[providers.provider_ids]
known = "pi"
alias = "pi"
"#,
        )
        .expect_err("duplicate native agent mapping fails");

        assert!(
            err.to_string()
                .contains("multiple native provider ids for agent `pi`")
        );
    }

    #[test]
    fn invalid_mapping_rejects_empty_values() {
        let err = ProviderKeyMapping::from_toml(
            r#"
[[api_keys]]
env_var = ""
provider_ids = ["openai"]

[[providers]]
id = ["openai"]
name = "OpenAI"
agents = ["pi"]
"#,
        )
        .expect_err("empty env var fails");

        assert!(err.to_string().contains("must not be empty"));
    }

    #[test]
    fn subagent_provider_structured_key_resolves_without_active_block() {
        // Guards the subagent-discovery-auth fix: once the subagent provider is
        // registered (which `configure_mapped_subagent` now does before model
        // discovery), its structured credential resolves into the probe env
        // even with no `[agent.providers]` active block.
        use crate::config::AgentSubagentConfig;
        let mut config = resolver_config("opencode");
        config.agent.provider = Some(mapped_provider("openai", None));
        config.agent.subagent = Some(AgentSubagentConfig {
            disabled: false,
            provider: Some(mapped_provider("opencode-go", None)),
        });
        let (_home, store) = catalog_store(BTreeMap::from([
            (
                "openai".to_owned(),
                ProviderCredentialSet::aliasless(credential("OPENAI_API_KEY", "openai-key")),
            ),
            (
                "opencode-go".to_owned(),
                ProviderCredentialSet::aliasless(credential("OPENCODE_API_KEY", "go-key")),
            ),
        ]));

        let resolved = resolve_agent_environment(&config, &store).expect("resolve");

        assert_eq!(resolved.env["OPENCODE_API_KEY"], "go-key");
        assert_eq!(resolved.env["OPENAI_API_KEY"], "openai-key");
        assert!(
            resolved
                .providers
                .iter()
                .any(|provider| provider.provider_id == "opencode-go")
        );
    }

    #[test]
    fn custom_provider_appears_in_resolved_snapshot() {
        use crate::config::{AgentCustomProviderConfig, CustomProviderApi};
        let mut config = resolver_config("opencode");
        config.agent.env = vec!["CUSTOM_KEY".to_owned()];
        config.agent.provider = Some(AgentProviderConfig {
            id: "my-custom".to_owned(),
            model: None,
            api_key_ref: Some("CUSTOM_KEY".to_owned()),
            custom: Some(AgentCustomProviderConfig {
                name: "My Custom".to_owned(),
                base_url: "https://example.test/v1".to_owned(),
                api: CustomProviderApi::default(),
                model_name: None,
                context: 128_000,
                output_max_tokens: 8_192,
            }),
        });
        let home = tempfile::tempdir().expect("home");
        let mut store = SecretStore::open_or_create(home.path()).expect("secret store");
        store.set("CUSTOM_KEY", "custom-secret").expect("flat key");

        let resolved = resolve_agent_environment(&config, &store).expect("resolve");

        let snapshot = resolved
            .providers
            .iter()
            .find(|provider| provider.provider_id == "my-custom")
            .expect("custom provider snapshot present");
        assert_eq!(snapshot.env_names, vec!["CUSTOM_KEY".to_owned()]);
        assert!(snapshot.revision.is_none());
        assert!(snapshot.alias.is_none());
    }

    fn invalid_param_reason(error: StackError) -> String {
        match error {
            StackError::InvalidParam { reason, .. } => reason,
            other => panic!("expected InvalidParam, got {other:?}"),
        }
    }

    #[test]
    fn env_keyed_values_accept_canonical_single_key() {
        let values = BTreeMap::from([("OPENAI_API_KEY".to_owned(), "sk-value".to_owned())]);
        validate_env_keyed_credential_values("openai", &values, "test.values")
            .expect("canonical single-key credential is valid");
    }

    #[test]
    fn env_keyed_values_reject_unknown_provider() {
        let values = BTreeMap::from([("SOME_KEY".to_owned(), "value".to_owned())]);
        let error =
            validate_env_keyed_credential_values("no-such-provider", &values, "test.values")
                .expect_err("unknown provider must be rejected");
        assert!(invalid_param_reason(error).contains("no canonical API-key env var"));
    }

    #[test]
    fn env_keyed_values_reject_missing_required_companion() {
        let primary = env_var_for_provider_id("cloudflare-ai-gateway")
            .expect("cloudflare-ai-gateway has a canonical env var");
        let companions = companion_env_refs_for_provider_id("cloudflare-ai-gateway");
        assert!(
            !companions.is_empty(),
            "fixture provider must require companions"
        );
        let values = BTreeMap::from([(primary.to_owned(), "cf-key".to_owned())]);
        let error =
            validate_env_keyed_credential_values("cloudflare-ai-gateway", &values, "test.values")
                .expect_err("missing companion must be rejected");
        assert!(invalid_param_reason(error).contains(companions[0]));
    }

    #[test]
    fn env_keyed_values_accept_full_companion_set() {
        let primary = env_var_for_provider_id("cloudflare-ai-gateway")
            .expect("cloudflare-ai-gateway has a canonical env var");
        let mut values = BTreeMap::from([(primary.to_owned(), "cf-key".to_owned())]);
        for companion in companion_env_refs_for_provider_id("cloudflare-ai-gateway") {
            values.insert(companion.to_owned(), "companion-value".to_owned());
        }
        validate_env_keyed_credential_values("cloudflare-ai-gateway", &values, "test.values")
            .expect("full companion set is valid");
    }

    #[test]
    fn env_keyed_values_reject_key_outside_contract() {
        let values = BTreeMap::from([
            ("OPENAI_API_KEY".to_owned(), "sk-value".to_owned()),
            ("UNRELATED_ENV".to_owned(), "value".to_owned()),
        ]);
        let error = validate_env_keyed_credential_values("openai", &values, "test.values")
            .expect_err("key outside the provider contract must be rejected");
        assert!(invalid_param_reason(error).contains("UNRELATED_ENV"));
    }

    #[test]
    fn env_keyed_values_allow_optional_env_vars() {
        let optional = optional_env_refs_for_provider_id("azure-openai-responses");
        assert!(
            !optional.is_empty(),
            "fixture provider must have optional env vars"
        );
        let primary = env_var_for_provider_id("azure-openai-responses")
            .expect("azure-openai-responses has a canonical env var");
        let mut values = BTreeMap::from([(primary.to_owned(), "az-key".to_owned())]);
        for companion in companion_env_refs_for_provider_id("azure-openai-responses") {
            values.insert(companion.to_owned(), "companion-value".to_owned());
        }
        values.insert(optional[0].to_owned(), "optional-value".to_owned());
        validate_env_keyed_credential_values("azure-openai-responses", &values, "test.values")
            .expect("optional env vars are allowed");
    }

    #[test]
    fn env_keyed_values_reject_empty_value() {
        let values = BTreeMap::from([("OPENAI_API_KEY".to_owned(), String::new())]);
        let error = validate_env_keyed_credential_values("openai", &values, "test.values")
            .expect_err("empty value must be rejected");
        assert!(invalid_param_reason(error).contains("must not be empty"));
    }
}
