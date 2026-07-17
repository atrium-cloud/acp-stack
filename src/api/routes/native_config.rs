use std::path::Path as FsPath;

use axum::Json;
use axum::extract::{Path, State};
use serde::Deserialize;

use super::agent::{
    cancel_pending_acp_permissions_for_target, ensure_array_process_start_allowed,
    open_agent_environment,
};
use crate::api::core::AppState;
use crate::config::Config;
use crate::envelope::ApiSuccess;
use crate::error::{Result, StackError};
use crate::fs_util::home_dir;
use crate::runtime::agent::acp_bridge::AgentSessionConfigCategory;
use crate::runtime::agent::model_discovery::{
    DEFAULT_MODELS_DISCOVERY_TIMEOUT, fetch_session_config_with_timeout,
    model_value_is_explicit_without_discovery, validate_advertised_value,
};
use crate::runtime::agent::native_config_import::{
    APPLIED_ROLLBACK_RETENTION_SECONDS, NativeConfigImportRequest, NativeConfigInspection,
    NativeConfigOperation, NativeConfigOperationError, NativeConfigOperationPhase,
    NativeConfigOperationRecord, NativeConfigOperationStatus, NativeConfigPathSnapshot,
    NativeConfigRestartMetadata, PreparedNativeConfigImport, TERMINAL_RETENTION_SECONDS,
    capture_native_config_file_digests, capture_native_config_snapshots,
    load_native_config_operation_journal, native_config_projection,
    native_config_transaction_paths, next_native_config_operation_id,
    persist_native_config_operation, prepare_native_config_file_paths,
    remove_native_config_operation_journal, restore_native_config_snapshots,
    validate_native_config_file_digests, validate_native_config_secret_refs_read_only,
    write_native_config_files,
};
use crate::runtime::agent::supervisor::{AgentStartRequest, AgentStateLabel};

const QUEUED_IMPORT_POLL_SECONDS: u64 = 2;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeConfigInspectBody {
    filename: String,
    content: String,
}

pub(crate) async fn native_config_inspect_handler(
    State(state): State<AppState>,
    Json(body): Json<NativeConfigInspectBody>,
) -> std::result::Result<ApiSuccess<NativeConfigInspection>, StackError> {
    let config = state.refresh_array_runtime_from_disk().await?;
    let inspection = state.native_config_imports.lock().await.inspect(
        &config.agent.id,
        Some(&body.filename),
        &body.content,
    )?;
    Ok(ApiSuccess::new(inspection))
}

pub(crate) async fn native_config_import_handler(
    State(state): State<AppState>,
    Json(request): Json<NativeConfigImportRequest>,
) -> std::result::Result<ApiSuccess<NativeConfigOperation>, StackError> {
    let selection = request.selection();
    let home = home_dir()?;
    let current = state.refresh_array_runtime_from_disk().await?;
    let prepared = state
        .native_config_imports
        .lock()
        .await
        .prepare(&selection, &current, &home)?;
    validate_native_config_secret_refs_read_only(&prepared, &home)?;
    let _mutation = state.lock_agent_config_mutation().await?;

    let operation_id = next_native_config_operation_id();
    let target_id = current.array.primary_target.clone();
    let operation = operation_for(
        &operation_id,
        &prepared,
        NativeConfigOperationStatus::Queued,
        NativeConfigRestartMetadata {
            required: true,
            queued: true,
            restarted: false,
            target_id,
        },
        None,
    );
    {
        let mut imports = state.native_config_imports.lock().await;
        if let Some(existing) = imports.operation_for_fingerprint(&prepared.transaction_fingerprint)
            && (existing.operation.status == NativeConfigOperationStatus::Queued
                || validate_native_config_file_digests(&existing.applied_file_digests, &home)
                    .is_ok())
        {
            return Ok(ApiSuccess::new(existing.operation));
        }
        if imports
            .operations
            .values()
            .any(|record| operation_phase_is_pending(record.phase))
        {
            return Err(native_error("native_config_operation_in_progress"));
        }
        imports.insert_operation(NativeConfigOperationRecord {
            operation: operation.clone(),
            transaction_fingerprint: prepared.transaction_fingerprint.clone(),
            prepared: Some(prepared),
            rollback_snapshots: Vec::new(),
            prior_config: None,
            prior_was_running: false,
            applied_file_digests: Vec::new(),
            applied_at: None,
            updated_at: chrono::Utc::now(),
            cancelled: false,
            phase: NativeConfigOperationPhase::Staged,
        });
    }
    if let Err(error) = persist_operation_record(&state, &operation_id).await {
        state
            .native_config_imports
            .lock()
            .await
            .operations
            .remove(&operation_id);
        return Err(error);
    }

    let outcome = match apply_stored_operation_locked(&state, &operation_id).await {
        Ok(outcome) => outcome,
        Err(error) => {
            let record = operation_record(&state, &operation_id).await?;
            if record.phase == NativeConfigOperationPhase::Staged {
                mark_failed(&state, &operation_id, error.error_code()).await?;
                persist_operation_record(&state, &operation_id).await?;
            } else if matches!(
                record.phase,
                NativeConfigOperationPhase::Applying | NativeConfigOperationPhase::RollingBack
            ) {
                spawn_queued_worker(state.clone(), operation_id.clone());
            }
            let operation = operation_record(&state, &operation_id).await?.operation;
            return Ok(ApiSuccess::new(operation));
        }
    };
    match outcome {
        ApplyStoredOutcome::Applied(operation) => Ok(ApiSuccess::new(operation)),
        ApplyStoredOutcome::Blocked(operation) => {
            spawn_queued_worker(state.clone(), operation_id);
            Ok(ApiSuccess::new(operation))
        }
    }
}

pub(crate) async fn native_config_status_handler(
    State(state): State<AppState>,
    Path(operation_id): Path<String>,
) -> std::result::Result<ApiSuccess<NativeConfigOperation>, StackError> {
    let operation = state
        .native_config_imports
        .lock()
        .await
        .operation(&operation_id)
        .ok_or_else(|| native_error("native_config_operation_not_found"))?;
    Ok(ApiSuccess::new(operation))
}

pub(crate) async fn native_config_cancel_handler(
    State(state): State<AppState>,
    Path(operation_id): Path<String>,
) -> std::result::Result<ApiSuccess<NativeConfigOperation>, StackError> {
    let _mutation = state.lock_agent_config_mutation().await?;
    let original = operation_record(&state, &operation_id).await?;
    if original.operation.status == NativeConfigOperationStatus::Applied
        && native_config_rollback_expired(&original)
    {
        return Err(native_error("native_config_rollback_expired"));
    }
    let validate_applied_files = match original.phase {
        NativeConfigOperationPhase::Terminal => {
            if original.operation.status == NativeConfigOperationStatus::Applied {
                return Err(native_error("native_config_rollback_expired"));
            }
            return Ok(ApiSuccess::new(original.operation));
        }
        NativeConfigOperationPhase::Staged | NativeConfigOperationPhase::CancellingQueued => {
            let marker = mutate_operation_record(&state, &operation_id, |record| {
                record.cancelled = true;
                record.updated_at = chrono::Utc::now();
                record.phase = NativeConfigOperationPhase::CancellingQueued;
            })
            .await?;
            if let Err(error) = persist_operation_record(&state, &operation_id).await {
                replace_operation_record(&state, original).await?;
                return Err(error);
            }
            let operation = finalize_queued_cancellation(&state, &operation_id).await?;
            if let Err(error) = persist_operation_record(&state, &operation_id).await {
                replace_operation_record(&state, marker).await?;
                spawn_queued_worker(state.clone(), operation_id.clone());
                return Err(error);
            }
            return Ok(ApiSuccess::new(operation));
        }
        NativeConfigOperationPhase::RollingBack => {
            if !original.cancelled {
                mutate_operation_record(&state, &operation_id, |record| {
                    record.cancelled = true;
                    record.updated_at = chrono::Utc::now();
                })
                .await?;
                if let Err(error) = persist_operation_record(&state, &operation_id).await {
                    replace_operation_record(&state, original).await?;
                    return Err(error);
                }
            }
            let outcome = resume_pending_rollback_locked(&state, &operation_id).await?;
            return match outcome {
                ApplyStoredOutcome::Applied(operation) => Ok(ApiSuccess::new(operation)),
                ApplyStoredOutcome::Blocked(operation) => {
                    spawn_queued_worker(state.clone(), operation_id);
                    Ok(ApiSuccess::new(operation))
                }
            };
        }
        NativeConfigOperationPhase::Applying => false,
        NativeConfigOperationPhase::Applied => true,
    };

    if validate_applied_files {
        ensure_latest_applied_operation(&state, &operation_id).await?;
        validate_native_config_file_digests(&original.applied_file_digests, &home_dir()?)?;
    }
    let rollback_marker = mutate_operation_record(&state, &operation_id, |record| {
        record.cancelled = true;
        record.operation.status = NativeConfigOperationStatus::Queued;
        record.operation.restart.required = record.prior_was_running;
        record.operation.restart.queued = true;
        record.operation.restart.restarted = false;
        record.operation.error = None;
        record.updated_at = chrono::Utc::now();
        record.phase = NativeConfigOperationPhase::RollingBack;
    })
    .await?;
    if let Err(error) = persist_operation_record(&state, &operation_id).await {
        replace_operation_record(&state, original).await?;
        return Err(error);
    }
    match resume_pending_rollback_locked(&state, &operation_id).await {
        Ok(ApplyStoredOutcome::Applied(operation)) => Ok(ApiSuccess::new(operation)),
        Ok(ApplyStoredOutcome::Blocked(operation)) => {
            spawn_queued_worker(state.clone(), operation_id);
            Ok(ApiSuccess::new(operation))
        }
        Err(error) => {
            replace_operation_record(&state, rollback_marker).await?;
            spawn_queued_worker(state.clone(), operation_id);
            Err(error)
        }
    }
}

fn native_config_rollback_expired(record: &NativeConfigOperationRecord) -> bool {
    chrono::Utc::now()
        .signed_duration_since(record.applied_at.unwrap_or(record.updated_at))
        .num_seconds()
        >= APPLIED_ROLLBACK_RETENTION_SECONDS as i64
}

async fn ensure_latest_applied_operation(state: &AppState, operation_id: &str) -> Result<()> {
    let imports = state.native_config_imports.lock().await;
    let latest = imports
        .operations
        .values()
        .filter(|record| record.phase == NativeConfigOperationPhase::Applied)
        .filter_map(|record| {
            record
                .applied_at
                .map(|applied_at| (applied_at, record.operation.operation_id.as_str()))
        })
        .max();
    if latest.is_some_and(|(_, latest_id)| latest_id != operation_id) {
        return Err(native_error("native_config_rollback_conflict"));
    }
    Ok(())
}

enum ApplyStoredOutcome {
    Applied(NativeConfigOperation),
    Blocked(NativeConfigOperation),
}

async fn apply_stored_operation(
    state: &AppState,
    operation_id: &str,
) -> Result<ApplyStoredOutcome> {
    let _mutation = state.lock_agent_config_mutation().await?;
    apply_stored_operation_locked(state, operation_id).await
}

async fn apply_stored_operation_locked(
    state: &AppState,
    operation_id: &str,
) -> Result<ApplyStoredOutcome> {
    let (prepared, phase, stored_prior, stored_snapshots, stored_prior_was_running) = {
        let imports = state.native_config_imports.lock().await;
        let record = imports
            .operations
            .get(operation_id)
            .ok_or_else(|| native_error("native_config_operation_not_found"))?;
        if record.cancelled {
            return Ok(ApplyStoredOutcome::Blocked(record.operation.clone()));
        }
        (
            record
                .prepared
                .clone()
                .ok_or_else(|| native_error("native_config_operation_invalid"))?,
            record.phase,
            record.prior_config.clone(),
            record.rollback_snapshots.clone(),
            record.prior_was_running,
        )
    };
    let home = home_dir()?;
    validate_native_config_secret_refs_read_only(&prepared, &home)?;
    let resuming_apply = phase == NativeConfigOperationPhase::Applying;
    let prior_config = if resuming_apply {
        stored_prior.ok_or_else(|| native_error("native_config_journal_invalid"))?
    } else {
        state.refresh_array_runtime_from_disk().await?
    };
    if !resuming_apply {
        let current_revision = crate::runtime::agent::native_config_import::sha256_hex(
            prior_config.to_canonical_toml()?.as_bytes(),
        );
        if current_revision != prepared.base_config_revision
            || prior_config.agent.id != prepared.harness
        {
            return Err(native_error("native_config_base_config_changed"));
        }
    }
    let target_id = prior_config.array.primary_target.clone();
    ensure_array_process_start_allowed(&prepared.canonical_config, &target_id)?;
    let target = state.agent_target(&target_id)?;
    let paths = native_config_transaction_paths(
        &state.runtime_paths.config_path,
        &prepared.native_path,
        &prepared.harness,
        &home,
    );
    let (snapshots, prior_was_running) = if resuming_apply {
        if stored_snapshots.is_empty() {
            return Err(native_error("native_config_journal_invalid"));
        }
        (stored_snapshots, stored_prior_was_running)
    } else {
        let supervisor_state = target.supervisor.snapshot().await.state;
        if matches!(
            supervisor_state,
            AgentStateLabel::Starting | AgentStateLabel::Stopping | AgentStateLabel::Updating
        ) {
            let operation = operation_record(state, operation_id).await?.operation;
            return Ok(ApplyStoredOutcome::Blocked(operation));
        }
        let blockers = {
            let store = state.state.lock().await;
            store.query_restart_blockers(Some(&target_id))?
        };
        if !blockers.is_empty() {
            let operation = state
                .native_config_imports
                .lock()
                .await
                .operations
                .get(operation_id)
                .map(|record| record.operation.clone())
                .ok_or_else(|| native_error("native_config_operation_not_found"))?;
            return Ok(ApplyStoredOutcome::Blocked(operation));
        }
        let prior_was_running = supervisor_state == AgentStateLabel::Running;
        let snapshots = capture_native_config_snapshots(&paths, &home)?;
        let mut applying_marker = operation_record(state, operation_id).await?;
        applying_marker.rollback_snapshots = snapshots.clone();
        applying_marker.prior_config = Some(prior_config.clone());
        applying_marker.prior_was_running = prior_was_running;
        applying_marker.updated_at = chrono::Utc::now();
        applying_marker.phase = NativeConfigOperationPhase::Applying;
        persist_operation_record_value(state, &applying_marker)?;
        replace_operation_record(state, applying_marker).await?;
        (snapshots, prior_was_running)
    };
    prepare_native_config_file_paths(&prepared, &state.runtime_paths.config_path, &home)?;
    let applying_record = operation_record(state, operation_id).await?;

    let live_state = target.supervisor.snapshot().await.state;
    if matches!(
        live_state,
        AgentStateLabel::Starting | AgentStateLabel::Stopping | AgentStateLabel::Updating
    ) || (live_state == AgentStateLabel::Running && !prior_was_running)
    {
        if !resuming_apply {
            mutate_operation_record(state, operation_id, |record| {
                record.rollback_snapshots.clear();
                record.prior_config = None;
                record.prior_was_running = false;
                record.updated_at = chrono::Utc::now();
                record.phase = NativeConfigOperationPhase::Staged;
            })
            .await?;
            if persist_operation_record(state, operation_id).await.is_err() {
                replace_operation_record(state, applying_record.clone()).await?;
            }
        }
        let operation = operation_record(state, operation_id).await?.operation;
        return Ok(ApplyStoredOutcome::Blocked(operation));
    }
    if live_state == AgentStateLabel::Running {
        match target
            .supervisor
            .stop_when_restart_safe(&target_id, &state.state, &state.event_hub)
            .await
        {
            Ok(Ok(_)) => {
                cancel_pending_acp_permissions_for_target(
                    state,
                    &target_id,
                    "native-config-import",
                )
                .await;
            }
            Ok(Err(_)) => {
                if !resuming_apply {
                    mutate_operation_record(state, operation_id, |record| {
                        record.rollback_snapshots.clear();
                        record.prior_config = None;
                        record.prior_was_running = false;
                        record.updated_at = chrono::Utc::now();
                        record.phase = NativeConfigOperationPhase::Staged;
                    })
                    .await?;
                    if persist_operation_record(state, operation_id).await.is_err() {
                        replace_operation_record(state, applying_record.clone()).await?;
                    }
                }
                let operation = state
                    .native_config_imports
                    .lock()
                    .await
                    .operations
                    .get(operation_id)
                    .map(|record| record.operation.clone())
                    .ok_or_else(|| native_error("native_config_operation_not_found"))?;
                return Ok(ApplyStoredOutcome::Blocked(operation));
            }
            Err(error) => {
                return rollback_failed_apply(
                    state,
                    operation_id,
                    &applying_record,
                    &snapshots,
                    &prior_config,
                    prior_was_running,
                    &home,
                    error.error_code(),
                )
                .await;
            }
        }
    }

    if let Err(error) = apply_files_and_runtime(state, &prepared, prior_was_running).await {
        return rollback_failed_apply(
            state,
            operation_id,
            &applying_record,
            &snapshots,
            &prior_config,
            prior_was_running,
            &home,
            error.error_code(),
        )
        .await;
    }

    let applied_file_digests = match capture_native_config_file_digests(&paths, &home) {
        Ok(digests) => digests,
        Err(error) => {
            return rollback_failed_apply(
                state,
                operation_id,
                &applying_record,
                &snapshots,
                &prior_config,
                prior_was_running,
                &home,
                error.error_code(),
            )
            .await;
        }
    };
    let mut imports = state.native_config_imports.lock().await;
    let record = imports
        .operations
        .get_mut(operation_id)
        .ok_or_else(|| native_error("native_config_operation_not_found"))?;
    record.operation.status = NativeConfigOperationStatus::Applied;
    record.operation.restart.queued = false;
    record.operation.restart.required = prior_was_running;
    record.operation.restart.restarted = prior_was_running;
    record.operation.agent_config = native_config_projection(&prepared.canonical_config);
    record.prepared = None;
    record.applied_file_digests = applied_file_digests;
    record.applied_at = Some(chrono::Utc::now());
    record.updated_at = chrono::Utc::now();
    record.phase = NativeConfigOperationPhase::Applied;
    let operation = record.operation.clone();
    drop(imports);
    if let Err(error) = persist_operation_record(state, operation_id).await {
        replace_operation_record(state, applying_record.clone()).await?;
        return rollback_failed_apply(
            state,
            operation_id,
            &applying_record,
            &snapshots,
            &prior_config,
            prior_was_running,
            &home,
            error.error_code(),
        )
        .await;
    }
    Ok(ApplyStoredOutcome::Applied(operation))
}

async fn apply_files_and_runtime(
    state: &AppState,
    prepared: &PreparedNativeConfigImport,
    restart: bool,
) -> Result<()> {
    write_native_config_files(prepared, &state.runtime_paths.config_path, &home_dir()?)?;
    if prepared.imported_model
        && !model_value_is_explicit_without_discovery(&prepared.canonical_config)
    {
        let model = native_config_projection(&prepared.canonical_config)
            .model
            .ok_or_else(|| native_error("native_config_model_invalid"))?;
        let response = fetch_session_config_with_timeout(
            &home_dir()?,
            &prepared.canonical_config,
            DEFAULT_MODELS_DISCOVERY_TIMEOUT,
        )
        .await?;
        validate_advertised_value(&response, AgentSessionConfigCategory::Model, &model)?;
    }
    let fresh = state.refresh_array_runtime_from_disk().await?;
    if restart {
        start_agent_for_config(state, &fresh).await?;
    } else {
        let target = state.agent_target(&fresh.array.primary_target)?;
        *target.live_agent_config.lock().await = fresh.agent.clone();
    }
    Ok(())
}

async fn start_agent_for_config(state: &AppState, config: &Config) -> Result<()> {
    let target_id = config.array.primary_target.clone();
    ensure_array_process_start_allowed(config, &target_id)?;
    let target = state.agent_target(&target_id)?;
    let environment = open_agent_environment(config)?;
    *target.live_agent_config.lock().await = config.agent.clone();
    target
        .supervisor
        .start(AgentStartRequest {
            target_id: &target_id,
            agent: &config.agent,
            workspace_root: &config.workspace.root,
            env: environment.env,
            providers: environment.providers,
            state: &state.state,
            session_changes: &state.session_changes,
            event_hub: state.event_hub.clone(),
            permissions: Some(state.permissions.clone()),
            sandbox: config.workspace.sandbox.clone(),
        })
        .await?;
    Ok(())
}

async fn restore_transaction_and_agent(
    state: &AppState,
    snapshots: &[NativeConfigPathSnapshot],
    prior_config: &Config,
    prior_was_running: bool,
    home: &FsPath,
) -> Result<()> {
    let target = state.agent_target(&prior_config.array.primary_target)?;
    if target.supervisor.snapshot().await.state == AgentStateLabel::Running {
        target
            .supervisor
            .stop(&target.target_id, &state.state, &state.event_hub)
            .await?;
    }
    restore_native_config_snapshots(snapshots, home)?;
    state.refresh_array_runtime_from_disk().await?;
    if prior_was_running {
        start_agent_for_config(state, prior_config).await?;
    } else {
        *target.live_agent_config.lock().await = prior_config.agent.clone();
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn rollback_failed_apply(
    state: &AppState,
    operation_id: &str,
    applying_record: &NativeConfigOperationRecord,
    snapshots: &[NativeConfigPathSnapshot],
    prior_config: &Config,
    prior_was_running: bool,
    home: &FsPath,
    error_code: &str,
) -> Result<ApplyStoredOutcome> {
    mutate_operation_record(state, operation_id, |record| {
        record.operation.status = NativeConfigOperationStatus::Failed;
        record.operation.restart.queued = false;
        record.operation.error = Some(NativeConfigOperationError {
            code: error_code.to_owned(),
        });
        record.updated_at = chrono::Utc::now();
        record.phase = NativeConfigOperationPhase::RollingBack;
    })
    .await?;
    if persist_operation_record(state, operation_id).await.is_err() {
        tracing::warn!(
            operation_id,
            "failed to persist native config rollback marker; durable applying marker remains recoverable"
        );
    }

    if restore_transaction_and_agent(state, snapshots, prior_config, prior_was_running, home)
        .await
        .is_err()
    {
        let rollback_failed =
            queue_rollback_retry(state, operation_id, "native_config_rollback_failed").await?;
        if persist_operation_record(state, operation_id).await.is_err() {
            tracing::warn!(
                operation_id,
                "failed to persist native config rollback failure; durable applying or rollback marker remains recoverable"
            );
        }
        return Ok(ApplyStoredOutcome::Blocked(rollback_failed.operation));
    }

    let settled = mutate_operation_record(state, operation_id, |record| {
        record.operation.status = NativeConfigOperationStatus::Failed;
        record.operation.agent_config = native_config_projection(prior_config);
        record.operation.restart.queued = false;
        record.operation.restart.required = prior_was_running;
        record.operation.restart.restarted = prior_was_running;
        record.prepared = None;
        record.rollback_snapshots.clear();
        record.applied_file_digests.clear();
        record.updated_at = chrono::Utc::now();
        record.phase = NativeConfigOperationPhase::Terminal;
    })
    .await?;
    if persist_operation_record(state, operation_id).await.is_err() {
        replace_operation_record(state, applying_record.clone()).await?;
        let retry =
            queue_rollback_retry(state, operation_id, "native_config_journal_persist_failed")
                .await?;
        if persist_operation_record(state, operation_id).await.is_err() {
            tracing::warn!(
                operation_id,
                "failed to persist native config rollback retry after restoring prior files"
            );
        }
        return Ok(ApplyStoredOutcome::Blocked(retry.operation));
    }
    Ok(ApplyStoredOutcome::Applied(settled.operation))
}

async fn queue_rollback_retry(
    state: &AppState,
    operation_id: &str,
    error_code: &str,
) -> Result<NativeConfigOperationRecord> {
    mutate_operation_record(state, operation_id, |record| {
        record.operation.status = NativeConfigOperationStatus::Queued;
        record.operation.restart.required = record.prior_was_running;
        record.operation.restart.queued = true;
        record.operation.restart.restarted = false;
        record.operation.error = Some(NativeConfigOperationError {
            code: error_code.to_owned(),
        });
        record.updated_at = chrono::Utc::now();
        record.phase = NativeConfigOperationPhase::RollingBack;
    })
    .await
}

fn operation_for(
    operation_id: &str,
    prepared: &PreparedNativeConfigImport,
    status: NativeConfigOperationStatus,
    restart: NativeConfigRestartMetadata,
    error: Option<&str>,
) -> NativeConfigOperation {
    NativeConfigOperation {
        operation_id: operation_id.to_owned(),
        status,
        harness: prepared.harness.clone(),
        revision: prepared.revision.clone(),
        agent_config: native_config_projection(&prepared.canonical_config),
        restart,
        error: error.map(|code| NativeConfigOperationError {
            code: code.to_owned(),
        }),
    }
}

async fn mark_failed(state: &AppState, operation_id: &str, code: &str) -> Result<()> {
    let current = state.refresh_array_runtime_from_disk().await?;
    let mut imports = state.native_config_imports.lock().await;
    let record = imports
        .operations
        .get_mut(operation_id)
        .ok_or_else(|| native_error("native_config_operation_not_found"))?;
    record.operation.status = NativeConfigOperationStatus::Failed;
    record.operation.agent_config = native_config_projection(&current);
    record.operation.restart.queued = false;
    record.operation.error = Some(NativeConfigOperationError {
        code: code.to_owned(),
    });
    record.prepared = None;
    record.updated_at = chrono::Utc::now();
    record.phase = NativeConfigOperationPhase::Terminal;
    Ok(())
}

async fn operation_record(
    state: &AppState,
    operation_id: &str,
) -> Result<NativeConfigOperationRecord> {
    state
        .native_config_imports
        .lock()
        .await
        .operations
        .get(operation_id)
        .cloned()
        .ok_or_else(|| native_error("native_config_operation_not_found"))
}

async fn mutate_operation_record(
    state: &AppState,
    operation_id: &str,
    mutate: impl FnOnce(&mut NativeConfigOperationRecord),
) -> Result<NativeConfigOperationRecord> {
    let mut imports = state.native_config_imports.lock().await;
    let record = imports
        .operations
        .get_mut(operation_id)
        .ok_or_else(|| native_error("native_config_operation_not_found"))?;
    mutate(record);
    Ok(record.clone())
}

async fn replace_operation_record(
    state: &AppState,
    record: NativeConfigOperationRecord,
) -> Result<()> {
    let operation_id = record.operation.operation_id.clone();
    let mut imports = state.native_config_imports.lock().await;
    let current = imports
        .operations
        .get_mut(&operation_id)
        .ok_or_else(|| native_error("native_config_operation_not_found"))?;
    *current = record;
    Ok(())
}

async fn finalize_queued_cancellation(
    state: &AppState,
    operation_id: &str,
) -> Result<NativeConfigOperation> {
    let current = state.refresh_array_runtime_from_disk().await?;
    Ok(mutate_operation_record(state, operation_id, |record| {
        record.operation.status = NativeConfigOperationStatus::Cancelled;
        record.operation.agent_config = native_config_projection(&current);
        record.operation.restart.queued = false;
        record.operation.restart.required = false;
        record.operation.restart.restarted = false;
        record.operation.error = None;
        record.prepared = None;
        record.rollback_snapshots.clear();
        record.prior_config = None;
        record.applied_file_digests.clear();
        record.updated_at = chrono::Utc::now();
        record.phase = NativeConfigOperationPhase::Terminal;
    })
    .await?
    .operation)
}

async fn finalize_applied_cancellation(
    state: &AppState,
    operation_id: &str,
) -> Result<NativeConfigOperation> {
    let record = mutate_operation_record(state, operation_id, |record| {
        record.operation.status = NativeConfigOperationStatus::Cancelled;
        if let Some(prior_config) = record.prior_config.as_ref() {
            record.operation.agent_config = native_config_projection(prior_config);
        }
        record.operation.restart.queued = false;
        record.operation.restart.required = record.prior_was_running;
        record.operation.restart.restarted = record.prior_was_running;
        record.operation.error = None;
        record.prepared = None;
        record.rollback_snapshots.clear();
        record.applied_file_digests.clear();
        record.updated_at = chrono::Utc::now();
        record.phase = NativeConfigOperationPhase::Terminal;
    })
    .await?;
    if record.prior_config.is_none() {
        return Err(native_error("native_config_rollback_failed"));
    }
    Ok(record.operation)
}

async fn persist_operation_record(state: &AppState, operation_id: &str) -> Result<()> {
    let record = state
        .native_config_imports
        .lock()
        .await
        .operations
        .get(operation_id)
        .cloned()
        .ok_or_else(|| native_error("native_config_operation_not_found"))?;
    persist_operation_record_value(state, &record)?;
    if !operation_phase_is_pending(record.phase) {
        spawn_terminal_operation_cleanup(state.clone(), operation_id.to_owned());
    }
    Ok(())
}

fn persist_operation_record_value(
    state: &AppState,
    record: &NativeConfigOperationRecord,
) -> Result<()> {
    persist_native_config_operation(
        &state.runtime_paths.state_path,
        &state.runtime_paths.config_path,
        &home_dir()?,
        record,
    )
}

fn spawn_terminal_operation_cleanup(state: AppState, operation_id: String) {
    tokio::spawn(async move {
        loop {
            let mutation = match state.lock_agent_config_mutation().await {
                Ok(mutation) => mutation,
                Err(error) => {
                    tracing::warn!(error = %error, operation_id, "failed to acquire native config cleanup lock");
                    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                    continue;
                }
            };
            let record = state
                .native_config_imports
                .lock()
                .await
                .operations
                .get(&operation_id)
                .cloned();
            let Some(record) = record else {
                if let Err(error) = remove_native_config_operation_journal(
                    &state.runtime_paths.state_path,
                    &operation_id,
                ) {
                    tracing::warn!(error = %error, operation_id, "failed to remove expired native config import journal");
                }
                return;
            };
            if operation_phase_is_pending(record.phase) {
                return;
            }
            let age = chrono::Utc::now()
                .signed_duration_since(record.updated_at)
                .num_seconds();
            if age < TERMINAL_RETENTION_SECONDS as i64 {
                let remaining = (TERMINAL_RETENTION_SECONDS as i64 - age).max(1) as u64;
                drop(mutation);
                tokio::time::sleep(std::time::Duration::from_secs(remaining)).await;
                continue;
            }
            if let Err(error) = remove_native_config_operation_journal(
                &state.runtime_paths.state_path,
                &operation_id,
            ) {
                tracing::warn!(error = %error, operation_id, "failed to remove expired native config import journal");
                drop(mutation);
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                continue;
            }
            state
                .native_config_imports
                .lock()
                .await
                .operations
                .remove(&operation_id);
            return;
        }
    });
}

pub(crate) async fn recover_native_config_imports(state: &AppState) -> Result<()> {
    let records = load_native_config_operation_journal(
        &state.runtime_paths.state_path,
        &state.runtime_paths.config_path,
        &home_dir()?,
    )?;
    let pending = records
        .iter()
        .filter(|record| operation_phase_is_pending(record.phase))
        .map(|record| (record.updated_at, record.operation.operation_id.clone()))
        .collect::<Vec<_>>();
    if pending.len() > 1 {
        return Err(native_error("native_config_journal_conflict"));
    }
    let terminal = records
        .iter()
        .filter(|record| !operation_phase_is_pending(record.phase))
        .map(|record| record.operation.operation_id.clone())
        .collect::<Vec<_>>();
    {
        let mut imports = state.native_config_imports.lock().await;
        for record in records {
            imports.insert_operation(record);
        }
    }
    for operation_id in terminal {
        spawn_terminal_operation_cleanup(state.clone(), operation_id);
    }
    let mut pending = pending;
    pending.sort();
    for (_, operation_id) in pending {
        match process_pending_operation_once(state, &operation_id).await {
            Ok(ApplyStoredOutcome::Applied(_)) => {}
            Ok(ApplyStoredOutcome::Blocked(_)) => {
                spawn_queued_worker(state.clone(), operation_id);
            }
            Err(error) => {
                let record = operation_record(state, &operation_id).await?;
                if record.phase == NativeConfigOperationPhase::Staged {
                    mark_failed(state, &operation_id, error.error_code()).await?;
                    persist_operation_record(state, &operation_id).await?;
                } else {
                    return Err(error);
                }
            }
        }
    }
    Ok(())
}

fn operation_phase_is_pending(phase: NativeConfigOperationPhase) -> bool {
    matches!(
        phase,
        NativeConfigOperationPhase::Staged
            | NativeConfigOperationPhase::Applying
            | NativeConfigOperationPhase::CancellingQueued
            | NativeConfigOperationPhase::RollingBack
    )
}

async fn process_pending_operation_once(
    state: &AppState,
    operation_id: &str,
) -> Result<ApplyStoredOutcome> {
    match operation_record(state, operation_id).await?.phase {
        NativeConfigOperationPhase::Staged => apply_stored_operation(state, operation_id).await,
        NativeConfigOperationPhase::Applying => {
            let _mutation = state.lock_agent_config_mutation().await?;
            let applying = operation_record(state, operation_id).await?;
            queue_rollback_retry(state, operation_id, "native_config_apply_interrupted").await?;
            if let Err(error) = persist_operation_record(state, operation_id).await {
                replace_operation_record(state, applying).await?;
                return Err(error);
            }
            resume_pending_rollback_locked(state, operation_id).await
        }
        NativeConfigOperationPhase::CancellingQueued => {
            let _mutation = state.lock_agent_config_mutation().await?;
            let marker = operation_record(state, operation_id).await?;
            let operation = finalize_queued_cancellation(state, operation_id).await?;
            if persist_operation_record(state, operation_id).await.is_err() {
                replace_operation_record(state, marker.clone()).await?;
                return Ok(ApplyStoredOutcome::Blocked(marker.operation));
            }
            Ok(ApplyStoredOutcome::Applied(operation))
        }
        NativeConfigOperationPhase::RollingBack => {
            resume_pending_rollback(state, operation_id).await
        }
        NativeConfigOperationPhase::Applied | NativeConfigOperationPhase::Terminal => Ok(
            ApplyStoredOutcome::Applied(operation_record(state, operation_id).await?.operation),
        ),
    }
}

async fn resume_pending_rollback(
    state: &AppState,
    operation_id: &str,
) -> Result<ApplyStoredOutcome> {
    let _mutation = state.lock_agent_config_mutation().await?;
    resume_pending_rollback_locked(state, operation_id).await
}

async fn resume_pending_rollback_locked(
    state: &AppState,
    operation_id: &str,
) -> Result<ApplyStoredOutcome> {
    let marker = operation_record(state, operation_id).await?;
    let prior_config = marker
        .prior_config
        .as_ref()
        .ok_or_else(|| native_error("native_config_journal_invalid"))?;
    if marker.prior_was_running {
        let blockers = {
            let store = state.state.lock().await;
            store.query_restart_blockers(Some(&prior_config.array.primary_target))?
        };
        if !blockers.is_empty() {
            let blocked = mutate_operation_record(state, operation_id, |record| {
                record.operation.status = NativeConfigOperationStatus::Queued;
                record.operation.restart.required = true;
                record.operation.restart.queued = true;
                record.operation.restart.restarted = false;
                record.updated_at = chrono::Utc::now();
            })
            .await?;
            if let Err(error) = persist_operation_record(state, operation_id).await {
                replace_operation_record(state, marker).await?;
                return Err(error);
            }
            return Ok(ApplyStoredOutcome::Blocked(blocked.operation));
        }
    }
    let home = home_dir()?;
    if restore_transaction_and_agent(
        state,
        &marker.rollback_snapshots,
        prior_config,
        marker.prior_was_running,
        &home,
    )
    .await
    .is_err()
    {
        let operation = queue_rollback_retry(state, operation_id, "native_config_rollback_failed")
            .await?
            .operation;
        persist_operation_record(state, operation_id).await?;
        return Ok(ApplyStoredOutcome::Blocked(operation));
    }

    let operation = if marker.cancelled {
        finalize_applied_cancellation(state, operation_id).await?
    } else {
        finalize_failed_rollback(state, operation_id).await?
    };
    if persist_operation_record(state, operation_id).await.is_err() {
        replace_operation_record(state, marker.clone()).await?;
        let retry =
            queue_rollback_retry(state, operation_id, "native_config_journal_persist_failed")
                .await?;
        if persist_operation_record(state, operation_id).await.is_err() {
            tracing::warn!(
                operation_id,
                "failed to persist native config rollback retry after restoring prior files"
            );
        }
        return Ok(ApplyStoredOutcome::Blocked(retry.operation));
    }
    Ok(ApplyStoredOutcome::Applied(operation))
}

async fn finalize_failed_rollback(
    state: &AppState,
    operation_id: &str,
) -> Result<NativeConfigOperation> {
    let record = mutate_operation_record(state, operation_id, |record| {
        record.operation.status = NativeConfigOperationStatus::Failed;
        if let Some(prior_config) = record.prior_config.as_ref() {
            record.operation.agent_config = native_config_projection(prior_config);
        }
        record.operation.restart.queued = false;
        record.operation.restart.required = record.prior_was_running;
        record.operation.restart.restarted = record.prior_was_running;
        record.prepared = None;
        record.rollback_snapshots.clear();
        record.applied_file_digests.clear();
        record.updated_at = chrono::Utc::now();
        record.phase = NativeConfigOperationPhase::Terminal;
    })
    .await?;
    if record.prior_config.is_none() {
        return Err(native_error("native_config_rollback_failed"));
    }
    Ok(record.operation)
}

fn spawn_queued_worker(state: AppState, operation_id: String) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(QUEUED_IMPORT_POLL_SECONDS)).await;
            let active = {
                let imports = state.native_config_imports.lock().await;
                imports
                    .operations
                    .get(&operation_id)
                    .is_some_and(|record| operation_phase_is_pending(record.phase))
            };
            if !active {
                return;
            }
            match process_pending_operation_once(&state, &operation_id).await {
                Ok(ApplyStoredOutcome::Blocked(_)) => {}
                Ok(ApplyStoredOutcome::Applied(_)) => return,
                Err(error) => {
                    let record = match operation_record(&state, &operation_id).await {
                        Ok(record) => record,
                        Err(record_error) => {
                            tracing::warn!(error = %record_error, operation_id, "native config import worker lost its operation");
                            return;
                        }
                    };
                    if record.phase == NativeConfigOperationPhase::Staged {
                        if let Err(mark_error) =
                            mark_failed(&state, &operation_id, error.error_code()).await
                        {
                            tracing::warn!(error = %mark_error, operation_id, "failed to record queued native config import failure");
                            return;
                        }
                        if let Err(persist_error) =
                            persist_operation_record(&state, &operation_id).await
                        {
                            tracing::warn!(error = %persist_error, operation_id, "failed to persist queued native config import failure");
                        }
                        return;
                    }
                    tracing::warn!(error = %error, operation_id, "native config import recovery will retry");
                }
            }
        }
    });
}

fn native_error(code: &'static str) -> StackError {
    StackError::NativeAgentConfig { code }
}
