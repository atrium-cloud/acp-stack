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
    if let Some(value) = prompts.tool_call_stale_threshold.as_deref() {
        let tool_call_threshold =
            validate_duration_field("prompts.tool_call_stale_threshold", value)?;
        if tool_call_threshold.is_zero() {
            return Err(StackError::NonZeroRequired {
                field: "prompts.tool_call_stale_threshold",
            });
        }
        // An open tool call only ever widens the window; a shorter one would
        // stall a turn sooner for running a tool than for going quiet.
        if tool_call_threshold < threshold {
            return Err(StackError::InvalidParam {
                field: "prompts.tool_call_stale_threshold",
                reason: format!(
                    "must not be shorter than prompts.stale_threshold (`{}`)",
                    prompts.stale_threshold
                ),
            });
        }
    }
    let interval = validate_duration_field("prompts.sweep_interval", &prompts.sweep_interval)?;
    if interval.is_zero() {
        return Err(StackError::NonZeroRequired {
            field: "prompts.sweep_interval",
        });
    }
    Ok(())
}
