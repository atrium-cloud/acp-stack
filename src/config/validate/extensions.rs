//! Validation for the `[extensions]` table. The config struct is flat across
//! extension types, so per-type field discipline is enforced here.

use std::collections::BTreeMap;
use std::path::Path;

use crate::config::Config;
use crate::config::schema::{
    ExtensionConfig, ExtensionType, MAX_WORKLOAD_ENV_COUNT, MAX_WORKLOAD_ENV_NAME_BYTES,
    MAX_WORKLOAD_ENV_VALUE_BYTES,
};
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
    // Only the unshare backend gives each spawn an isolated network namespace;
    // any other backend would imply an unenforced guarantee.
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
    // Mediated spawns can run without PATH, so a bare-name provider would
    // resolve for agent spawns but fail closed for mediated ones.
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
    validate_workload_env(name, &extension.workload_env)?;
    Ok(())
}

/// `workload_env` entries go straight into the workload's `execve` envp, so
/// anything a shell or libc treats specially must fail at load, not at spawn.
fn validate_workload_env(name: &str, workload_env: &BTreeMap<String, String>) -> Result<()> {
    if workload_env.len() > MAX_WORKLOAD_ENV_COUNT {
        return Err(StackError::InvalidParam {
            field: "extensions",
            reason: format!(
                "extension `{name}`: workload_env declares {} entries, exceeding the limit of \
                 {MAX_WORKLOAD_ENV_COUNT}",
                workload_env.len()
            ),
        });
    }
    for (env_name, value) in workload_env {
        if env_name.is_empty() || env_name.len() > MAX_WORKLOAD_ENV_NAME_BYTES {
            return Err(StackError::InvalidParam {
                field: "extensions",
                reason: format!(
                    "extension `{name}`: workload_env variable names must be non-empty and at most \
                     {MAX_WORKLOAD_ENV_NAME_BYTES} bytes"
                ),
            });
        }
        let valid_start = env_name
            .chars()
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic() || first == '_');
        let valid_body = env_name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !valid_start || !valid_body {
            return Err(StackError::InvalidParam {
                field: "extensions",
                reason: format!(
                    "extension `{name}`: workload_env variable `{env_name}` must match \
                     [A-Za-z_][A-Za-z0-9_]*"
                ),
            });
        }
        // PATH and HOME are runtime-managed at both spawn seams, which would
        // silently drop the declared value. Fail loudly at load instead.
        if matches!(env_name.as_str(), "PATH" | "HOME") {
            return Err(StackError::InvalidParam {
                field: "extensions",
                reason: format!(
                    "extension `{name}`: workload_env must not declare `{env_name}`; it is \
                     runtime-managed for every sandboxed spawn"
                ),
            });
        }
        if value.is_empty() || value.len() > MAX_WORKLOAD_ENV_VALUE_BYTES {
            return Err(StackError::InvalidParam {
                field: "extensions",
                reason: format!(
                    "extension `{name}`: workload_env value for `{env_name}` must be non-empty and \
                     at most {MAX_WORKLOAD_ENV_VALUE_BYTES} bytes"
                ),
            });
        }
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
        (!extension.workload_env.is_empty(), "workload_env"),
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
