//! Agent-domain validators: provider, custom provider, adapter install,
//! restart policy, agent install escape hatch.

use crate::config::schema::{
    AgentAutoUpdateConfig, AgentConfigOptionValue, AgentCustomProviderConfig, AgentInstallConfig,
    AgentProviderConfig, AgentProvidersConfig, AgentSubagentConfig, CustomProviderApi,
};
use crate::config::validate::primitives::{
    DurationLimits, DurationUnit, normalize_duration, require_present, validate_non_empty_trimmed,
    validate_nonempty, validate_secret_ref_name_value,
};
use crate::error::{Result, StackError};
use crate::runtime::agent::provider_keys::{
    CLAUDE_CODE_AGENT_ID, provider_id_is_known, provider_id_supports_agent,
};
use crate::runtime::install::agent_registry::RegistryCatalog;

use std::collections::HashSet;

pub(crate) fn validate_agent_provider(
    agent_id: &str,
    provider: &AgentProviderConfig,
) -> Result<()> {
    validate_agent_provider_at(agent_id, provider, AGENT_PROVIDER_FIELDS)
}

pub(crate) fn validate_agent_subagent(
    agent_id: &str,
    subagent: &AgentSubagentConfig,
) -> Result<()> {
    if subagent.disabled && subagent.provider.is_some() {
        return Err(StackError::InvalidParam {
            field: "agent.subagent.provider",
            reason: "must be omitted when agent.subagent.disabled is true".to_owned(),
        });
    }
    if let Some(provider) = subagent.provider.as_ref() {
        validate_agent_provider_at(agent_id, provider, AGENT_SUBAGENT_PROVIDER_FIELDS)?;
    }
    Ok(())
}

pub(crate) fn validate_agent_providers(
    agent_id: &str,
    default_provider: Option<&AgentProviderConfig>,
    subagent: Option<&AgentSubagentConfig>,
    providers: &AgentProvidersConfig,
) -> Result<()> {
    if providers.active.is_empty() {
        return Err(StackError::InvalidParam {
            field: "agent.providers.active",
            reason: "must contain at least one mapped provider".to_owned(),
        });
    }
    let mut active = HashSet::new();
    for provider_id in &providers.active {
        validate_mapped_provider_id(agent_id, provider_id, "agent.providers.active")?;
        if !active.insert(provider_id.as_str()) {
            return Err(StackError::InvalidParam {
                field: "agent.providers.active",
                reason: format!("duplicate provider `{provider_id}`"),
            });
        }
    }

    let default_provider = default_provider.ok_or(StackError::MissingField {
        field: "agent.provider",
    })?;
    if default_provider.custom.is_some() {
        return Err(StackError::InvalidParam {
            field: "agent.providers.active",
            reason: "custom providers do not participate in active provider sets".to_owned(),
        });
    }
    if !active.contains(default_provider.id.as_str()) {
        return Err(StackError::InvalidParam {
            field: "agent.providers.active",
            reason: format!("must include default provider `{}`", default_provider.id),
        });
    }
    if let Some(subagent_provider) = subagent
        .filter(|subagent| !subagent.disabled)
        .and_then(|subagent| subagent.provider.as_ref())
    {
        if subagent_provider.custom.is_some() {
            return Err(StackError::InvalidParam {
                field: "agent.providers.active",
                reason: "custom subagent providers do not participate in active provider sets"
                    .to_owned(),
            });
        }
        if !active.contains(subagent_provider.id.as_str()) {
            return Err(StackError::InvalidParam {
                field: "agent.providers.active",
                reason: format!(
                    "must include configured subagent provider `{}`",
                    subagent_provider.id
                ),
            });
        }
    }

    if providers.active.len() > 1 {
        let registry = RegistryCatalog::load_embedded()?;
        let entry = registry.lookup_required(agent_id)?;
        if !entry.multiple_active_providers {
            return Err(StackError::InvalidParam {
                field: "agent.providers.active",
                reason: format!("agent `{agent_id}` does not support multiple active providers"),
            });
        }
    }

    for (provider_id, alias) in &providers.selected_aliases {
        validate_mapped_provider_id(agent_id, provider_id, "agent.providers.selected_aliases")?;
        validate_secret_ref_name_value(alias)?;
    }
    Ok(())
}

/// Bounds for `[agent.config_options]`. Keys follow ACP config-option id
/// shape: a leading `_` is explicitly legal (ACP reserves `_`-prefixed ids
/// for implementation-specific options). Values carry no charset restriction
/// because advertised select ids legitimately contain `/`, `.`, and brackets.
const MAX_AGENT_CONFIG_OPTIONS: usize = 32;
const MAX_AGENT_CONFIG_OPTION_KEY_BYTES: usize = 128;
const MAX_AGENT_CONFIG_OPTION_VALUE_BYTES: usize = 512;

pub(crate) fn validate_agent_config_options(
    config_options: &std::collections::BTreeMap<String, AgentConfigOptionValue>,
) -> Result<()> {
    if config_options.len() > MAX_AGENT_CONFIG_OPTIONS {
        return Err(StackError::InvalidParam {
            field: "agent.config_options",
            reason: format!(
                "at most {MAX_AGENT_CONFIG_OPTIONS} entries are supported (found {})",
                config_options.len()
            ),
        });
    }
    for (key, value) in config_options {
        if key.is_empty() || key.len() > MAX_AGENT_CONFIG_OPTION_KEY_BYTES {
            return Err(StackError::InvalidParam {
                field: "agent.config_options",
                reason: format!(
                    "option id `{key}` must be 1..={MAX_AGENT_CONFIG_OPTION_KEY_BYTES} bytes"
                ),
            });
        }
        if !key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
        {
            return Err(StackError::InvalidParam {
                field: "agent.config_options",
                reason: format!(
                    "option id `{key}` may only contain ASCII alphanumerics, `_`, `.`, and `-`"
                ),
            });
        }
        // The typed lanes own their ids (and aliases) so a map entry cannot
        // silently fight `agent.mode`/`agent.model`/`agent.effort`.
        for category in [
            crate::runtime::agent::acp_bridge::AgentSessionConfigCategory::Mode,
            crate::runtime::agent::acp_bridge::AgentSessionConfigCategory::Model,
            crate::runtime::agent::acp_bridge::AgentSessionConfigCategory::Effort,
        ] {
            if category.matches_id(key) {
                return Err(StackError::InvalidParam {
                    field: "agent.config_options",
                    reason: format!(
                        "option id `{key}` belongs to the typed `agent.{}` setting; configure that instead",
                        category.id()
                    ),
                });
            }
        }
        if let AgentConfigOptionValue::Text(text) = value {
            if text.trim().is_empty() || text.len() != text.trim().len() {
                return Err(StackError::InvalidParam {
                    field: "agent.config_options",
                    reason: format!("option `{key}` has a blank or untrimmed value"),
                });
            }
            if text.len() > MAX_AGENT_CONFIG_OPTION_VALUE_BYTES {
                return Err(StackError::InvalidParam {
                    field: "agent.config_options",
                    reason: format!(
                        "option `{key}` value exceeds {MAX_AGENT_CONFIG_OPTION_VALUE_BYTES} bytes"
                    ),
                });
            }
        }
    }
    Ok(())
}

fn validate_mapped_provider_id(
    agent_id: &str,
    provider_id: &str,
    field: &'static str,
) -> Result<()> {
    if !provider_id_is_known(provider_id) {
        return Err(StackError::InvalidParam {
            field,
            reason: format!("provider `{provider_id}` is not listed in provider/env mapping"),
        });
    }
    if !provider_id_supports_agent(provider_id, agent_id) {
        return Err(StackError::InvalidParam {
            field,
            reason: format!("provider `{provider_id}` is not supported for agent `{agent_id}`"),
        });
    }
    Ok(())
}

/// Managed agent updates poll upstream package registries; an hour is the
/// finest cadence worth allowing, so eager operators can still run e.g. `12h`.
/// Shared with init's `--agent-update-frequency` handling.
pub(crate) const AGENT_UPDATE_FREQUENCY_LIMITS: DurationLimits = DurationLimits::new(
    &[DurationUnit::Hour, DurationUnit::Day, DurationUnit::Week],
    std::time::Duration::from_secs(3_600),
);

pub(crate) fn validate_agent_auto_update(auto_update: &AgentAutoUpdateConfig) -> Result<()> {
    normalize_duration(
        "agent.auto_update.frequency",
        &auto_update.frequency,
        &AGENT_UPDATE_FREQUENCY_LIMITS,
    )?;
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct AgentProviderFieldNames {
    id: &'static str,
    model: &'static str,
    api_key_ref: &'static str,
    custom_name: &'static str,
    custom_base_url: &'static str,
    custom_api: &'static str,
    custom_model_name: &'static str,
    custom_context: &'static str,
    custom_output_max_tokens: &'static str,
}

const AGENT_PROVIDER_FIELDS: AgentProviderFieldNames = AgentProviderFieldNames {
    id: "agent.provider.id",
    model: "agent.provider.model",
    api_key_ref: "agent.provider.api_key_ref",
    custom_name: "agent.provider.custom.name",
    custom_base_url: "agent.provider.custom.base_url",
    custom_api: "agent.provider.custom.api",
    custom_model_name: "agent.provider.custom.model_name",
    custom_context: "agent.provider.custom.context",
    custom_output_max_tokens: "agent.provider.custom.output_max_tokens",
};

const AGENT_SUBAGENT_PROVIDER_FIELDS: AgentProviderFieldNames = AgentProviderFieldNames {
    id: "agent.subagent.provider.id",
    model: "agent.subagent.provider.model",
    api_key_ref: "agent.subagent.provider.api_key_ref",
    custom_name: "agent.subagent.provider.custom.name",
    custom_base_url: "agent.subagent.provider.custom.base_url",
    custom_api: "agent.subagent.provider.custom.api",
    custom_model_name: "agent.subagent.provider.custom.model_name",
    custom_context: "agent.subagent.provider.custom.context",
    custom_output_max_tokens: "agent.subagent.provider.custom.output_max_tokens",
};

fn validate_agent_provider_at(
    agent_id: &str,
    provider: &AgentProviderConfig,
    fields: AgentProviderFieldNames,
) -> Result<()> {
    if provider.id.trim().is_empty() || provider.id.len() != provider.id.trim().len() {
        return Err(StackError::MissingField { field: fields.id });
    }
    if let Some(model) = provider.model.as_deref()
        && (model.trim().is_empty() || model.len() != model.trim().len())
    {
        return Err(StackError::MissingField {
            field: fields.model,
        });
    }
    if let Some(api_key_ref) = provider.api_key_ref.as_deref() {
        validate_secret_ref_name_value(api_key_ref)?;
    }
    if let Some(custom) = provider.custom.as_ref() {
        // Every runtime and apply site classifies a provider id by registry
        // membership before it looks at `custom`, so a custom declaration
        // reusing a registry id resolves down the mapped path and hard-fails at
        // spawn. Reserve registry ids globally, including ids the registry knows
        // but does not map for this harness.
        if provider_id_is_known(&provider.id) {
            return Err(StackError::InvalidParam {
                field: fields.id,
                reason: format!(
                    "`{id}` is reserved by the mapped-provider registry; choose a distinct custom id such as `{id}-1`",
                    id = provider.id
                ),
            });
        }
        if provider.model.is_none() {
            return Err(StackError::MissingField {
                field: fields.model,
            });
        }
        if provider.api_key_ref.is_none() {
            return Err(StackError::MissingField {
                field: fields.api_key_ref,
            });
        }
        validate_agent_custom_provider(agent_id, custom, fields)?;
    }
    Ok(())
}

fn validate_agent_custom_provider(
    agent_id: &str,
    custom: &AgentCustomProviderConfig,
    fields: AgentProviderFieldNames,
) -> Result<()> {
    validate_non_empty_trimmed(fields.custom_name, &custom.name)?;
    validate_non_empty_trimmed(fields.custom_base_url, &custom.base_url)?;
    if !custom.base_url.starts_with("http://") && !custom.base_url.starts_with("https://") {
        return Err(StackError::InvalidParam {
            field: fields.custom_base_url,
            reason: "must start with http:// or https://".to_owned(),
        });
    }
    validate_agent_custom_provider_api(agent_id, custom.api, fields.custom_api)?;
    if let Some(model_name) = custom.model_name.as_deref() {
        validate_non_empty_trimmed(fields.custom_model_name, model_name)?;
    }
    if custom.context == 0 {
        return Err(StackError::InvalidParam {
            field: fields.custom_context,
            reason: "must be greater than 0".to_owned(),
        });
    }
    if custom.output_max_tokens == 0 {
        return Err(StackError::InvalidParam {
            field: fields.custom_output_max_tokens,
            reason: "must be greater than 0".to_owned(),
        });
    }
    Ok(())
}

fn validate_agent_custom_provider_api(
    agent_id: &str,
    api: CustomProviderApi,
    field: &'static str,
) -> Result<()> {
    if agent_id == "codex" && api != CustomProviderApi::Responses {
        return Err(StackError::InvalidParam {
            field,
            reason: "Codex custom providers only support responses".to_owned(),
        });
    }
    if agent_id == CLAUDE_CODE_AGENT_ID && api != CustomProviderApi::AnthropicMessages {
        return Err(StackError::InvalidParam {
            field,
            reason: "Claude Code custom providers only support anthropic-messages".to_owned(),
        });
    }
    if agent_id != CLAUDE_CODE_AGENT_ID
        && agent_id != crate::runtime::agent::acp_bridge::KIMI_CODE_AGENT_ID
        && api == CustomProviderApi::AnthropicMessages
    {
        return Err(StackError::InvalidParam {
            field,
            reason: "anthropic-messages custom providers only support Claude Code and Kimi Code"
                .to_owned(),
        });
    }
    Ok(())
}

pub(crate) fn validate_agent_restart(value: &str) -> Result<()> {
    match value {
        "never" | "on-crash" => Ok(()),
        _ => Err(StackError::InvalidAgentRestart),
    }
}

pub(crate) fn validate_agent_install(install: &AgentInstallConfig) -> Result<()> {
    validate_nonempty("agent.install.creates", &install.creates)?;
    match install.install_type.as_str() {
        "shell" => {
            require_present("agent.install.shell", install.shell.as_deref())?;
            Ok(())
        }
        _ => Err(StackError::InvalidAgentInstallType),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::{AgentCustomProviderConfig, AgentSubagentConfig};
    use std::collections::BTreeMap;

    fn mapped_provider(provider_id: &str) -> AgentProviderConfig {
        AgentProviderConfig {
            id: provider_id.to_owned(),
            model: None,
            api_key_ref: None,
            custom: None,
        }
    }

    #[test]
    fn multiple_active_providers_are_limited_to_capable_harnesses() {
        let providers = AgentProvidersConfig {
            active: vec!["anthropic".to_owned(), "openrouter".to_owned()],
            selected_aliases: BTreeMap::new(),
        };

        validate_agent_providers("pi", Some(&mapped_provider("anthropic")), None, &providers)
            .expect("Pi supports multiple providers");
        let error = validate_agent_providers(
            "goose",
            Some(&mapped_provider("anthropic")),
            None,
            &providers,
        )
        .expect_err("Goose rejects multiple providers");
        assert!(error.to_string().contains("does not support multiple"));
    }

    #[test]
    fn active_providers_require_default_and_enabled_subagent() {
        let default = mapped_provider("opencode-go");
        let subagent = AgentSubagentConfig {
            disabled: false,
            provider: Some(mapped_provider("openrouter")),
        };
        let missing_default = AgentProvidersConfig {
            active: vec!["openrouter".to_owned()],
            selected_aliases: BTreeMap::new(),
        };
        let error = validate_agent_providers(
            "opencode",
            Some(&default),
            Some(&subagent),
            &missing_default,
        )
        .expect_err("default required");
        assert!(error.to_string().contains("default provider `opencode-go`"));

        let missing_subagent = AgentProvidersConfig {
            active: vec!["opencode-go".to_owned()],
            selected_aliases: BTreeMap::new(),
        };
        let error = validate_agent_providers(
            "opencode",
            Some(&default),
            Some(&subagent),
            &missing_subagent,
        )
        .expect_err("subagent required");
        assert!(error.to_string().contains("subagent provider `openrouter`"));
    }

    #[test]
    fn active_provider_sets_reject_duplicates_and_custom_defaults() {
        let duplicate = AgentProvidersConfig {
            active: vec!["openrouter".to_owned(), "openrouter".to_owned()],
            selected_aliases: BTreeMap::new(),
        };
        let error = validate_agent_providers(
            "opencode",
            Some(&mapped_provider("openrouter")),
            None,
            &duplicate,
        )
        .expect_err("duplicate rejected");
        assert!(
            error
                .to_string()
                .contains("duplicate provider `openrouter`")
        );

        let custom = AgentProviderConfig {
            id: "custom".to_owned(),
            model: None,
            api_key_ref: Some("CUSTOM_API_KEY".to_owned()),
            custom: Some(AgentCustomProviderConfig {
                name: "Custom".to_owned(),
                base_url: "https://example.com/v1".to_owned(),
                api: CustomProviderApi::ChatCompletions,
                model_name: None,
                context: 1,
                output_max_tokens: 1,
            }),
        };
        let providers = AgentProvidersConfig {
            active: vec!["openrouter".to_owned()],
            selected_aliases: BTreeMap::new(),
        };
        let error = validate_agent_providers("opencode", Some(&custom), None, &providers)
            .expect_err("custom default rejected");
        assert!(error.to_string().contains("custom providers"));
    }

    #[test]
    fn custom_providers_reject_registry_known_ids() {
        let custom = |provider_id: &str| AgentProviderConfig {
            id: provider_id.to_owned(),
            model: Some("some-model".to_owned()),
            api_key_ref: Some("CUSTOM_API_KEY".to_owned()),
            custom: Some(AgentCustomProviderConfig {
                name: "Custom".to_owned(),
                base_url: "https://example.com/v1".to_owned(),
                api: CustomProviderApi::ChatCompletions,
                model_name: None,
                context: 128_000,
                output_max_tokens: 8_192,
            }),
        };

        let error = validate_agent_provider("opencode", &custom("anthropic"))
            .expect_err("registry-known id rejected");
        let message = error.to_string();
        assert!(message.contains("reserved by the mapped-provider registry"));
        assert!(message.contains("anthropic-1"));

        validate_agent_provider("opencode", &custom("anthropic-1")).expect("distinct id accepted");

        let subagent = AgentSubagentConfig {
            disabled: false,
            provider: Some(custom("anthropic")),
        };
        let error =
            validate_agent_subagent("opencode", &subagent).expect_err("subagent id also reserved");
        assert!(
            error
                .to_string()
                .contains("reserved by the mapped-provider registry")
        );
    }

    #[test]
    fn selected_aliases_are_case_sensitive_identifiers() {
        let providers = AgentProvidersConfig {
            active: vec!["opencode-go".to_owned()],
            selected_aliases: BTreeMap::from([("opencode-go".to_owned(), "go_2".to_owned())]),
        };
        validate_agent_providers(
            "opencode",
            Some(&mapped_provider("opencode-go")),
            None,
            &providers,
        )
        .expect("valid alias");

        let mut invalid = providers;
        invalid
            .selected_aliases
            .insert("opencode-go".to_owned(), "go two".to_owned());
        assert!(
            validate_agent_providers(
                "opencode",
                Some(&mapped_provider("opencode-go")),
                None,
                &invalid,
            )
            .is_err()
        );
    }
}
