//! Permissions validation.

use std::net::IpAddr;

use crate::config::schema::{
    ACP_PROMPT_ACTION_APPROVE, ACP_PROMPT_ACTION_ASK, PermissionsConfig, SecurityHttpConfig,
    TIMEOUT_ACTION_APPROVE, TIMEOUT_ACTION_DENY,
};
use crate::config::validate::primitives::validate_duration_field;
use crate::error::{Result, StackError};

pub(crate) fn validate_permissions(permissions: &PermissionsConfig) -> Result<()> {
    match permissions.mode.as_str() {
        "auto" | "supervised" | "locked" => {}
        _ => return Err(StackError::InvalidPermissionsMode),
    }
    if let Some(value) = permissions.request_timeout.as_deref() {
        let parsed = validate_duration_field("permissions.request_timeout", value)?;
        if parsed.is_zero() {
            return Err(StackError::NonZeroRequired {
                field: "permissions.request_timeout",
            });
        }
    }
    if let Some(action) = permissions.timeout_action.as_deref() {
        match action {
            TIMEOUT_ACTION_DENY | TIMEOUT_ACTION_APPROVE => {}
            _ => return Err(StackError::InvalidTimeoutAction),
        }
    }
    if let Some(action) = permissions.acp_prompt_action.as_deref() {
        match action {
            ACP_PROMPT_ACTION_ASK | ACP_PROMPT_ACTION_APPROVE => {}
            _ => return Err(StackError::InvalidAcpPromptAction),
        }
    }
    Ok(())
}

pub(crate) fn validate_trusted_proxies(http: &SecurityHttpConfig) -> Result<()> {
    for entry in &http.trusted_proxies {
        if entry.parse::<IpAddr>().is_err() {
            return Err(StackError::InvalidTrustedProxy {
                value: entry.clone(),
            });
        }
    }
    Ok(())
}
