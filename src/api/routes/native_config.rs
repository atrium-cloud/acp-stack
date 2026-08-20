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

mod apply;
mod record;
mod rollback;

// Cross-seam helpers keep `pub(super)` visibility; re-import them here so each
// sibling's `use super::*;` resolves items defined in the other siblings.
use self::apply::*;
use self::record::*;
use self::rollback::*;

const QUEUED_IMPORT_POLL_SECONDS: u64 = 2;

#[derive(Deserialize, schemars::JsonSchema)]
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
