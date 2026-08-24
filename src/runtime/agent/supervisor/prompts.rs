//! Prompt submission, cancellation, and the in-flight prompt registry.

use super::*;

impl AgentSupervisor {
    /// `POST /v1/sessions/{id}/prompt`. Fire-and-forget: inserts a `pending`
    /// row, spawns the task that drives ACP `session/prompt`, and returns the
    /// prompt id immediately for clients to poll.
    pub async fn submit_prompt(
        &self,
        session_id: &str,
        prompt_blocks: Vec<ContentBlock>,
        prompt_json: String,
        state: &Arc<TokioMutex<StateStore>>,
    ) -> Result<PromptRecord> {
        let _dispatch_guard = self.dispatch_gate.lock().await;
        let bridge = self.bridge().await?;
        let agent_session_id = {
            let guard = state.lock().await;
            let session =
                guard
                    .get_session(session_id)?
                    .ok_or_else(|| StackError::SessionNotFound {
                        id: session_id.to_owned(),
                    })?;
            if session.status == SESSION_STATUS_CLOSED {
                return Err(StackError::SessionClosed {
                    id: session_id.to_owned(),
                });
            }
            if session.status != SESSION_STATUS_ACTIVE {
                return Err(StackError::SessionNotActive {
                    id: session_id.to_owned(),
                    status: session.status,
                });
            }
            session.agent_session_id
        };
        let prompt_id = next_prompt_id();
        let message_id = next_prompt_message_id();
        let record = {
            let guard = state.lock().await;
            guard.insert_prompt_with_message_id(
                NewPromptRecord {
                    id: prompt_id.clone(),
                    session_id: session_id.to_owned(),
                    prompt_json,
                },
                Some(message_id.clone()),
            )?
        };

        let cancel = CancellationToken::new();
        let cancel_inner = cancel.clone();
        let state_clone = state.clone();
        let session_id_owned = session_id.to_owned();
        let prompt_id_owned = prompt_id.clone();
        let message_id_owned = message_id.clone();
        let acp_request = PromptRequest::new(AcpSessionId::new(agent_session_id), prompt_blocks)
            .meta(prompt_message_id_meta(&message_id));

        let join = tokio::spawn(async move {
            // A failed `pending -> running` flip is survivable: the row stays
            // `pending` and settle still writes the terminal status.
            {
                let guard = state_clone.lock().await;
                if let Err(err) = guard.update_prompt_status(
                    &prompt_id_owned,
                    PromptStatus::Running,
                    None,
                    None,
                    None,
                    None,
                    None,
                ) {
                    tracing::warn!(error = %err, prompt_id = %prompt_id_owned, "failed to mark prompt running");
                }
            }

            let bridge_call = bridge.prompt_session(acp_request);
            let outcome = tokio::select! {
                result = bridge_call => Outcome::Settled(result),
                _ = cancel_inner.cancelled() => Outcome::Cancelled,
            };
            if let Outcome::Settled(Ok(response)) = &outcome
                && meta_message_id(response.meta.as_ref()) == Some(message_id_owned.as_str())
            {
                let guard = state_clone.lock().await;
                if let Err(err) =
                    guard.acknowledge_prompt_message_id(&prompt_id_owned, &message_id_owned)
                {
                    tracing::warn!(
                        error = %err,
                        prompt_id = %prompt_id_owned,
                        message_id = %message_id_owned,
                        "failed to acknowledge prompt message id"
                    );
                }
            }

            // The session-event emit must follow the row write so subscribers
            // observe consistent SQL state.
            let terminal = build_terminal_outcome_with_prompt_id(outcome, Some(&prompt_id_owned));

            {
                let guard = state_clone.lock().await;
                let status_updated = match guard.update_prompt_status(
                    &prompt_id_owned,
                    terminal.status,
                    terminal.stop_reason.as_deref(),
                    terminal.error_code.as_deref(),
                    terminal.error_message.as_deref(),
                    terminal.failure_class,
                    terminal.failure_detail_json.as_deref(),
                ) {
                    Ok(updated) => updated,
                    Err(err) => {
                        tracing::warn!(
                            error = %err,
                            prompt_id = %prompt_id_owned,
                            "failed to record terminal prompt status"
                        );
                        false
                    }
                };
                if !status_updated {
                    tracing::warn!(
                        prompt_id = %prompt_id_owned,
                        terminal_status = %terminal.status.as_str(),
                        "skipping terminal prompt event because prompt row was already terminal"
                    );
                } else if let Some(event) = terminal.session_event.as_ref()
                    && let Err(err) = guard.append_session_event_with_source(
                        &session_id_owned,
                        event.level,
                        event.kind,
                        EVENT_SOURCE_SYSTEM,
                        event.message,
                        &event.payload_json,
                    )
                {
                    tracing::warn!(
                        error = %err,
                        prompt_id = %prompt_id_owned,
                        session_id = %session_id_owned,
                        event_kind = event.kind,
                        "failed to record terminal prompt session event"
                    );
                }
            }
        });

        self.prompts.lock().await.insert(
            prompt_id.clone(),
            PromptHandle {
                cancel,
                join,
                session_id: session_id.to_owned(),
            },
        );
        self.reap_finished().await;
        Ok(record)
    }

    /// `POST /v1/sessions/{id}/cancel`. ACP `session/cancel` goes out FIRST;
    /// local cancellation tokens fire only on its success, so a failed bridge
    /// call cannot leave rows `cancelled` while the agent keeps running.
    pub async fn cancel_session(
        &self,
        session_id: &str,
        state: &Arc<TokioMutex<StateStore>>,
    ) -> Result<()> {
        let bridge = self.bridge().await?;
        let agent_session_id = {
            let guard = state.lock().await;
            let record =
                guard
                    .get_session(session_id)?
                    .ok_or_else(|| StackError::SessionNotFound {
                        id: session_id.to_owned(),
                    })?;
            record.agent_session_id
        };
        bridge
            .cancel_session(AcpSessionId::new(agent_session_id.clone()))
            .await?;
        self.cancel_prompts_for_session(session_id).await;
        let guard = state.lock().await;
        guard.append_session_event(
            session_id,
            "info",
            "session.cancel_requested",
            "cancel requested",
            &json!({ "agent_session_id": agent_session_id }).to_string(),
        )?;
        Ok(())
    }

    pub(super) async fn cancel_prompts_for_session(&self, session_id: &str) {
        let prompts = self.prompts.lock().await;
        for handle in prompts.values() {
            if handle.session_id == session_id {
                handle.cancel.cancel();
            }
        }
    }

    pub(super) async fn cancel_all_prompts(&self) {
        // Drain handles out of the map before awaiting: the tasks may
        // re-enter the registry via `reap_finished` from other paths.
        let handles: Vec<PromptHandle> = {
            let mut prompts = self.prompts.lock().await;
            prompts.drain().map(|(_, handle)| handle).collect()
        };
        for handle in &handles {
            handle.cancel.cancel();
        }
        // Await each task so terminal `prompts` rows are written before
        // shutdown returns, bounded so a stuck task cannot delay teardown.
        let deadline = tokio::time::Instant::now() + PROMPT_DRAIN_BUDGET;
        for handle in handles {
            let PromptHandle { join, .. } = handle;
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(remaining, join).await {
                Ok(Ok(())) => {}
                Ok(Err(err)) => {
                    tracing::warn!(error = ?err, "prompt task panicked during drain");
                }
                Err(_) => {
                    // Dropping the JoinHandle detaches the already-cancelled
                    // task; the imminent bridge teardown makes it settle.
                    tracing::warn!("prompt task did not settle within drain budget");
                }
            }
        }
    }

    async fn reap_finished(&self) {
        let mut prompts = self.prompts.lock().await;
        prompts.retain(|_, handle| !handle.join.is_finished());
    }
}
