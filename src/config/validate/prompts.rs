//! Prompts (`[prompts]`) validation.

use crate::config::schema::PromptsConfig;
use crate::config::validate::primitives::parse_duration_string;
use crate::error::{Result, StackError};

pub(crate) fn validate_prompts(prompts: &PromptsConfig) -> Result<()> {
    let threshold = parse_duration_string(&prompts.stale_threshold).ok_or(
        StackError::InvalidDurationField {
            field: "prompts.stale_threshold",
        },
    )?;
    if threshold.is_zero() {
        return Err(StackError::NonZeroRequired {
            field: "prompts.stale_threshold",
        });
    }
    let interval =
        parse_duration_string(&prompts.sweep_interval).ok_or(StackError::InvalidDurationField {
            field: "prompts.sweep_interval",
        })?;
    if interval.is_zero() {
        return Err(StackError::NonZeroRequired {
            field: "prompts.sweep_interval",
        });
    }
    Ok(())
}
