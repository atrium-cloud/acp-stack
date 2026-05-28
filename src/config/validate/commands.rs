//! Commands (`[commands]`) validation.

use crate::config::schema::CommandsConfig;
use crate::config::validate::primitives::parse_duration_string;
use crate::error::{Result, StackError};

pub(crate) fn validate_commands(commands: &CommandsConfig) -> Result<()> {
    let timeout = parse_duration_string(&commands.default_timeout).ok_or(
        StackError::InvalidDurationField {
            field: "commands.default_timeout",
        },
    )?;
    if timeout.is_zero() {
        return Err(StackError::NonZeroRequired {
            field: "commands.default_timeout",
        });
    }
    parse_duration_string(&commands.cancel_grace).ok_or(StackError::InvalidDurationField {
        field: "commands.cancel_grace",
    })?;
    let progress_interval = parse_duration_string(&commands.progress_interval).ok_or(
        StackError::InvalidDurationField {
            field: "commands.progress_interval",
        },
    )?;
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
