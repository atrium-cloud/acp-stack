use axum::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};

use super::super::core::AppState;
use crate::envelope::ApiSuccess;
use crate::error::StackError;
use crate::fs_util::home_dir;

#[derive(Serialize)]
pub(crate) struct ConfigExportResponse {
    toml: String,
}

pub(crate) async fn config_export_handler(
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<ConfigExportResponse>, StackError> {
    let toml = state.config.to_canonical_toml()?;
    Ok(ApiSuccess::new(ConfigExportResponse { toml }))
}

#[derive(Serialize)]
pub(crate) struct ConfigValidateResponse {
    valid: bool,
}

/// POST /v1/config/validate accepts the canonical TOML in the raw request
/// body (any content type). Returning `{valid:true}` on parse + validate
/// success matches the read-only contract; on failure the standard envelope
/// surfaces the underlying `config.invalid` (or related) code.
pub(crate) async fn config_validate_handler(
    body: String,
) -> std::result::Result<ApiSuccess<ConfigValidateResponse>, StackError> {
    crate::config::load_config_from_str(&body)?;
    Ok(ApiSuccess::new(ConfigValidateResponse { valid: true }))
}

#[derive(Serialize)]
pub(crate) struct ConfigImportResponse {
    imported: bool,
    restart_required: bool,
}

/// POST /v1/config/import (admin-tier). Parses TOML from the raw body,
/// rejects auth-ref changes, atomically writes the canonical form to the
/// default config path, and records a `server.config_imported` audit event.
/// The running daemon retains its old `AppState`; the client must restart
/// the daemon for the new config to take effect.
pub(crate) async fn config_import_handler(
    State(state): State<AppState>,
    body: String,
) -> std::result::Result<ApiSuccess<ConfigImportResponse>, StackError> {
    let incoming = crate::config::load_config_from_str(&body)?;
    crate::config::compare_auth_refs(&state.config.auth, &incoming.auth)?;
    let canonical = incoming.to_canonical_toml()?;
    let target = crate::config::default_config_path()?;
    if let Some(parent) = target.parent() {
        crate::fs_util::create_dir_owner_only(parent)?;
    }
    crate::fs_util::atomic_write_owner_only(&target, canonical.as_bytes())?;

    // Audit event: durable record that an import landed. Pin the path so the
    // operator's `acps logs events` shows which file changed. The import has
    // already succeeded on disk, so an event-write failure must not fail the
    // response — but it must also not be silently dropped (CLAUDE.md error
    // rule). Log at warn so monitoring sees the divergence.
    let payload = serde_json::json!({
        "path": target.to_string_lossy(),
        "size_bytes": canonical.len(),
    });
    match serde_json::to_string(&payload) {
        Ok(payload_text) => {
            let store = state.state.lock().await;
            if let Err(err) = store.append_event_with_source(
                "info",
                "server.config_imported",
                crate::state::EVENT_SOURCE_API,
                "config imported via /v1/config/import",
                &payload_text,
            ) {
                tracing::warn!(error = %err, "failed to record server.config_imported audit event");
            }
        }
        Err(err) => {
            tracing::warn!(error = %err, "failed to serialize config-import audit payload");
        }
    }

    Ok(ApiSuccess::new(ConfigImportResponse {
        imported: true,
        restart_required: true,
    }))
}

#[derive(Serialize)]
pub(crate) struct SecretsListResponse {
    names: Vec<String>,
}

#[derive(Deserialize)]
pub(crate) struct SecretsSetBody {
    name: String,
    value: String,
}

#[derive(Serialize)]
pub(crate) struct SecretsSetResponse {
    name: String,
    action: &'static str,
}

#[derive(Serialize)]
pub(crate) struct SecretsDeleteResponse {
    name: String,
    deleted: bool,
}

pub(crate) async fn secrets_list_handler(
    State(_state): State<AppState>,
) -> std::result::Result<ApiSuccess<SecretsListResponse>, StackError> {
    let home = home_dir()?;
    let store = crate::secrets::SecretStore::open(&home)?;
    let names = store.list_names().iter().map(|s| (*s).to_owned()).collect();
    Ok(ApiSuccess::new(SecretsListResponse { names }))
}

pub(crate) async fn secrets_set_handler(
    State(state): State<AppState>,
    Json(body): Json<SecretsSetBody>,
) -> std::result::Result<ApiSuccess<SecretsSetResponse>, StackError> {
    crate::secrets::reject_auth_ref_mutation(&body.name, &state.config)?;
    let home = home_dir()?;
    let mut store = crate::secrets::SecretStore::open(&home)?;
    let action = if store.contains(&body.name) {
        "updated"
    } else {
        "set"
    };
    store.set(&body.name, &body.value)?;
    Ok(ApiSuccess::new(SecretsSetResponse {
        name: body.name,
        action,
    }))
}

pub(crate) async fn secrets_delete_handler(
    Path(name): Path<String>,
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<SecretsDeleteResponse>, StackError> {
    crate::secrets::reject_auth_ref_mutation(&name, &state.config)?;
    let home = home_dir()?;
    let mut store = crate::secrets::SecretStore::open(&home)?;
    store.delete(&name)?;
    Ok(ApiSuccess::new(SecretsDeleteResponse {
        name,
        deleted: true,
    }))
}
