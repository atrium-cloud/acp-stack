//! Native config rollback: restore prior files and runtime after a failed apply.

use super::*;

pub(super) async fn restore_transaction_and_agent(
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
pub(super) async fn rollback_failed_apply(
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
            queue_rollback_retry(state, operation_id, "agent.native_config_rollback_failed")
                .await?;
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
        let retry = queue_rollback_retry(
            state,
            operation_id,
            "agent.native_config_journal_persist_failed",
        )
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

pub(super) async fn queue_rollback_retry(
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

pub(super) async fn resume_pending_rollback(
    state: &AppState,
    operation_id: &str,
) -> Result<ApplyStoredOutcome> {
    let _mutation = state.lock_agent_config_mutation().await?;
    resume_pending_rollback_locked(state, operation_id).await
}

pub(super) async fn resume_pending_rollback_locked(
    state: &AppState,
    operation_id: &str,
) -> Result<ApplyStoredOutcome> {
    let marker = operation_record(state, operation_id).await?;
    let prior_config = marker
        .prior_config
        .as_ref()
        .ok_or_else(|| native_error("agent.native_config_journal_invalid"))?;
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
        let operation =
            queue_rollback_retry(state, operation_id, "agent.native_config_rollback_failed")
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
        let retry = queue_rollback_retry(
            state,
            operation_id,
            "agent.native_config_journal_persist_failed",
        )
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

pub(super) async fn finalize_failed_rollback(
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
        return Err(native_error("agent.native_config_rollback_failed"));
    }
    Ok(record.operation)
}
