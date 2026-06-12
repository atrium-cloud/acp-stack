//! Commands (`[commands]`) validation.

use crate::config::schema::CommandsConfig;
use crate::config::validate::primitives::validate_duration_field;
use crate::error::{Result, StackError};

pub(crate) fn validate_commands(commands: &CommandsConfig) -> Result<()> {
    let timeout = validate_duration_field("commands.default_timeout", &commands.default_timeout)?;
    if timeout.is_zero() {
        return Err(StackError::NonZeroRequired {
            field: "commands.default_timeout",
        });
    }
    validate_duration_field("commands.cancel_grace", &commands.cancel_grace)?;
    let progress_interval =
        validate_duration_field("commands.progress_interval", &commands.progress_interval)?;
    if progress_interval.is_zero() {
        return Err(StackError::NonZeroRequired {
            field: "commands.progress_interval",
        });
    }
    if commands.max_output_bytes == 0 {
        return Err(StackError::NonZeroRequired {
            field: "commands.max_output_bytes",
        });
    }
    for name in &commands.env_allowlist {
        if name.trim().is_empty()
            || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            || name.chars().next().is_some_and(|c| c.is_ascii_digit())
        {
            return Err(StackError::InvalidEnvName { name: name.clone() });
        }
    }
    Ok(())
}
