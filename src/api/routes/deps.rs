use axum::extract::State;

use super::super::core::AppState;
use crate::envelope::ApiSuccess;
use crate::error::StackError;

pub(crate) async fn deps_get_handler(
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<crate::deps::DepsReport>, StackError> {
    Ok(ApiSuccess::new(crate::deps::check_dependencies(
        &state.config,
    )))
}

pub(crate) async fn deps_check_handler(
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<crate::deps::DepsReport>, StackError> {
    Ok(ApiSuccess::new(crate::deps::check_dependencies(
        &state.config,
    )))
}
