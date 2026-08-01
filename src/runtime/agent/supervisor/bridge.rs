//! Bridge spawn and exit monitoring for the agent supervisor.
//!
//! Spawning owns the pre-spawn integrity guard and the `agent.starting` /
//! `agent.started` / `agent.spawn_failed` lifecycle rows. Exit monitoring owns
//! the crash-detection loop and the `on-crash` restart path.

use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) async fn spawn_agent_bridge(
    target_id: &str,
    agent: &AgentConfig,
    workspace_root: &str,
    env: HashMap<String, String>,
    state: &Arc<TokioMutex<StateStore>>,
    session_changes: &SessionChangesHandle,
    event_hub: EventHub,
    permissions: Option<crate::runtime::mediation::permissions::PermissionService>,
    sandbox: crate::config::SandboxConfig,
    network_provider: Option<crate::extensions::NetworkProviderExtension>,
) -> Result<(AgentCapabilitiesDto, AcpBridge)> {
    let cwd = resolve_agent_cwd(agent, workspace_root);

    // Enforce the optional integrity guard BEFORE spawning. The installer
    // already hashes `[agent.install].creates`, but `[agent].command` may
    // resolve to a different binary (different path, or replaced on disk
    // between install and start).
    if let Some(expected) = agent.expected_sha256.as_deref() {
        verify_agent_binary_sha256(&agent.command, &cwd, expected)?;
    }

    append_and_publish_agent_lifecycle(
        state,
        &event_hub,
        "agent.starting",
        "starting acp agent",
        json!({
            "target_id": target_id,
            "agent_id": agent.id,
            "command": agent.command,
            "adapter": agent.adapter,
        }),
    )
    .await?;

    let sink: Arc<dyn SessionEventSink> = Arc::new(StateStoreSessionSink::with_session_changes(
        target_id.to_owned(),
        state.clone(),
        session_changes.clone(),
    ));
    let bridge = match AcpBridge::spawn(
        agent,
        env,
        cwd,
        sink,
        permissions.into(),
        &sandbox,
        network_provider.as_ref(),
        Some(crate::runtime::agent::acp_bridge::TerminalCommandLog {
            state: state.clone(),
            event_hub: event_hub.clone(),
        }),
    )
    .await
    {
        Ok(bridge) => bridge,
        Err(err) => {
            let data = json!({
                "target_id": target_id,
                "agent_id": agent.id,
                "reason": err.to_string(),
            });
            if let Err(persist_err) = append_and_publish_agent_lifecycle(
                state,
                &event_hub,
                "agent.spawn_failed",
                "agent spawn failed",
                data,
            )
            .await
            {
                tracing::warn!(error = %persist_err, "failed to record agent.spawn_failed lifecycle row");
            }
            return Err(err);
        }
    };

    let capabilities = bridge.capabilities().clone();
    let pid = bridge.pid();
    let caps_json = capabilities.to_json()?;

    let started_data = json!({
        "target_id": target_id,
        "agent_id": agent.id,
        "pid": pid,
        "adapter": agent.adapter,
    });
    let started_row_result: Result<crate::state::AgentLifecycleEvent> = {
        let guard = state.lock().await;
        (|| {
            guard.upsert_agent_capabilities(&agent.id, &caps_json)?;
            guard.append_agent_lifecycle(
                "agent.started",
                "agent initialized",
                &started_data.to_string(),
            )
        })()
    };
    match started_row_result {
        Ok(row) => {
            event_hub.publish_agent_event(&row.id, &row.created_at, "agent.started", started_data);
        }
        Err(err) => {
            if let Err(shutdown_err) = bridge.shutdown().await {
                tracing::warn!(
                    error = %shutdown_err,
                    "agent bridge shutdown after persist failure also failed"
                );
            }
            return Err(err);
        }
    }

    Ok((capabilities, bridge))
}

pub(super) fn spawn_bridge_exit_monitor(
    shared: SupervisorShared,
    bridge: Arc<AcpBridge>,
    restart_context: RestartContext,
) {
    tokio::spawn(async move {
        if let Err(err) = monitor_bridge_exit(shared, bridge, restart_context).await {
            tracing::warn!(error = %err, "agent supervisor: bridge exit monitor failed");
        }
    });
}

async fn monitor_bridge_exit(
    shared: SupervisorShared,
    bridge: Arc<AcpBridge>,
    restart_context: RestartContext,
) -> Result<()> {
    let Some(exit) = wait_for_bridge_exit(&bridge).await else {
        return Ok(());
    };
    if exit.planned {
        return Ok(());
    }

    let was_current_running_bridge = {
        let mut guard = shared.state.lock().await;
        match &*guard {
            AgentState::Running(current) if Arc::ptr_eq(current, &bridge) => {
                *guard = AgentState::Stopped;
                true
            }
            _ => false,
        }
    };
    if !was_current_running_bridge {
        return Ok(());
    }
    *shared.last_pid.write().await = None;
    *shared.loaded_providers.write().await = None;

    let exit_status = match bridge.shutdown().await {
        Ok(status) => status,
        Err(err) => {
            tracing::warn!(error = %err, "agent supervisor: failed to reap crashed agent bridge");
            None
        }
    };
    let restart_policy = restart_context.agent.restart.as_str();
    append_and_publish_agent_lifecycle(
        &restart_context.state_store,
        &restart_context.event_hub,
        "agent.exited",
        "agent exited unexpectedly",
        bridge_exit_payload(
            &restart_context.target_id,
            &restart_context.agent.id,
            restart_policy,
            &exit,
            exit_status,
        ),
    )
    .await?;

    if restart_policy != "on-crash" {
        append_and_publish_agent_lifecycle(
            &restart_context.state_store,
            &restart_context.event_hub,
            "agent.restart_skipped",
            "agent restart skipped",
            json!({
                "target_id": restart_context.target_id,
                "agent_id": restart_context.agent.id,
                "restart": restart_policy,
                "reason": "restart policy is not on-crash",
            }),
        )
        .await?;
        return Ok(());
    }

    let backoff_ms = u64::try_from(AGENT_CRASH_RESTART_BACKOFF.as_millis()).unwrap_or(u64::MAX);
    append_and_publish_agent_lifecycle(
        &restart_context.state_store,
        &restart_context.event_hub,
        "agent.restart_scheduled",
        "agent restart scheduled",
        json!({
            "target_id": restart_context.target_id,
            "agent_id": restart_context.agent.id,
            "restart": restart_policy,
            "backoff_ms": backoff_ms,
        }),
    )
    .await?;
    tokio::time::sleep(AGENT_CRASH_RESTART_BACKOFF).await;

    {
        let mut guard = shared.state.lock().await;
        match &*guard {
            AgentState::Stopped => {
                *guard = AgentState::Starting;
            }
            AgentState::Starting
            | AgentState::Running(_)
            | AgentState::Stopping
            | AgentState::Updating => {
                tracing::debug!(
                    agent_id = %restart_context.agent.id,
                    "agent supervisor: automatic restart abandoned because state changed"
                );
                return Ok(());
            }
        }
    }

    match spawn_agent_bridge(
        &restart_context.target_id,
        &restart_context.agent,
        &restart_context.workspace_root,
        restart_context.env.clone(),
        &restart_context.state_store,
        &restart_context.session_changes,
        restart_context.event_hub.clone(),
        restart_context.permissions.clone(),
        restart_context.sandbox.clone(),
        restart_context.network_provider.clone(),
    )
    .await
    {
        Ok((capabilities, new_bridge)) => {
            let pid = new_bridge.pid();
            let new_bridge = Arc::new(new_bridge);
            {
                let mut guard = shared.state.lock().await;
                *guard = AgentState::Running(Arc::clone(&new_bridge));
            }
            *shared.capabilities.write().await = Some(capabilities);
            *shared.last_pid.write().await = pid;
            *shared.loaded_providers.write().await = Some(restart_context.providers.clone());
            spawn_bridge_exit_monitor(shared, new_bridge, restart_context);
        }
        Err(err) => {
            {
                let mut guard = shared.state.lock().await;
                *guard = AgentState::Stopped;
            }
            *shared.last_pid.write().await = None;
            *shared.loaded_providers.write().await = None;
            append_and_publish_agent_lifecycle(
                &restart_context.state_store,
                &restart_context.event_hub,
                "agent.restart_failed",
                "agent restart failed",
                json!({
                    "target_id": restart_context.target_id,
                    "agent_id": restart_context.agent.id,
                    "reason": err.to_string(),
                }),
            )
            .await?;
        }
    }

    Ok(())
}

async fn wait_for_bridge_exit(bridge: &AcpBridge) -> Option<AcpBridgeExit> {
    let exit_rx = bridge.subscribe_exit();
    loop {
        if let Some(exit) = exit_rx.borrow().clone() {
            return Some(exit);
        }
        match bridge.try_wait_child().await {
            Ok(Some(exit_status)) => {
                return Some(AcpBridgeExit {
                    pid: bridge.pid(),
                    planned: bridge.planned_shutdown(),
                    reason: AcpBridgeExitReason::ProcessExited,
                    message: None,
                    exit_status: Some(exit_status),
                });
            }
            Ok(None) => {}
            Err(err) => {
                tracing::warn!(error = %err, "agent supervisor: child exit poll failed");
            }
        }
        tokio::time::sleep(AGENT_EXIT_POLL_INTERVAL).await;
    }
}

fn bridge_exit_payload(
    target_id: &str,
    agent_id: &str,
    restart_policy: &str,
    exit: &AcpBridgeExit,
    exit_status: Option<i32>,
) -> Value {
    json!({
        "target_id": target_id,
        "agent_id": agent_id,
        "pid": exit.pid,
        "planned": exit.planned,
        "reason": exit.reason.as_str(),
        "message": exit.message,
        "exit_status": exit.exit_status.or(exit_status),
        "restart": restart_policy,
    })
}

pub(super) async fn append_and_publish_agent_lifecycle(
    state: &Arc<TokioMutex<StateStore>>,
    event_hub: &EventHub,
    event_kind: &str,
    message: &str,
    data: Value,
) -> Result<()> {
    let payload = data.to_string();
    let row = {
        let guard = state.lock().await;
        guard.append_agent_lifecycle(event_kind, message, &payload)?
    };
    event_hub.publish_agent_event(&row.id, &row.created_at, event_kind, data);
    Ok(())
}
