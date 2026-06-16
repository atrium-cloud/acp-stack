use axum::extract::State;
use serde::Serialize;

use super::super::core::AppState;
use crate::auth::{AuthVerifier, KeyKind, generate_api_key};
use crate::envelope::ApiSuccess;
use crate::error::StackError;

#[derive(Serialize)]
pub(crate) struct RegenerateSessionKeyResponse {
    pub(crate) session_key: String,
}

pub(crate) async fn auth_regenerate_session_key_handler(
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<RegenerateSessionKeyResponse>, StackError> {
    let new_key = generate_api_key();
    let verifier = AuthVerifier::create(KeyKind::Session, &new_key);
    let store = state.state.lock().await;
    store.upsert_auth_key(KeyKind::Session, &verifier)?;
    drop(store);

    let mut auth_verifiers = state.auth_verifiers.write().await;
    auth_verifiers.session = verifier;
    drop(auth_verifiers);

    Ok(ApiSuccess::new(RegenerateSessionKeyResponse {
        session_key: new_key,
    }))
}
