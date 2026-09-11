//! Sessions (`[sessions]`) validation.

use crate::config::schema::SessionsConfig;
use crate::config::validate::primitives::validate_duration_field;
use crate::error::{Result, StackError};

pub(crate) fn validate_sessions(sessions: &SessionsConfig) -> Result<()> {
    let threshold = validate_duration_field("sessions.idle_threshold", &sessions.idle_threshold)?;
    if threshold.is_zero() {
        return Err(StackError::NonZeroRequired {
            field: "sessions.idle_threshold",
        });
    }
    Ok(())
}
