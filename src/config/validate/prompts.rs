//! Prompts (`[prompts]`) validation.

use crate::config::schema::PromptsConfig;
use crate::config::validate::primitives::validate_duration_field;
use crate::error::{Result, StackError};

pub(crate) fn validate_prompts(prompts: &PromptsConfig) -> Result<()> {
    let threshold = validate_duration_field("prompts.stale_threshold", &prompts.stale_threshold)?;
    if threshold.is_zero() {
        return Err(StackError::NonZeroRequired {
            field: "prompts.stale_threshold",
        });
    }
    let interval = validate_duration_field("prompts.sweep_interval", &prompts.sweep_interval)?;
    if interval.is_zero() {
        return Err(StackError::NonZeroRequired {
            field: "prompts.sweep_interval",
        });
    }
    Ok(())
}
