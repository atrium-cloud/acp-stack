//! Reusable provider/API-key compatibility mapping.
//!
//! The mapping itself is embedded data, not Rust control flow. Runtime code only
//! parses, validates, and queries it.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::LazyLock;

use serde::Deserialize;

use crate::config::{AgentProviderConfig, Config};
use crate::error::{Result, StackError};
use crate::runtime::agent::claude_code_provider_profiles::{
    CLAUDE_CODE_AGENT_ID, profile_for_provider_id,
};

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

/// Apply one mapped provider to canonical Agent config. CLI `agent set` and
/// native-config import share this mutation so provider/env semantics cannot
/// drift between the two surfaces.
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
}
