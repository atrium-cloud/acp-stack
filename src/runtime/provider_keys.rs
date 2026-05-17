//! Reusable provider/API-key compatibility mapping.
//!
//! The mapping itself is embedded data, not Rust control flow. Runtime code only
//! parses, validates, and queries it.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::LazyLock;

use serde::Deserialize;

use crate::error::{Result, StackError};

const EMBEDDED_MAPPING: &str = include_str!("../../data/mapping.toml");

static PROVIDER_KEY_MAPPING: LazyLock<ProviderKeyMapping> = LazyLock::new(|| {
    ProviderKeyMapping::from_toml(EMBEDDED_MAPPING).expect("valid provider mapping")
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
    pub id: String,
    pub name: String,
    pub agents: Vec<String>,
    #[serde(default)]
    pub api_key_env_vars: BTreeMap<String, String>,
    #[serde(default)]
    pub companion_env_vars: Vec<String>,
    #[serde(default)]
    pub optional_env_vars: Vec<String>,
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

    pub fn api_keys(&self) -> &[ApiKeyProviderMapping] {
        &self.api_keys
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
    }

    fn provider_mapping(&self, provider_id: &str) -> Option<&ProviderEnvMapping> {
        self.providers
            .iter()
            .find(|mapping| mapping.id == provider_id)
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
            validate_token("providers.id", &mapping.id)?;
            validate_token(&format!("providers.{}.name", mapping.id), &mapping.name)?;
            if mapping.agents.is_empty() {
                return provider_mapping_error(format!(
                    "provider `{}` has no supported agents",
                    mapping.id
                ));
            }
            validate_tokens(format!("providers.{}.agents", mapping.id), &mapping.agents)?;
            for agent in &mapping.agents {
                if !matches!(agent.as_str(), "opencode" | "pi" | "cursor" | "goose") {
                    return provider_mapping_error(format!(
                        "provider `{}` references unsupported agent `{agent}`",
                        mapping.id
                    ));
                }
            }
            for (agent, env_var) in &mapping.api_key_env_vars {
                validate_token(&format!("providers.{}.api_key_env_vars", mapping.id), agent)?;
                validate_token(
                    &format!("providers.{}.api_key_env_vars.{agent}", mapping.id),
                    env_var,
                )?;
                if !matches!(agent.as_str(), "opencode" | "pi" | "cursor" | "goose") {
                    return provider_mapping_error(format!(
                        "provider `{}` references unsupported API-key agent `{agent}`",
                        mapping.id
                    ));
                }
                if !mapping.agents.iter().any(|supported| supported == agent) {
                    return provider_mapping_error(format!(
                        "provider `{}` has API-key env var for unsupported agent `{agent}`",
                        mapping.id
                    ));
                }
            }
            if !provider_overrides.insert(mapping.id.as_str()) {
                return provider_mapping_error(format!(
                    "duplicate provider env mapping `{}`",
                    mapping.id
                ));
            }
            validate_tokens(
                format!("providers.{}.companion_env_vars", mapping.id),
                &mapping.companion_env_vars,
            )?;
            validate_tokens(
                format!("providers.{}.optional_env_vars", mapping.id),
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
    ProviderKeyMapping::load_embedded()
        .provider_mapping(provider_id)
        .and_then(|provider| {
            if !provider.agents.iter().any(|id| id == agent_id) {
                return None;
            }
            provider
                .api_key_env_vars
                .get(agent_id)
                .map(String::as_str)
                .or_else(|| env_var_for_provider_id(provider_id))
        })
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

pub fn required_env_refs_for_provider_id(provider_id: &str, api_key_ref: &str) -> Vec<String> {
    let mut refs = vec![api_key_ref.to_owned()];
    refs.extend(
        companion_env_refs_for_provider_id(provider_id)
            .into_iter()
            .map(str::to_owned),
    );
    refs
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

pub fn provider_ids_for_env_refs<'a>(
    env_refs: impl IntoIterator<Item = &'a str>,
) -> BTreeSet<&'static str> {
    let mapping = ProviderKeyMapping::load_embedded();
    let mut provider_ids = BTreeSet::new();
    for env_ref in env_refs {
        if let Some(key_mapping) = mapping.mapping_for_env_var(env_ref) {
            provider_ids.extend(key_mapping.provider_ids.iter().map(String::as_str));
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
                .map(|provider| provider.id.as_str()),
        );
    }
    provider_ids
}

pub fn env_ref_allows_provider(env_var: &str, provider_id: &str) -> bool {
    mapping_for_env_var(env_var)
        .is_some_and(|mapping| mapping.provider_ids.iter().any(|id| id == provider_id))
        || ProviderKeyMapping::load_embedded()
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

fn provider_mapping_error<T>(reason: String) -> Result<T> {
    Err(StackError::RegistryLoad { reason })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_mapping_loads_and_validates() {
        let mapping = ProviderKeyMapping::from_toml(EMBEDDED_MAPPING).expect("mapping parses");

        assert!(!mapping.api_keys().is_empty());
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
            "UNKNOWN_KEY",
        ]);

        assert_eq!(
            providers.into_iter().collect::<Vec<_>>(),
            ["cloudflare-ai-gateway", "cloudflare-workers-ai", "openai"]
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
        assert!(!provider_id_supports_agent("fireworks", "opencode"));
        assert!(provider_id_supports_agent("fireworks-ai", "opencode"));
        assert!(!provider_id_supports_agent("fireworks-ai", "pi"));
        assert!(provider_id_supports_agent("openai", "pi"));
        assert!(provider_id_supports_agent("openai", "opencode"));
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
    }

    #[test]
    fn direct_agent_env_refs_are_data_driven() {
        assert_eq!(env_refs_for_agent_id("amp"), ["AMP_API_KEY"]);
        assert_eq!(env_refs_for_agent_id("cursor"), ["CURSOR_API_KEY"]);
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
id = "same"
name = "Same"
agents = ["pi"]
"#,
        )
        .expect_err("duplicate provider id fails");

        assert!(err.to_string().contains("duplicate provider id `same`"));
    }

    #[test]
    fn invalid_mapping_rejects_empty_values() {
        let err = ProviderKeyMapping::from_toml(
            r#"
[[api_keys]]
env_var = ""
provider_ids = ["openai"]

[[providers]]
id = "openai"
name = "OpenAI"
agents = ["pi"]
"#,
        )
        .expect_err("empty env var fails");

        assert!(err.to_string().contains("must not be empty"));
    }
}
