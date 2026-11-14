//! Admin-tier managed-state apply endpoint (`POST /v1/admin/extensions/{name}/apply`).
//! Revision semantics and ownership enforcement live in the secret store; this
//! handler resolves the namespace, serializes writers, and audits.

use std::collections::BTreeMap;

use axum::Json;
use axum::extract::{Path, State};

use super::super::core::{AgentTargetRuntime, AppState};
use crate::config::Config;
use crate::envelope::ApiSuccess;
use crate::error::StackError;
use crate::extensions::managed_state::{
    AGENT_ENVIRONMENT_LIVE, AGENT_ENVIRONMENT_RESTART_REQUIRED, ApplyRequest, ApplyResponse,
    KIND_PROVIDER_CREDENTIAL_SET, KIND_SECRET_ENVIRONMENT,
};
use crate::runtime::agent::agent_headless_config::provision_agent_headless_config;
use crate::runtime::agent::provider_keys::ResolvedAgentEnvironment;
use crate::secrets::{ManagedApplyOutcome, ProviderCredentialSet, SecretStore};

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
    let (runtime_config, dropped) =
        Config::load_lenient_from_path_reporting(&state.runtime_paths.config_path)?;
    let home = state.runtime_paths.home.clone();
    let mut store = SecretStore::open(&home)?;
    let revision = body.revision;
    let kind = body.desired.kind();
    // Computed before the store commits, so a config the Agent cannot run leaves no watermark.
    let canonical = crate::extensions::managed_state::credential_config(
        &runtime_config,
        &store,
        &name,
        &body.desired,
    )?;
    let previous_catalog = store.provider_credentials().clone();
    let previous_provider_ids = store.managed_provider_ids(&name);
    let mut response =
        crate::extensions::managed_state::apply(&mut store, &runtime_config, &name, body)?;
    if kind == KIND_SECRET_ENVIRONMENT {
        // Workload spawns read the entries from the store, so nothing is provisioned here.
        record_apply_event(&state, &name, kind, &response, revision, &store).await;
        return Ok(ApiSuccess::new(response));
    }
    let applied_provider_ids = store.managed_provider_ids(&name);
    let touched = previous_provider_ids.union(&applied_provider_ids);
    invalidate_changed_provider_models(&home, &previous_catalog, &store, touched);

    // The config write runs on `noop` too, so a replay converges after a failed write. The
    // agent reads its native config at process start.
    if let Some(canonical) = &canonical {
        super::skills::warn_dropped_declarations_healed(&dropped);
        crate::fs_util::atomic_write_owner_only(
            &state.runtime_paths.config_path,
            canonical.as_bytes(),
        )?;
    }
    let (config, target) = state.default_agent_target().await?;
    if canonical.is_some() {
        *target.live_agent_config.lock().await = config.agent.clone();
    }
    let environment = (kind == KIND_PROVIDER_CREDENTIAL_SET)
        .then(|| super::agent::open_agent_environment(&state, &config))
        .and_then(|resolved| applied_environment(resolved, &mut response));
    // A `noop` set changes nothing a running Agent runs on, and a rewrite could move its default
    // endpoint off the hide list it started with; every start rewrites native config itself.
    let running_noop = kind == KIND_PROVIDER_CREDENTIAL_SET
        && response.outcome == ManagedApplyOutcome::Noop.as_str()
        && target.supervisor.is_running().await;
    if !running_noop {
        provision_agent_headless_config(&config, &home)?;
    }
    if let Some(environment) = environment {
        response.agent_environment = Some(agent_environment(&target, &environment).await);
    }

    record_apply_event(&state, &name, kind, &response, revision, &store).await;
    Ok(ApiSuccess::new(response))
}

/// The revision is applied by now, so a launch env that does not resolve degrades as the status
/// route does: logged by name, and the Agent reported as needing a restart.
fn applied_environment(
    resolved: std::result::Result<ResolvedAgentEnvironment, StackError>,
    response: &mut ApplyResponse,
) -> Option<ResolvedAgentEnvironment> {
    match resolved {
        Ok(environment) => Some(environment),
        Err(error) => {
            tracing::warn!(
                error = %crate::error::report(&error),
                "managed-state apply: the Agent launch env does not resolve; reporting a restart"
            );
            response.agent_environment = Some(AGENT_ENVIRONMENT_RESTART_REQUIRED);
            None
        }
    }
}

/// A provider whose credential was added, removed or changed answers its listing differently, so
/// its cached catalog and endpoint verdicts are dropped.
fn invalidate_changed_provider_models<'a>(
    home: &std::path::Path,
    previous_catalog: &BTreeMap<String, ProviderCredentialSet>,
    store: &SecretStore,
    provider_ids: impl Iterator<Item = &'a String>,
) {
    for provider_id in provider_ids {
        if previous_catalog.get(provider_id) == store.provider_credential_set(provider_id) {
            continue;
        }
        if let Err(err) = crate::runtime::agent::provider_model_catalog::invalidate_provider_models(
            home,
            provider_id,
        ) {
            tracing::warn!(
                error = %err,
                provider = %provider_id,
                "provider model catalog invalidation failed after managed-state apply"
            );
        }
    }
}

/// `live` when no Agent runs or the running Agent's loaded providers already match the applied
/// set; else `restart_required`.
async fn agent_environment(
    target: &AgentTargetRuntime,
    environment: &ResolvedAgentEnvironment,
) -> &'static str {
    let snapshot = target.supervisor.snapshot().await;
    if super::status::provider_snapshot_requires_restart(
        snapshot.state,
        snapshot.loaded_providers.as_deref(),
        &environment.providers,
    ) {
        AGENT_ENVIRONMENT_RESTART_REQUIRED
    } else {
        AGENT_ENVIRONMENT_LIVE
    }
}

/// Audit failure is non-fatal: the store mutation is already durable, so failing here
/// would make the orchestrator retry a revision that was in fact applied.
async fn record_apply_event(
    state: &AppState,
    namespace: &str,
    kind: &str,
    response: &ApplyResponse,
    revision: i64,
    store: &crate::secrets::SecretStore,
) {
    let record = store.managed_state_record(namespace);
    let payload = serde_json::json!({
        "namespace": namespace,
        "kind": kind,
        "outcome": response.outcome,
        "revision": revision,
        "provider_id": record.and_then(|record| record.provider_id.as_deref()),
        "provider_ids": record.map(|record| &record.provider_ids),
        "environment_names": record
            .map(|record| record.environment.keys().collect::<Vec<_>>())
            .unwrap_or_default(),
    });
    match serde_json::to_string(&payload) {
        Ok(payload_text) => {
            let recorded = state
                .state
                .write(move |store| {
                    store.append_event_with_source(
                        "info",
                        "server.extension_managed_state_applied",
                        crate::state::EVENT_SOURCE_API,
                        "managed-state extension registry applied",
                        &payload_text,
                    )?;
                    Ok(())
                })
                .await;
            if let Err(err) = recorded {
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
}
