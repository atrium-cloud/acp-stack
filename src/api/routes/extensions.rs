//! Admin-tier managed-state apply endpoint (`POST /v1/admin/extensions/{name}/apply`).
//! Revision semantics and ownership enforcement live in the secret store; this
//! handler resolves the namespace, serializes writers, and audits.

use std::collections::{BTreeMap, BTreeSet};

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
use crate::runtime::agent::acp_bridge::{
    AgentCapabilitiesDto, ENDPOINT_VERDICT_AGENT_IDS, LaunchEnvRequest, hidden_native_providers,
};
use crate::runtime::agent::agent_headless_config::{
    provision_agent_headless_config, provision_for_launch,
};
use crate::runtime::agent::provider_keys::{
    ResolvedAgentEnvironment, agent_provider_env_names, has_custom_provider_slot,
};
use crate::runtime::agent::provider_model_catalog::probe_provider_endpoints;
use crate::runtime::agent::supervisor::AgentStateLabel;
use crate::secrets::{ProviderCredentialSet, SecretStore};

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
    // An agent whose launch env carries a hide list has its native config rewritten at each
    // launch or push instead, so a running adapter's default never leaves the list it holds.
    if !ENDPOINT_VERDICT_AGENT_IDS.contains(&config.agent.id.as_str()) {
        provision_agent_headless_config(&config, &home)?;
    }
    if let Some(environment) = environment {
        response.agent_environment =
            Some(agent_environment(&target, &config, environment, &home, &store).await);
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

/// `live` when no Agent runs, the running Agent's loaded providers already match the applied set,
/// or its adapter took the set's env through a launch-env push; else `restart_required`.
async fn agent_environment(
    target: &AgentTargetRuntime,
    config: &Config,
    environment: ResolvedAgentEnvironment,
    home: &std::path::Path,
    store: &SecretStore,
) -> &'static str {
    let snapshot = target.supervisor.snapshot().await;
    match snapshot.state {
        // A crash respawn may be waiting out its backoff; it launches with this env instead.
        AgentStateLabel::Stopped => {
            let recorded = target
                .supervisor
                .record_respawn_launch(environment, config.clone());
            return if recorded.await {
                AGENT_ENVIRONMENT_LIVE
            } else {
                AGENT_ENVIRONMENT_RESTART_REQUIRED
            };
        }
        AgentStateLabel::Running => {}
        _ => return AGENT_ENVIRONMENT_RESTART_REQUIRED,
    }
    if !super::status::provider_snapshot_requires_restart(
        snapshot.state,
        snapshot.loaded_providers.as_deref(),
        &environment.providers,
    ) {
        return AGENT_ENVIRONMENT_LIVE;
    }
    let takes_launch_env = snapshot
        .latest_capabilities
        .as_ref()
        .is_some_and(AgentCapabilitiesDto::supports_launch_env);
    // A custom provider's key is not among the provider variables a push carries.
    if !takes_launch_env || has_custom_provider_slot(&config.agent) {
        return AGENT_ENVIRONMENT_RESTART_REQUIRED;
    }
    // The pushed hide list and Pi's default endpoint come from one probe, as at a start.
    probe_provider_endpoints(home, &config.agent, &environment.env).await;
    let provider_names = agent_provider_env_names(&config.agent.id);
    // The network provider's `workload_env` lands last at spawn, so the adapter keeps its value,
    // and a name any spawn source sets stays in the adapter's env, as a restart would keep it.
    let network_names: BTreeSet<String> = crate::extensions::resolve_network_provider(config)
        .map(|provider| provider.workload_env.into_keys().collect())
        .unwrap_or_default();
    let sandbox_names = config.workspace.sandbox.workload_env.keys();
    let spawn_names: BTreeSet<&str> = (network_names.iter().chain(sandbox_names))
        .map(String::as_str)
        .chain(store.managed_environment_names())
        .collect();
    let request = LaunchEnvRequest {
        env: (environment.env.iter())
            .filter(|(name, _)| {
                provider_names.contains(name.as_str()) && !network_names.contains(*name)
            })
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect(),
        unset: (provider_names.into_iter())
            .filter(|name| !environment.env.contains_key(*name) && !spawn_names.contains(name))
            .map(str::to_owned)
            .collect(),
        hide_providers: hidden_native_providers(home, &config.agent)
            .into_iter()
            .map(str::to_owned)
            .collect(),
    };
    match target
        .supervisor
        .push_launch_env(environment, config.clone(), &request)
        .await
    {
        // Native config follows the hide list only once the adapter holds it.
        Ok(()) if provision_for_launch(config, home) => AGENT_ENVIRONMENT_LIVE,
        Ok(()) => AGENT_ENVIRONMENT_RESTART_REQUIRED,
        Err(error) => {
            tracing::warn!(
                error = %crate::error::report(&error),
                "launch env push failed; the running Agent restarts to take the applied providers"
            );
            AGENT_ENVIRONMENT_RESTART_REQUIRED
        }
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
