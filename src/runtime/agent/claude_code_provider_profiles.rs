//! Claude Code-specific provider routing metadata.
//!
//! Provider compatibility lives in `data/providers.toml`; this module only
//! covers the extra env and endpoint shape Claude Code needs for headless use.

use std::collections::{BTreeMap, HashSet};
use std::sync::LazyLock;

use serde::Deserialize;

use crate::error::{Result, StackError};

pub const CLAUDE_CODE_AGENT_ID: &str = "claude-code";

const EMBEDDED_CLAUDE_CODE_PROVIDERS: &str =
    include_str!("../../../data/claude_code_providers.toml");

static CLAUDE_CODE_PROVIDER_PROFILES: LazyLock<ClaudeCodeProviderProfiles> = LazyLock::new(|| {
    ClaudeCodeProviderProfiles::from_toml(EMBEDDED_CLAUDE_CODE_PROVIDERS)
        .expect("valid Claude Code provider profiles")
});

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ClaudeCodeProviderProfile {
    pub provider_ids: Vec<String>,
    #[serde(default)]
    pub api_key_env_var: Option<String>,
    #[serde(default)]
    pub agent_native_auth: bool,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub default_model: Option<String>,
    #[serde(default)]
    pub set_subagent_model: bool,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub companion_env_vars: Vec<String>,
    #[serde(default)]
    pub optional_env_vars: Vec<String>,
}

impl ClaudeCodeProviderProfile {
    pub fn primary_provider_id(&self) -> &str {
        self.provider_ids
            .first()
            .expect("validated profile has at least one provider id")
    }

    pub fn contains_provider_id(&self, provider_id: &str) -> bool {
        self.provider_ids.iter().any(|id| id == provider_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeCodeProviderProfiles {
    profiles: Vec<ClaudeCodeProviderProfile>,
}

#[derive(Debug, Deserialize)]
struct RawClaudeCodeProviderProfiles {
    #[serde(default)]
    profiles: Vec<ClaudeCodeProviderProfile>,
}

impl ClaudeCodeProviderProfiles {
    pub fn load_embedded() -> &'static Self {
        &CLAUDE_CODE_PROVIDER_PROFILES
    }

    pub fn from_toml(body: &str) -> Result<Self> {
        let raw: RawClaudeCodeProviderProfiles =
            toml::from_str(body).map_err(|source| StackError::RegistryLoad {
                reason: format!("Claude Code provider profile TOML is invalid: {source}"),
            })?;
        let profiles = Self {
            profiles: raw.profiles,
        };
        profiles.validate()?;
        Ok(profiles)
    }

    pub fn profiles(&self) -> &[ClaudeCodeProviderProfile] {
        &self.profiles
    }

    pub fn profile_for_provider_id(&self, provider_id: &str) -> Option<&ClaudeCodeProviderProfile> {
        self.profiles
            .iter()
            .find(|profile| profile.contains_provider_id(provider_id))
    }

    fn validate(&self) -> Result<()> {
        let mut provider_ids = HashSet::new();
        for profile in &self.profiles {
            if profile.provider_ids.is_empty() {
                return claude_profile_error("profiles.provider_ids must not be empty".to_owned());
            }
            validate_tokens("profiles.provider_ids", &profile.provider_ids)?;
            for provider_id in &profile.provider_ids {
                if !provider_ids.insert(provider_id.as_str()) {
                    return claude_profile_error(format!(
                        "duplicate Claude Code provider profile `{provider_id}`"
                    ));
                }
            }
            if let Some(env_var) = profile.api_key_env_var.as_deref() {
                validate_token("profiles.api_key_env_var", env_var)?;
            }
            if profile.agent_native_auth && profile.api_key_env_var.is_some() {
                return claude_profile_error(format!(
                    "Claude Code provider `{}` cannot be both native-auth and API-key default",
                    profile.primary_provider_id()
                ));
            }
            if let Some(base_url) = profile.base_url.as_deref() {
                validate_url("profiles.base_url", base_url)?;
            }
            if let Some(default_model) = profile.default_model.as_deref() {
                validate_nonempty("profiles.default_model", default_model)?;
            }
            validate_env_map("profiles.env", &profile.env)?;
            validate_tokens("profiles.companion_env_vars", &profile.companion_env_vars)?;
            validate_tokens("profiles.optional_env_vars", &profile.optional_env_vars)?;
        }
        Ok(())
    }
}

pub fn profile_for_provider_id(provider_id: &str) -> Option<&'static ClaudeCodeProviderProfile> {
    ClaudeCodeProviderProfiles::load_embedded().profile_for_provider_id(provider_id)
}

pub fn is_claude_code_profiled_provider(provider_id: &str) -> bool {
    profile_for_provider_id(provider_id).is_some()
}

fn validate_env_map(field: &str, values: &BTreeMap<String, String>) -> Result<()> {
    for (key, value) in values {
        validate_token(&format!("{field}.key"), key)?;
        validate_nonempty(&format!("{field}.{key}"), value)?;
    }
    Ok(())
}

fn validate_tokens(field: &str, values: &[String]) -> Result<()> {
    let mut seen = HashSet::new();
    for value in values {
        validate_token(field, value)?;
        if !seen.insert(value.as_str()) {
            return claude_profile_error(format!("duplicate value `{value}` in `{field}`"));
        }
    }
    Ok(())
}

fn validate_token(field: &str, value: &str) -> Result<()> {
    validate_nonempty(field, value)?;
    if value.trim() != value {
        return claude_profile_error(format!(
            "`{field}` value `{value}` has surrounding whitespace"
        ));
    }
    Ok(())
}

fn validate_nonempty(field: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        return claude_profile_error(format!("`{field}` must not be empty"));
    }
    Ok(())
}

fn validate_url(field: &str, value: &str) -> Result<()> {
    validate_token(field, value)?;
    if !(value.starts_with("https://") || value.starts_with("http://")) {
        return claude_profile_error(format!("`{field}` must be an HTTP(S) URL"));
    }
    Ok(())
}

fn claude_profile_error(reason: String) -> Result<()> {
    Err(StackError::RegistryLoad { reason })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_claude_code_provider_profiles_parse() {
        let profiles = ClaudeCodeProviderProfiles::load_embedded();
        assert!(profiles.profile_for_provider_id("deepseek").is_some());
        assert!(
            profiles
                .profile_for_provider_id("xiaomi-token-plan-sgp")
                .is_some()
        );
    }

    #[test]
    fn embedded_profiles_keep_provider_ids_unique() {
        let profiles = ClaudeCodeProviderProfiles::load_embedded();
        let mut ids = HashSet::new();
        for profile in profiles.profiles() {
            for id in &profile.provider_ids {
                assert!(ids.insert(id.as_str()), "duplicate provider id `{id}`");
            }
        }
    }

    #[test]
    fn native_auth_profiles_do_not_declare_api_key_defaults() {
        let profiles = ClaudeCodeProviderProfiles::load_embedded();
        for provider_id in ["amazon-bedrock", "google-vertex-anthropic"] {
            let profile = profiles
                .profile_for_provider_id(provider_id)
                .unwrap_or_else(|| panic!("{provider_id} profile should exist"));
            assert!(profile.agent_native_auth);
            assert!(profile.api_key_env_var.is_none());
        }
    }
}
