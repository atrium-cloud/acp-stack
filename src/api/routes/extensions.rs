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
    // Custom-provider validation reads the provider declared in the agent
    // TOML; load it fresh from disk because `state.config` is a start-time
    // snapshot that predates any provider written by a later init. Lenient,
    // like every other read-only reload: strictness belongs to config-write
    // paths, and one unusable MCP or skill declaration must not block a
    // credential rotation on a daemon that is already running.
    let runtime_config =
        crate::config::Config::load_lenient_from_path(&state.runtime_paths.config_path)?;
    let home = home_dir()?;
    let mut store = crate::secrets::SecretStore::open(&home)?;
    let revision = body.revision;
    // Captured before the apply so an applied/cleared outcome can invalidate
    // the model-catalog cache for both the outgoing and the incoming provider:
    // the override changes where the listing is fetched from, and a stale
    // entry would keep serving the previous endpoint's catalog.
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

    // The agent reads its native config at process start, so the endpoint must
    // be on disk before the restart the orchestrator triggers after a
    // credential push. This runs on `noop` too: the store write above is
    // already durable, so a retry after a failed provisioning would otherwise
    // replay as a no-op and leave the endpoint permanently unapplied.
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
