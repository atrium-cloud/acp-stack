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
    push_delivers_env_ref_for_config, reconcile_provider_env_declarations,
    resolve_agent_environment, resolve_agent_environment_without_secrets, target_uses_provider,
};

const EMBEDDED_ENV_VARS: &str = include_str!("../../../data/env_vars.toml");
const EMBEDDED_PROVIDERS: &str = include_str!("../../../data/providers.toml");
pub const CLAUDE_CODE_AGENT_ID: &str = "claude";
pub const CODEX_AGENT_ID: &str = "codex";
pub const HERMES_AGENT_ID: &str = "hermes";
pub const KILO_AGENT_ID: &str = "kilo";
pub const GOOSE_AGENT_ID: &str = "goose";
pub const KIMI_AGENT_ID: &str = "kimi";
pub const ANTIGRAVITY_AGENT_ID: &str = "antigravity";

/// Codex reserves `openai` for its own built-in provider definition, whose replacement table
/// shape is version-dependent.
pub const CODEX_OPENAI_PROVIDER_ID: &str = "openai";

/// `KIMI_MODEL_PROVIDER_TYPE` values; `kimi` is Kimi Code's own wire and is never exported.
pub const KIMI_PROVIDER_TYPES: [&str; 4] = ["kimi", "anthropic", "openai", "openai_responses"];
pub const KIMI_NATIVE_PROVIDER_TYPE: &str = "kimi";
pub const KIMI_MODEL_API_KEY_ENV: &str = "KIMI_MODEL_API_KEY";
pub const KIMI_MODEL_NAME_ENV: &str = "KIMI_MODEL_NAME";
pub const KIMI_MODEL_BASE_URL_ENV: &str = "KIMI_MODEL_BASE_URL";
pub const KIMI_MODEL_PROVIDER_TYPE_ENV: &str = "KIMI_MODEL_PROVIDER_TYPE";
pub const KIMI_MODEL_MAX_CONTEXT_SIZE_ENV: &str = "KIMI_MODEL_MAX_CONTEXT_SIZE";
pub const KIMI_MODEL_MAX_OUTPUT_SIZE_ENV: &str = "KIMI_MODEL_MAX_OUTPUT_SIZE";
pub const KIMI_MODEL_DISPLAY_NAME_ENV: &str = "KIMI_MODEL_DISPLAY_NAME";
/// Every launch env var acp-stack builds Kimi's headless model from; fact-check-data verifies
/// that Kimi still reads each one.
pub const KIMI_MODEL_ENV_VARS: [&str; 7] = [
    KIMI_MODEL_API_KEY_ENV,
    KIMI_MODEL_NAME_ENV,
    KIMI_MODEL_BASE_URL_ENV,
    KIMI_MODEL_PROVIDER_TYPE_ENV,
    KIMI_MODEL_MAX_CONTEXT_SIZE_ENV,
    KIMI_MODEL_MAX_OUTPUT_SIZE_ENV,
    KIMI_MODEL_DISPLAY_NAME_ENV,
];
/// Env-name fragments that mark an optional provider env ref as key material.
const CREDENTIAL_ENV_NAME_MARKERS: [&str; 4] = ["KEY", "SECRET", "TOKEN", "PASSWORD"];

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
    /// Native provider id to agent, for agents whose id differs from the primary id; an
    /// unmapped agent uses the primary id.
    #[serde(default)]
    pub provider_ids: BTreeMap<String, String>,
    #[serde(default)]
    pub companion_env_vars: Vec<String>,
    #[serde(default)]
    pub optional_env_vars: Vec<String>,
    /// OpenAI-compatible `GET /models` endpoint for live model-catalog fetches.
    #[serde(default)]
    pub models_url: Option<String>,
    /// The vendor endpoint base most agents send this provider's traffic to.
    #[serde(default)]
    pub base_url: Option<String>,
    /// Per-agent vendor bases: what Kimi's launch env exports where its wire takes another path,
    /// and what OpenCode is pointed at where its default host is not the one a credential seals.
    #[serde(default)]
    pub base_urls: BTreeMap<String, String>,
    /// The companion env var carrying the endpoint of a provider with no fixed vendor base (one
    /// Azure resource per operator); agents configured by file get an env reference to it.
    #[serde(default)]
    pub base_url_env: Option<String>,
    /// The env var Hermes reads this provider's endpoint from, which carries the row's `hermes`
    /// base: set where Hermes' default host is not the one a sealed credential redeems at.
    #[serde(default)]
    pub hermes_base_url_env: Option<String>,
    #[serde(default)]
    pub claude_code: Option<ClaudeCodeProviderProfile>,
    #[serde(default)]
    pub kimi: Option<KimiProviderProfile>,
    /// Endpoints of one vendor that share the row's key; each row id belongs to exactly one.
    #[serde(default)]
    pub endpoints: Vec<ProviderEndpoint>,
    /// Each endpoint as a row of its own, built at load so lookups can hand out `'static` borrows.
    #[serde(skip)]
    lanes: Vec<ProviderEnvMapping>,
}

/// One endpoint of a provider row; each field means what the row-level field means on a row
/// without endpoints.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ProviderEndpoint {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub models_url: Option<String>,
    #[serde(default)]
    pub provider_ids: BTreeMap<String, String>,
    #[serde(default)]
    pub api_key_env_vars: BTreeMap<String, String>,
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

/// Kimi Code launch metadata for one provider: the wire Kimi speaks to the row's base and the
/// model seeded when nothing configures one.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct KimiProviderProfile {
    pub provider_type: String,
    #[serde(default)]
    pub default_model: Option<String>,
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

    /// The rows lookups answer from: this row, or one per endpoint.
    pub fn lanes(&self) -> &[ProviderEnvMapping] {
        if self.endpoints.is_empty() {
            std::slice::from_ref(self)
        } else {
            &self.lanes
        }
    }

    fn lane(&self, provider_id: &str) -> Option<&ProviderEnvMapping> {
        self.lanes()
            .iter()
            .find(|lane| lane.contains_id(provider_id))
    }

    /// The endpoint id leads each lane's ids, followed by the row ids its `provider_ids` claim.
    fn endpoint_lanes(&self) -> Vec<ProviderEnvMapping> {
        self.endpoints
            .iter()
            .map(|endpoint| ProviderEnvMapping {
                id: std::iter::once(&endpoint.id)
                    .chain(self.id.iter().filter(|id| {
                        **id != endpoint.id && endpoint.provider_ids.contains_key(*id)
                    }))
                    .cloned()
                    .collect(),
                name: endpoint.name.clone(),
                base_url: endpoint.base_url.clone(),
                models_url: endpoint.models_url.clone(),
                provider_ids: endpoint.provider_ids.clone(),
                api_key_env_vars: endpoint.api_key_env_vars.clone(),
                endpoints: Vec::new(),
                ..self.clone()
            })
            .collect()
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

    fn vendor_base_url(&self, agent_id: &str) -> Option<&str> {
        if !self.agents.iter().any(|agent| agent == agent_id) {
            return None;
        }
        self.base_urls
            .get(agent_id)
            .or(self.base_url.as_ref())
            .map(String::as_str)
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
        Self::new(raw.api_keys, raw.providers)
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
        Self::new(env_vars.api_keys, providers.providers)
    }

    fn new(
        api_keys: Vec<ApiKeyProviderMapping>,
        mut providers: Vec<ProviderEnvMapping>,
    ) -> Result<Self> {
        for provider in &mut providers {
            provider.lanes = provider.endpoint_lanes();
        }
        let mapping = Self {
            api_keys,
            providers,
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

    /// The lane answering for `provider_id`: its endpoint, or its whole row.
    fn provider_mapping(&self, provider_id: &str) -> Option<&ProviderEnvMapping> {
        self.providers
            .iter()
            .find_map(|mapping| mapping.lane(provider_id))
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

        for row in &self.providers {
            Self::validate_endpoints(row)?;
        }
        let mut provider_overrides = HashSet::new();
        for mapping in self.providers.iter().flat_map(ProviderEnvMapping::lanes) {
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
            // A base may only template the companions this provider's credential stores.
            let placeholder_env_vars: HashSet<&str> = self
                .api_keys
                .iter()
                .filter(|api_key| {
                    api_key
                        .provider_ids
                        .iter()
                        .any(|id| mapping.contains_id(id))
                })
                .flat_map(|api_key| api_key.companion_env_vars.iter())
                .chain(mapping.companion_env_vars.iter())
                .map(String::as_str)
                .collect();
            if let Some(base_url) = mapping.base_url.as_deref() {
                validate_vendor_base_url(
                    &format!("providers.{primary_id}.base_url"),
                    base_url,
                    &placeholder_env_vars,
                )?;
            }
            if let Some(base_url_env) = mapping.base_url_env.as_deref() {
                validate_token(
                    &format!("providers.{primary_id}.base_url_env"),
                    base_url_env,
                )?;
                if mapping.base_url.is_some() || !mapping.base_urls.is_empty() {
                    return provider_mapping_error(format!(
                        "provider `{primary_id}` declares both a vendor base URL and base_url_env"
                    ));
                }
                // A companion is required alongside the key, so the referenced endpoint is set.
                if !placeholder_env_vars.contains(base_url_env) {
                    return provider_mapping_error(format!(
                        "provider `{primary_id}` base_url_env `{base_url_env}` is not one of its companion env vars"
                    ));
                }
            }
            for (agent, base_url) in &mapping.base_urls {
                if !mapping.agents.iter().any(|supported| supported == agent) {
                    return provider_mapping_error(format!(
                        "provider `{primary_id}` has a base URL for unsupported agent `{agent}`"
                    ));
                }
                validate_vendor_base_url(
                    &format!("providers.{primary_id}.base_urls.{agent}"),
                    base_url,
                    &placeholder_env_vars,
                )?;
            }
            if let Some(base_url_env) = mapping.hermes_base_url_env.as_deref() {
                validate_token(
                    &format!("providers.{primary_id}.hermes_base_url_env"),
                    base_url_env,
                )?;
                // The launch env exports the base verbatim, so it must be declared and literal.
                let literal_base = mapping
                    .base_urls
                    .get(HERMES_AGENT_ID)
                    .is_some_and(|base| base_url_template_placeholders(base).is_empty());
                if !literal_base {
                    return provider_mapping_error(format!(
                        "provider `{primary_id}` hermes_base_url_env `{base_url_env}` needs a literal `hermes` entry under base_urls"
                    ));
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
            if let Some(profile) = &mapping.kimi {
                Self::validate_kimi_profile(mapping, profile)?;
            } else if mapping.agents.iter().any(|agent| agent == KIMI_AGENT_ID) {
                return provider_mapping_error(format!(
                    "provider `{primary_id}` supports `{KIMI_AGENT_ID}` but declares no [providers.kimi] profile"
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

    /// A row with endpoints splits its ids among them; the per-lane checks then run on each.
    fn validate_endpoints(row: &ProviderEnvMapping) -> Result<()> {
        let Some(first) = row.endpoints.first() else {
            return Ok(());
        };
        for endpoint in &row.endpoints {
            if !row.contains_id(&endpoint.id) {
                return provider_mapping_error(format!(
                    "provider endpoint `{}` is not one of its row's ids",
                    endpoint.id
                ));
            }
        }
        let primary_id = row.primary_id();
        validate_tokens("providers.id".to_owned(), &row.id)?;
        validate_token(&format!("providers.{primary_id}.name"), &row.name)?;
        if row.models_url.is_some()
            || !row.provider_ids.is_empty()
            || !row.api_key_env_vars.is_empty()
        {
            return provider_mapping_error(format!(
                "provider `{primary_id}` sets models_url, provider_ids or api_key_env_vars beside endpoints; each endpoint declares its own"
            ));
        }
        if row.base_url != first.base_url {
            return provider_mapping_error(format!(
                "provider `{primary_id}` base_url must equal its first endpoint's"
            ));
        }
        for id in &row.id {
            let claims = row
                .endpoints
                .iter()
                .filter(|endpoint| endpoint.id == *id || endpoint.provider_ids.contains_key(id))
                .count();
            if claims != 1 {
                return provider_mapping_error(format!(
                    "provider `{primary_id}` id `{id}` is claimed by {claims} endpoints; each id belongs to exactly one"
                ));
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
                    "provider `{primary_id}` uses Claude Code native auth but declares a `{CLAUDE_CODE_AGENT_ID}` API-key env var"
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

    fn validate_kimi_profile(
        mapping: &ProviderEnvMapping,
        profile: &KimiProviderProfile,
    ) -> Result<()> {
        let primary_id = mapping.primary_id();
        if !mapping.agents.iter().any(|agent| agent == KIMI_AGENT_ID) {
            return provider_mapping_error(format!(
                "provider `{primary_id}` declares a Kimi profile but does not support `{KIMI_AGENT_ID}`"
            ));
        }
        let provider_type = profile.provider_type.as_str();
        validate_token(
            &format!("providers.{primary_id}.kimi.provider_type"),
            provider_type,
        )?;
        if !KIMI_PROVIDER_TYPES.contains(&provider_type) {
            return provider_mapping_error(format!(
                "provider `{primary_id}` kimi.provider_type must be one of {}, got `{provider_type}`",
                KIMI_PROVIDER_TYPES.join(", ")
            ));
        }
        if let Some(default_model) = profile.default_model.as_deref() {
            validate_token(
                &format!("providers.{primary_id}.kimi.default_model"),
                default_model,
            )?;
        }
        // The launch env exports the base verbatim, so a kimi row needs a literal one: a
        // missing base cannot launch, and a templated one would reach the wire unresolved.
        let Some(base) = mapping.vendor_base_url(KIMI_AGENT_ID) else {
            return provider_mapping_error(format!(
                "provider `{primary_id}` supports `{KIMI_AGENT_ID}` but declares no base_url"
            ));
        };
        if !base_url_template_placeholders(base).is_empty() {
            return provider_mapping_error(format!(
                "provider `{primary_id}` kimi base `{base}` must be literal; the launch env exports it verbatim"
            ));
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

pub fn kimi_profile_for_provider_id(provider_id: &str) -> Option<&'static KimiProviderProfile> {
    ProviderKeyMapping::load_embedded()
        .provider_mapping(provider_id)
        .and_then(|provider| provider.kimi.as_ref())
}

/// The primary id of the first lane `agent_id` runs whose vendor base equals `base_url`; a
/// trailing slash on either side does not distinguish two endpoints.
pub fn provider_id_for_agent_vendor_base_url(
    agent_id: &str,
    base_url: &str,
) -> Option<&'static str> {
    let wanted = base_url.trim().trim_end_matches('/');
    if wanted.is_empty() {
        return None;
    }
    ProviderKeyMapping::load_embedded()
        .providers
        .iter()
        .flat_map(ProviderEnvMapping::lanes)
        .find(|provider| {
            provider
                .vendor_base_url(agent_id)
                .is_some_and(|vendor| vendor.trim_end_matches('/') == wanted)
        })
        .map(|provider| provider.primary_id())
}

pub fn provider_uses_agent_native_auth(agent_id: &str, provider_id: &str) -> bool {
    agent_id == CLAUDE_CODE_AGENT_ID
        && claude_code_profile_for_provider_id(provider_id)
            .is_some_and(|profile| profile.agent_native_auth)
}

/// The vendor endpoint base `agent_id` sends `provider_id`'s traffic to by default.
pub fn vendor_base_url_for_agent_provider_id(
    agent_id: &str,
    provider_id: &str,
) -> Option<&'static str> {
    ProviderKeyMapping::load_embedded()
        .provider_mapping(provider_id)
        .and_then(|provider| provider.vendor_base_url(agent_id))
}

/// The base a row declares for `agent_id` alone, leaving out the row-wide default.
pub fn agent_base_url_for_provider_id(agent_id: &str, provider_id: &str) -> Option<&'static str> {
    ProviderKeyMapping::load_embedded()
        .provider_mapping(provider_id)
        .and_then(|provider| provider.base_urls.get(agent_id))
        .map(String::as_str)
}

/// The launch env var and base that point Hermes at `provider_id`'s sealed host, for a row whose
/// Hermes default host differs from it.
pub fn hermes_launch_base_url(provider_id: &str) -> Option<(&'static str, &'static str)> {
    let provider = ProviderKeyMapping::load_embedded().provider_mapping(provider_id)?;
    Some((
        provider.hermes_base_url_env.as_deref()?,
        provider.base_urls.get(HERMES_AGENT_ID)?.as_str(),
    ))
}

/// The companion env var holding `provider_id`'s endpoint when the provider has no fixed vendor
/// base.
pub fn base_url_env_for_provider_id(provider_id: &str) -> Option<&'static str> {
    ProviderKeyMapping::load_embedded()
        .provider_mapping(provider_id)
        .and_then(|provider| provider.base_url_env.as_deref())
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
    for provider in mapping.providers.iter().flat_map(ProviderEnvMapping::lanes) {
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

/// Env refs a provider's stored credential may carry that are not key material: every companion
/// (regions, project ids, hosts) and every optional ref whose name marks no credential. Optional
/// lists mix both kinds (Bedrock's carries `AWS_REGION` beside `AWS_SECRET_ACCESS_KEY`).
pub fn non_credential_env_refs_for_provider_id(provider_id: &str) -> Vec<&'static str> {
    let mut refs = companion_env_refs_for_provider_id(provider_id);
    refs.extend(companion_env_refs_for_agent_provider_id(
        CLAUDE_CODE_AGENT_ID,
        provider_id,
    ));
    refs.extend(
        optional_env_refs_for_provider_id(provider_id)
            .into_iter()
            .chain(optional_env_refs_for_agent_provider_id(
                CLAUDE_CODE_AGENT_ID,
                provider_id,
            ))
            .filter(|env_ref| {
                !CREDENTIAL_ENV_NAME_MARKERS
                    .iter()
                    .any(|marker| env_ref.contains(marker))
            }),
    );
    dedupe_refs(refs)
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
                .flat_map(ProviderEnvMapping::lanes)
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

const BASE_URL_PLACEHOLDER_FILL: &str = "placeholder";

fn validate_vendor_base_url(
    field: &str,
    value: &str,
    placeholder_env_vars: &HashSet<&str>,
) -> Result<()> {
    validate_token(field, value)?;
    let mut filled = value.to_owned();
    for placeholder in base_url_template_placeholders(value) {
        if !placeholder_env_vars.contains(placeholder) {
            return provider_mapping_error(format!(
                "`{field}` value `{value}` templates `{{{placeholder}}}`, which is not a companion \
                 env var of this provider"
            ));
        }
        filled = filled.replace(&format!("{{{placeholder}}}"), BASE_URL_PLACEHOLDER_FILL);
    }
    if filled.contains(['{', '}']) {
        return provider_mapping_error(format!(
            "`{field}` value `{value}` has an unbalanced placeholder brace"
        ));
    }
    let parsed = reqwest::Url::parse(&filled).map_err(|_| StackError::RegistryLoad {
        reason: format!("`{field}` value `{value}` is not a valid URL"),
    })?;
    if parsed.scheme() != "https" || parsed.host_str().is_none() {
        return provider_mapping_error(format!("`{field}` value `{value}` must be an https URL"));
    }
    Ok(())
}

/// The `{ENV_VAR}` placeholders a vendor base template carries, in order of appearance.
pub fn base_url_template_placeholders(template: &str) -> Vec<&str> {
    let mut placeholders = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else {
            break;
        };
        placeholders.push(&after[..end]);
        rest = &after[end + 1..];
    }
    placeholders
}

fn is_supported_agent_id(agent_id: &str) -> bool {
    matches!(
        agent_id,
        "amp"
            | "antigravity"
            | "claude"
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
