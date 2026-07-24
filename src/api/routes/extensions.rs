//! Admin-tier managed-state apply endpoint.
//!
//! `POST /v1/admin/extensions/{name}/apply` is the fixed, namespace-
//! parameterized seam of the managed-state extension type: `{name}` must
//! resolve to a declared `type = "managed-state"` instance, and the request
//! body is the generic `{schema_version, revision, desired}` contract defined
//! in `crate::extensions::managed_state`. Revision semantics and ownership
//! enforcement live in the secret store; this handler only resolves the
//! namespace, serializes with other secret-store writers, and records the
//! audit event (which never carries credential values).

use axum::Json;
use axum::extract::{Path, State};

use super::super::core::AppState;
use crate::envelope::ApiSuccess;
use crate::error::StackError;
use crate::extensions::managed_state::{ApplyRequest, ApplyResponse};
use crate::fs_util::home_dir;

pub(crate) async fn extension_managed_state_apply_handler(
    Path(name): Path<String>,
    State(state): State<AppState>,
    Json(body): Json<ApplyRequest>,
) -> std::result::Result<ApiSuccess<ApplyResponse>, StackError> {
    crate::extensions::require_managed_state(&state.config, &name)?;
    // Serialize with every other secret-store writer: the store is a
    // whole-file read-modify-write, and the catalog swap + watermark persist
    // must not interleave with config import or the CLI credential commands.
    let _mutation = state.lock_agent_config_mutation().await?;
    let home = home_dir()?;
    let mut store = crate::secrets::SecretStore::open(&home)?;
    let revision = body.revision;
    let response = crate::extensions::managed_state::apply(&mut store, &name, body)?;

    let payload = serde_json::json!({
        "namespace": name,
        "outcome": response.outcome,
        "revision": revision,
        "provider_id": store
            .managed_state_record(&name)
            .and_then(|record| record.provider_id.as_deref()),
    });
    // Audit failure is deliberately non-fatal: the store mutation above is
    // already durable, so failing the request here would make the orchestrator
    // retry a revision that was in fact applied and read the 409 as a bug.
    match serde_json::to_string(&payload) {
        Ok(payload_text) => {
            let store = state.state.lock().await;
            if let Err(err) = store.append_event_with_source(
                "info",
                "server.extension_managed_state_applied",
                crate::state::EVENT_SOURCE_API,
                "managed-state extension registry applied",
                &payload_text,
            ) {
                tracing::warn!(
                    error = %err,
                    "failed to record server.extension_managed_state_applied audit event"
                );
            }
        }
        Err(err) => {
            tracing::warn!(error = %err, "failed to serialize managed-state audit payload");
        }
    }

    Ok(ApiSuccess::new(response))
}
