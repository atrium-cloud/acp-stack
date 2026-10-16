//! Validation for `[workspace.sandbox].workload_env` and `workload_path_prepend`, the
//! operator-declared environment every workload spawn starts with.

use std::path::Path;

use crate::config::Config;
use crate::config::schema::{
    ExtensionType, MAX_WORKLOAD_ENV_COUNT, MAX_WORKLOAD_ENV_NAME_BYTES,
    MAX_WORKLOAD_ENV_VALUE_BYTES, MAX_WORKLOAD_PATH_PREPEND_COUNT,
    MAX_WORKLOAD_PATH_PREPEND_ENTRY_BYTES,
};
use crate::config::secret_template::agent_env_declares;
use crate::error::{Result, StackError};
use crate::runtime::agent::acp_bridge::{KIMI_MANAGED_ENV_PREFIX, RUNTIME_MANAGED_LAUNCH_ENV};

// CONSTANTS

const WORKLOAD_ENV_FIELD: &str = "workspace.sandbox.workload_env";
const WORKLOAD_PATH_PREPEND_FIELD: &str = "workspace.sandbox.workload_path_prepend";
/// The runtime's own env namespace, which carries its sandbox protocol.
const RUNTIME_ENV_PREFIX: &str = "ACPS_";

pub(crate) fn validate_workload_env(config: &Config) -> Result<()> {
    let sandbox = &config.workspace.sandbox;
    if sandbox.workload_env.len() > MAX_WORKLOAD_ENV_COUNT {
        return Err(invalid_env(format!(
            "declares {} entries, exceeding the limit of {MAX_WORKLOAD_ENV_COUNT}",
            sandbox.workload_env.len()
        )));
    }
    for (name, value) in &sandbox.workload_env {
        validate_name(name)?;
        refuse_runtime_managed(config, name)?;
        if value.is_empty() || value.len() > MAX_WORKLOAD_ENV_VALUE_BYTES {
            return Err(invalid_env(format!(
                "value for `{name}` must be non-empty and at most {MAX_WORKLOAD_ENV_VALUE_BYTES} bytes"
            )));
        }
        if value.contains('\0') {
            return Err(invalid_env(format!(
                "value for `{name}` contains a NUL byte"
            )));
        }
    }
    validate_path_prepend(&sandbox.workload_path_prepend)
}

fn validate_name(name: &str) -> Result<()> {
    if name.len() > MAX_WORKLOAD_ENV_NAME_BYTES {
        return Err(invalid_env(format!(
            "variable names must be at most {MAX_WORKLOAD_ENV_NAME_BYTES} bytes"
        )));
    }
    if !super::primitives::is_valid_secret_ref_name(name) {
        return Err(invalid_env(format!(
            "variable `{name}` must match [A-Za-z_][A-Za-z0-9_]*"
        )));
    }
    Ok(())
}

/// A declared name the runtime also writes would be silently replaced at spawn, so every
/// overlap fails here instead.
fn refuse_runtime_managed(config: &Config, name: &str) -> Result<()> {
    if name == "PATH" {
        return Err(invalid_env(
            "must not declare `PATH`; list directories in workload_path_prepend".to_owned(),
        ));
    }
    if name == "HOME"
        || name.starts_with(RUNTIME_ENV_PREFIX)
        || name.starts_with(KIMI_MANAGED_ENV_PREFIX)
        || RUNTIME_MANAGED_LAUNCH_ENV.contains(&name)
    {
        return Err(invalid_env(format!(
            "must not declare `{name}`; the runtime sets it for workload spawns"
        )));
    }
    for target in &config.array.targets {
        if agent_env_declares(&target.agent.env, name) {
            return Err(invalid_env(format!(
                "must not declare `{name}`; [agent].env of target `{}` already supplies it",
                target.id
            )));
        }
    }
    for (extension_name, extension) in &config.extensions {
        if extension.extension_type == ExtensionType::NetworkProvider
            && extension.workload_env.contains_key(name)
        {
            return Err(invalid_env(format!(
                "must not declare `{name}`; network-provider extension `{extension_name}` \
                 workload_env already supplies it"
            )));
        }
    }
    Ok(())
}

fn validate_path_prepend(entries: &[String]) -> Result<()> {
    if entries.len() > MAX_WORKLOAD_PATH_PREPEND_COUNT {
        return Err(invalid_path(format!(
            "declares {} entries, exceeding the limit of {MAX_WORKLOAD_PATH_PREPEND_COUNT}",
            entries.len()
        )));
    }
    for entry in entries {
        if entry.contains('\0') {
            return Err(invalid_path(
                "entries must not contain a NUL byte".to_owned(),
            ));
        }
        if entry.contains(':') {
            return Err(invalid_path(format!(
                "entry `{entry}` must not contain `:`, the PATH separator"
            )));
        }
        if entry.len() > MAX_WORKLOAD_PATH_PREPEND_ENTRY_BYTES {
            return Err(invalid_path(format!(
                "entries must be at most {MAX_WORKLOAD_PATH_PREPEND_ENTRY_BYTES} bytes"
            )));
        }
        if !Path::new(entry).is_absolute() {
            return Err(invalid_path(format!(
                "entry `{entry}` must be an absolute path"
            )));
        }
    }
    Ok(())
}

fn invalid_env(reason: String) -> StackError {
    StackError::InvalidParam {
        field: WORKLOAD_ENV_FIELD,
        reason,
    }
}

fn invalid_path(reason: String) -> StackError {
    StackError::InvalidParam {
        field: WORKLOAD_PATH_PREPEND_FIELD,
        reason,
    }
}
