//! Admin-tier managed-state apply endpoint (`POST /v1/admin/extensions/{name}/apply`).
//! Revision semantics and ownership enforcement live in the secret store; this
//! handler resolves the namespace, serializes writers, and audits.

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
    // Serialize with every other secret-store writer: the store is a whole-file
    // read-modify-write, so the catalog swap must not interleave with config
    // import or the CLI credential commands.
    let _mutation = state.lock_agent_config_mutation().await?;
    // Load fresh from disk: `state.config` is a start-time snapshot that predates
    // any provider written by a later init.
    let runtime_config =
        crate::config::Config::load_lenient_from_path(&state.runtime_paths.config_path)?;
    let home = home_dir()?;
    let mut store = crate::secrets::SecretStore::open(&home)?;
    let revision = body.revision;
    // Captured before the apply so the model-catalog cache can be invalidated for
    // both the outgoing and the incoming provider.
    let previous_provider_id = store
        .managed_state_record(&name)
        .and_then(|record| record.provider_id.clone());
    let response =
        crate::extensions::managed_state::apply(&mut store, &runtime_config, &name, body)?;
    let new_provider_id = store
        .managed_state_record(&name)
        .and_then(|record| record.provider_id.clone());
    if response.outcome == "applied" || response.outcome == "cleared" {
        for provider_id in [previous_provider_id, new_provider_id]
            .into_iter()
            .flatten()
            .collect::<std::collections::BTreeSet<_>>()
        {
            if let Err(err) =
                crate::runtime::agent::provider_model_catalog::invalidate_provider_models(
                    &home,
                    &provider_id,
                )
            {
                tracing::warn!(
                    error = %err,
                    provider = %provider_id,
                    "provider model catalog invalidation failed after managed-state apply"
                );
            }
        }
    }

    // The agent reads its native config at process start, so the endpoint must be
    // on disk before the orchestrator's post-push restart. This runs on `noop` too,
    // or a retry after failed provisioning would leave it permanently unapplied.
    crate::runtime::agent::agent_headless_config::provision_agent_headless_config(
        &runtime_config,
        &home,
    )?;

    let payload = serde_json::json!({
        "namespace": name,
        "outcome": response.outcome,
        "revision": revision,
        "provider_id": store
            .managed_state_record(&name)
            .and_then(|record| record.provider_id.as_deref()),
    });
    // Audit failure is non-fatal: the store mutation is already durable, so failing
    // here would make the orchestrator retry a revision that was in fact applied.
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
