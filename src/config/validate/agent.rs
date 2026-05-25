//! Agent-domain validators: provider, custom provider, adapter install,
//! restart policy, agent install escape hatch.

use crate::config::schema::{AgentCustomProviderConfig, AgentInstallConfig, AgentProviderConfig};
use crate::config::validate::primitives::{
    require_present, validate_non_empty_trimmed, validate_nonempty, validate_secret_ref_name_value,
};
use crate::error::{Result, StackError};

pub(crate) fn validate_agent_provider(provider: &AgentProviderConfig) -> Result<()> {
    if provider.id.trim().is_empty() || provider.id.len() != provider.id.trim().len() {
        return Err(StackError::MissingField {
            field: "agent.provider.id",
        });
    }
    if let Some(model) = provider.model.as_deref()
        && (model.trim().is_empty() || model.len() != model.trim().len())
    {
        return Err(StackError::MissingField {
            field: "agent.provider.model",
        });
    }
    if let Some(api_key_ref) = provider.api_key_ref.as_deref() {
        validate_secret_ref_name_value(api_key_ref)?;
    }
    if let Some(custom) = provider.custom.as_ref() {
        if provider.model.is_none() {
            return Err(StackError::MissingField {
                field: "agent.provider.model",
            });
        }
        if provider.api_key_ref.is_none() {
            return Err(StackError::MissingField {
                field: "agent.provider.api_key_ref",
            });
        }
        validate_agent_custom_provider(custom)?;
    }
    Ok(())
}

fn validate_agent_custom_provider(custom: &AgentCustomProviderConfig) -> Result<()> {
    validate_non_empty_trimmed("agent.provider.custom.name", &custom.name)?;
    validate_non_empty_trimmed("agent.provider.custom.base_url", &custom.base_url)?;
    if !custom.base_url.starts_with("http://") && !custom.base_url.starts_with("https://") {
        return Err(StackError::InvalidParam {
            field: "agent.provider.custom.base_url",
            reason: "must start with http:// or https://".to_owned(),
        });
    }
    if let Some(model_name) = custom.model_name.as_deref() {
        validate_non_empty_trimmed("agent.provider.custom.model_name", model_name)?;
    }
    if custom.context == 0 {
        return Err(StackError::InvalidParam {
            field: "agent.provider.custom.context",
            reason: "must be greater than 0".to_owned(),
        });
    }
    if custom.output_max_tokens == 0 {
        return Err(StackError::InvalidParam {
            field: "agent.provider.custom.output_max_tokens",
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
