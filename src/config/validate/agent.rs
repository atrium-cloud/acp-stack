//! Agent-domain validators: provider, custom provider, adapter install,
//! restart policy, agent install escape hatch.

use crate::config::schema::{
    AgentAutoUpdateConfig, AgentCustomProviderConfig, AgentInstallConfig, AgentProviderConfig,
    AgentSubagentConfig,
};
use crate::config::validate::primitives::{
    require_present, validate_duration_field, validate_non_empty_trimmed, validate_nonempty,
    validate_secret_ref_name_value,
};
use crate::error::{Result, StackError};

pub(crate) fn validate_agent_provider(provider: &AgentProviderConfig) -> Result<()> {
    validate_agent_provider_at(provider, AGENT_PROVIDER_FIELDS)
}

pub(crate) fn validate_agent_subagent(subagent: &AgentSubagentConfig) -> Result<()> {
    if subagent.disabled && subagent.provider.is_some() {
        return Err(StackError::InvalidParam {
            field: "agent.subagent.provider",
            reason: "must be omitted when agent.subagent.disabled is true".to_owned(),
        });
    }
    if let Some(provider) = subagent.provider.as_ref() {
        validate_agent_provider_at(provider, AGENT_SUBAGENT_PROVIDER_FIELDS)?;
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
    custom_model_name: "agent.subagent.provider.custom.model_name",
    custom_context: "agent.subagent.provider.custom.context",
    custom_output_max_tokens: "agent.subagent.provider.custom.output_max_tokens",
};

fn validate_agent_provider_at(
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
        validate_agent_custom_provider(custom, fields)?;
    }
    Ok(())
}

fn validate_agent_custom_provider(
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
