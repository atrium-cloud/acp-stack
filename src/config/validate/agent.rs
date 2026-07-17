//! Agent-domain validators: provider, custom provider, adapter install,
//! restart policy, agent install escape hatch.

use crate::config::schema::{
    AgentAutoUpdateConfig, AgentCustomProviderConfig, AgentInstallConfig, AgentProviderConfig,
    AgentProvidersConfig, AgentSubagentConfig, CustomProviderApi,
};
use crate::config::validate::primitives::{
    require_present, validate_duration_field, validate_non_empty_trimmed, validate_nonempty,
    validate_secret_ref_name_value,
};
use crate::error::{Result, StackError};
use crate::runtime::agent::claude_code_provider_profiles::CLAUDE_CODE_AGENT_ID;
use crate::runtime::agent::provider_keys::{provider_id_is_known, provider_id_supports_agent};
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

pub(crate) fn validate_agent_auto_update(auto_update: &AgentAutoUpdateConfig) -> Result<()> {
    let frequency = validate_duration_field("agent.auto_update.frequency", &auto_update.frequency)?;
    if frequency.is_zero() {
        return Err(StackError::InvalidParam {
            field: "agent.auto_update.frequency",
            reason: "must be greater than zero".to_owned(),
        });
    }
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
    if agent_id != CLAUDE_CODE_AGENT_ID && api == CustomProviderApi::AnthropicMessages {
        return Err(StackError::InvalidParam {
            field,
            reason: "anthropic-messages custom providers only support Claude Code".to_owned(),
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
