//! Validation for the `[extensions]` table.
//!
//! Extensions are typed, data-declared seams; the config struct is flat across
//! all types, so the per-type field discipline is enforced here: a field that
//! looks configured but would enforce nothing for the declared type must not
//! load. Cross-section coupling with the sandbox also lives here — a
//! `network-provider` instance switches every wrapped spawn to an isolated
//! network namespace, which only the `unshare` backend can provide.

use std::path::Path;

use crate::config::Config;
use crate::config::schema::{ExtensionConfig, ExtensionType};
use crate::error::{Result, StackError};

// CONSTANTS

/// Extension names become API path segments and log labels; keep them to a
/// conservative charset and length so they never need escaping anywhere.
const MAX_EXTENSION_NAME_BYTES: usize = 64;

/// `provider-credential` is the only managed-state capability today.
pub const MANAGED_STATE_CAPABILITY_PROVIDER_CREDENTIAL: &str = "provider-credential";

pub(crate) fn validate_extensions(config: &Config) -> Result<()> {
    let mut network_provider_names: Vec<&str> = Vec::new();
    for (name, extension) in &config.extensions {
        validate_extension_name(name)?;
        match extension.extension_type {
            ExtensionType::NetworkProvider => {
                network_provider_names.push(name.as_str());
                validate_network_provider_fields(name, extension)?;
            }
            ExtensionType::ManagedState => validate_managed_state_fields(name, extension)?,
        }
    }
    if network_provider_names.len() > 1 {
        return Err(StackError::InvalidParam {
            field: "extensions",
            reason: format!(
                "at most one network-provider extension may be declared; found {}: {}",
                network_provider_names.len(),
                network_provider_names.join(", ")
            ),
        });
    }
    // Cross-section coupling: an isolated network namespace per spawn exists
    // only for the unshare backend; any other backend carrying a
    // network-provider extension would imply an unenforced guarantee.
    if !network_provider_names.is_empty()
        && config.workspace.sandbox.mode != crate::config::SandboxMode::Unshare
    {
        return Err(StackError::InvalidParam {
            field: "extensions",
            reason: format!(
                "network-provider extension `{}` requires [workspace.sandbox] mode = \"unshare\"; \
                 remove the extension or change the sandbox mode first",
                network_provider_names[0]
            ),
        });
    }
    Ok(())
}

fn validate_extension_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(StackError::InvalidParam {
            field: "extensions",
            reason: "extension names must not be empty".to_owned(),
        });
    }
    if name.len() > MAX_EXTENSION_NAME_BYTES {
        return Err(StackError::InvalidParam {
            field: "extensions",
            reason: format!(
                "extension name `{name}` exceeds the {MAX_EXTENSION_NAME_BYTES}-byte limit"
            ),
        });
    }
    let valid_start = name
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit());
    let valid_body = name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !valid_start || !valid_body || name.ends_with('-') {
        return Err(StackError::InvalidParam {
            field: "extensions",
            reason: format!(
                "extension name `{name}` must be lowercase alphanumeric with interior hyphens \
                 (it is used as an API path segment)"
            ),
        });
    }
    Ok(())
}

fn validate_network_provider_fields(name: &str, extension: &ExtensionConfig) -> Result<()> {
    if extension.capability.is_some() {
        return Err(StackError::InvalidParam {
            field: "extensions",
            reason: format!(
                "extension `{name}`: `capability` is a managed-state field and does not apply to \
                 type = \"network-provider\""
            ),
        });
    }
    if extension
        .provider
        .iter()
        .any(|argument| argument.trim().is_empty())
    {
        return Err(StackError::InvalidParam {
            field: "extensions",
            reason: format!("extension `{name}`: provider argv entries must be non-empty"),
        });
    }
    // Mediated spawns can run without PATH in their environment, so a
    // bare-name provider would resolve for agent spawns but fail closed
    // for mediated ones. Require an absolute path for determinism.
    if let Some(provider) = extension.provider.first()
        && !Path::new(provider).is_absolute()
    {
        return Err(StackError::InvalidParam {
            field: "extensions",
            reason: format!(
                "extension `{name}`: provider executable `{provider}` must be an absolute path"
            ),
        });
    }
    let provider_timeout = super::primitives::validate_duration_field(
        "extensions.provider_timeout",
        extension
            .provider_timeout
            .as_deref()
            .unwrap_or(crate::config::schema::DEFAULT_NETWORK_PROVIDER_TIMEOUT),
    )?;
    // A zero deadline makes every provider run race an already-expired
    // timer, succeeding or SIGKILLed depending on scheduling.
    if provider_timeout.is_zero() {
        return Err(StackError::InvalidParam {
            field: "extensions",
            reason: format!("extension `{name}`: provider timeout must be greater than zero"),
        });
    }
    Ok(())
}

fn validate_managed_state_fields(name: &str, extension: &ExtensionConfig) -> Result<()> {
    for (field_configured, field_name) in [
        (!extension.provider.is_empty(), "provider"),
        (extension.provider_timeout.is_some(), "provider_timeout"),
        (
            extension.provider_stderr != crate::config::SandboxProviderStderr::default(),
            "provider_stderr",
        ),
    ] {
        if field_configured {
            return Err(StackError::InvalidParam {
                field: "extensions",
                reason: format!(
                    "extension `{name}`: `{field_name}` is a network-provider field and does not \
                     apply to type = \"managed-state\""
                ),
            });
        }
    }
    match extension.capability.as_deref() {
        Some(MANAGED_STATE_CAPABILITY_PROVIDER_CREDENTIAL) => Ok(()),
        Some(other) => Err(StackError::InvalidParam {
            field: "extensions",
            reason: format!(
                "extension `{name}`: unknown managed-state capability `{other}`; the only \
                 capability is \"{MANAGED_STATE_CAPABILITY_PROVIDER_CREDENTIAL}\""
            ),
        }),
        None => Err(StackError::InvalidParam {
            field: "extensions",
            reason: format!(
                "extension `{name}`: type = \"managed-state\" requires \
                 `capability = \"{MANAGED_STATE_CAPABILITY_PROVIDER_CREDENTIAL}\"`"
            ),
        }),
    }
}
