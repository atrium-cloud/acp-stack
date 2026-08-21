//! Native config operation-record persistence and cancellation finalizers.

use super::*;

pub(super) fn operation_for(
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

pub(super) async fn mark_failed(state: &AppState, operation_id: &str, code: &str) -> Result<()> {
    let current = state.refresh_array_runtime_from_disk().await?;
    let mut imports = state.native_config_imports.lock().await;
    let record = imports
        .operations
        .get_mut(operation_id)
        .ok_or_else(|| native_error("agent.native_config_operation_not_found"))?;
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

pub(super) async fn operation_record(
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
        .ok_or_else(|| native_error("agent.native_config_operation_not_found"))
}

pub(super) async fn mutate_operation_record(
    state: &AppState,
    operation_id: &str,
    mutate: impl FnOnce(&mut NativeConfigOperationRecord),
) -> Result<NativeConfigOperationRecord> {
    let mut imports = state.native_config_imports.lock().await;
    let record = imports
        .operations
        .get_mut(operation_id)
        .ok_or_else(|| native_error("agent.native_config_operation_not_found"))?;
    mutate(record);
    Ok(record.clone())
}

pub(super) async fn replace_operation_record(
    state: &AppState,
    record: NativeConfigOperationRecord,
) -> Result<()> {
    let operation_id = record.operation.operation_id.clone();
    let mut imports = state.native_config_imports.lock().await;
    let current = imports
        .operations
        .get_mut(&operation_id)
        .ok_or_else(|| native_error("agent.native_config_operation_not_found"))?;
    *current = record;
    Ok(())
}

pub(super) async fn finalize_queued_cancellation(
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

pub(super) async fn finalize_applied_cancellation(
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
        return Err(native_error("agent.native_config_rollback_failed"));
    }
    Ok(record.operation)
}

pub(super) async fn persist_operation_record(state: &AppState, operation_id: &str) -> Result<()> {
    let record = state
        .native_config_imports
        .lock()
        .await
        .operations
        .get(operation_id)
        .cloned()
        .ok_or_else(|| native_error("agent.native_config_operation_not_found"))?;
    persist_operation_record_value(state, &record)?;
    if !operation_phase_is_pending(record.phase) {
        spawn_terminal_operation_cleanup(state.clone(), operation_id.to_owned());
    }
    Ok(())
}

pub(super) fn persist_operation_record_value(
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

pub(super) fn spawn_terminal_operation_cleanup(state: AppState, operation_id: String) {
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
