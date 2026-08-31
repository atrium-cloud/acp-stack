//! Native config apply pipeline: staged operations become live runtime state.

use super::*;

pub(super) enum ApplyStoredOutcome {
    Applied(NativeConfigOperation),
    Blocked(NativeConfigOperation),
}

pub(super) async fn apply_stored_operation(
    state: &AppState,
    operation_id: &str,
) -> Result<ApplyStoredOutcome> {
    let _mutation = state.lock_agent_config_mutation().await?;
    apply_stored_operation_locked(state, operation_id).await
}

pub(super) async fn apply_stored_operation_locked(
    state: &AppState,
    operation_id: &str,
) -> Result<ApplyStoredOutcome> {
    let (prepared, phase, stored_prior, stored_snapshots, stored_prior_was_running) = {
        let imports = state.native_config_imports.lock().await;
        let record = imports
            .operations
            .get(operation_id)
            .ok_or_else(|| native_error("agent.native_config_operation_not_found"))?;
        if record.cancelled {
            return Ok(ApplyStoredOutcome::Blocked(record.operation.clone()));
        }
        (
            record
                .prepared
                .clone()
                .ok_or_else(|| native_error("agent.native_config_operation_invalid"))?,
            record.phase,
            record.prior_config.clone(),
            record.rollback_snapshots.clone(),
            record.prior_was_running,
        )
    };
    let home = state.runtime_paths.home.clone();
    validate_native_config_secret_refs_read_only(&prepared, &home)?;
    let resuming_apply = phase == NativeConfigOperationPhase::Applying;
    let prior_config = if resuming_apply {
        stored_prior.ok_or_else(|| native_error("agent.native_config_journal_invalid"))?
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
            return Err(native_error("agent.native_config_base_config_changed"));
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
            return Err(native_error("agent.native_config_journal_invalid"));
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
                .ok_or_else(|| native_error("agent.native_config_operation_not_found"))?;
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
                    .ok_or_else(|| native_error("agent.native_config_operation_not_found"))?;
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
        .ok_or_else(|| native_error("agent.native_config_operation_not_found"))?;
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

pub(super) async fn apply_files_and_runtime(
    state: &AppState,
    prepared: &PreparedNativeConfigImport,
    restart: bool,
) -> Result<()> {
    write_native_config_files(
        prepared,
        &state.runtime_paths.config_path,
        &state.runtime_paths.home,
    )?;
    if prepared.imported_model
        && !model_value_is_explicit_without_discovery(&prepared.canonical_config.agent)
    {
        let model = native_config_projection(&prepared.canonical_config)
            .model
            .ok_or_else(|| native_error("agent.native_config_model_invalid"))?;
        let response = fetch_session_config_with_timeout(
            &state.runtime_paths.home,
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

pub(super) async fn start_agent_for_config(state: &AppState, config: &Config) -> Result<()> {
    let target_id = config.array.primary_target.clone();
    ensure_array_process_start_allowed(config, &target_id)?;
    let target = state.agent_target(&target_id)?;
    let environment = open_agent_environment(&state.runtime_paths.home, config)?;
    *target.live_agent_config.lock().await = config.agent.clone();
    target
        .supervisor
        .start(AgentStartRequest {
            target_id: &target_id,
            agent: &config.agent,
            workspace_root: &config.workspace.root,
            home: state.runtime_paths.home.clone(),
            env: environment.env,
            providers: environment.providers,
            state: &state.state,
            session_changes: &state.session_changes,
            event_hub: state.event_hub.clone(),
            permissions: Some(state.permissions.clone()),
            sandbox: config.workspace.sandbox.clone(),
            shell: config.workspace.default_shell.clone(),
            network_provider: crate::extensions::resolve_network_provider(config),
        })
        .await?;
    Ok(())
}
