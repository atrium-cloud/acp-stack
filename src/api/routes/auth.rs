use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};

use super::super::core::AppState;
use crate::auth::{AuthVerifier, KeyKind, generate_api_key};
use crate::config::LocalSessionAuth;
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

#[derive(Deserialize)]
pub(crate) struct LocalSessionAccessRequest {
    session_auth: LocalSessionAuth,
}

#[derive(Serialize)]
pub(crate) struct LocalSessionAccessResponse {
    session_auth: LocalSessionAuth,
}

pub(crate) async fn auth_local_session_access_handler(
    State(state): State<AppState>,
    Json(body): Json<LocalSessionAccessRequest>,
) -> std::result::Result<ApiSuccess<LocalSessionAccessResponse>, StackError> {
    persist_local_session_auth(&state, body.session_auth).await?;
    Ok(ApiSuccess::new(LocalSessionAccessResponse {
        session_auth: body.session_auth,
    }))
}

pub(crate) async fn persist_local_session_auth(
    state: &AppState,
    session_auth: LocalSessionAuth,
) -> std::result::Result<(), StackError> {
    let mut config = crate::config::Config::load_from_path(&state.runtime_paths.config_path)?;
    config.local.session_auth = session_auth;
    let canonical = config.to_canonical_toml()?;
    if let Some(parent) = state.runtime_paths.config_path.parent() {
        crate::fs_util::create_dir_owner_only(parent)?;
    }
    crate::fs_util::atomic_write_owner_only(
        &state.runtime_paths.config_path,
        canonical.as_bytes(),
    )?;
    state.set_local_session_auth(session_auth).await;
    Ok(())
}
