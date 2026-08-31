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
        // One ACP session drives one turn at a time: a second prompt would
        // race the live one on the same agent session, and its `session/cancel`
        // would be ambiguous. Reap first so a finished-but-unreaped task does
        // not read as live.
        self.reap_finished().await;
        if !self
            .live_prompts_for_session(session_id, state)
            .await?
            .is_empty()
        {
            return Err(StackError::PromptInFlight {
                session_id: session_id.to_owned(),
            });
        }
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

    /// `POST /v1/sessions/{id}/cancel`. ACP `session/cancel` goes out, the
    /// session's outstanding permission requests are answered `cancelled`, then
    /// the live prompt must actually settle as `cancelled` within
    /// [`PROMPT_CANCEL_SETTLE_BUDGET`] for this to succeed. The agent owns the
    /// turn: firing our own cancellation token here would write `cancelled`
    /// rows for an agent that ignored the notification and is still working.
    pub async fn cancel_session(
        &self,
        session_id: &str,
        state: &Arc<TokioMutex<StateStore>>,
        permissions: &PermissionService,
    ) -> Result<()> {
        // Held only across collect-and-send: a prompt submitted concurrently either
        // lands before the ids are collected or is rejected as in-flight. It is
        // dropped before the settle wait below so a turn that ignores cancellation
        // cannot block prompts, stops, or restarts on the agent's other sessions for
        // the whole settle budget. A submit racing the wait still sees this turn's row
        // as non-terminal and is refused by the in-flight guard, not by this gate.
        let dispatch_guard = self.dispatch_gate.lock().await;
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
        let live_prompts = self.live_prompts_for_session(session_id, state).await?;
        bridge
            .cancel_session(AcpSessionId::new(agent_session_id.clone()))
            .await?;
        drop(dispatch_guard);

        let observed = self
            .await_prompt_settle(session_id, &live_prompts, state, permissions)
            .await?;
        let verdict = cancel_settle_verdict(&observed);
        if verdict != CancelSettleVerdict::Cancelled {
            tracing::warn!(
                session_id,
                verdict = verdict.as_str(),
                prompts = live_prompts.len(),
                "agent did not settle the turn as cancelled"
            );
            // Nothing is torn down on failure: a turn that never settled keeps
            // its handle and its unfired token so a retry can cancel it again,
            // and one that settled on its own terms is already terminal, so
            // later liveness checks skip its handle anyway.
            return Err(StackError::AgentRequestFailed {
                method: "session/cancel",
                message: format!("prompt did not settle as cancelled ({})", verdict.as_str()),
            });
        }
        self.forget_prompts(&live_prompts).await;

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

    /// Prompts for `session_id` this process is still driving: their task has
    /// not finished AND their durable row is not terminal. Both halves matter.
    /// Without the registry check a row orphaned by an earlier crash would look
    /// live forever; without the row check a task parked on an ACP future the
    /// stale-prompt sweeper already wrote off as `stalled` would block every
    /// later submission and cancel on that session until the agent restarts.
    async fn live_prompts_for_session(
        &self,
        session_id: &str,
        state: &Arc<TokioMutex<StateStore>>,
    ) -> Result<Vec<String>> {
        let registered = self.registered_prompts_for_session(session_id).await;
        if registered.is_empty() {
            return Ok(Vec::new());
        }
        let guard = state.lock().await;
        let mut live = Vec::with_capacity(registered.len());
        for prompt_id in registered {
            // A row deleted underneath us belongs to a session teardown, not to
            // a turn anything still has to wait for.
            let Some(record) = guard.get_prompt(&prompt_id)? else {
                continue;
            };
            if !record.status.parse::<PromptStatus>()?.terminal() {
                live.push(prompt_id);
            }
        }
        Ok(live)
    }

    /// Prompt ids for `session_id` whose background task has not finished.
    async fn registered_prompts_for_session(&self, session_id: &str) -> Vec<String> {
        let prompts = self.prompts.lock().await;
        prompts
            .iter()
            .filter(|(_, handle)| handle.session_id == session_id && !handle.join.is_finished())
            .map(|(prompt_id, _)| prompt_id.clone())
            .collect()
    }

    /// Poll each prompt row until it is terminal or the budget expires. `None`
    /// marks a prompt that never settled. The state mutex is taken per pass and
    /// released across the sleep so the prompt task can write its own terminal
    /// row while we wait for it.
    ///
    /// Every pass first answers the session's outstanding
    /// `session/request_permission` calls with the `cancelled` outcome, which
    /// the ACP cancellation contract requires of the client. An agent parked on
    /// a permission it raised cannot end its turn until that answer arrives, so
    /// without the sweep the wait below could only ever time out. It repeats
    /// per pass because the agent may raise a fresh request in the window
    /// between the notification going out and the turn unwinding.
    async fn await_prompt_settle(
        &self,
        session_id: &str,
        prompt_ids: &[String],
        state: &Arc<TokioMutex<StateStore>>,
        permissions: &PermissionService,
    ) -> Result<Vec<Option<PromptStatus>>> {
        let deadline = tokio::time::Instant::now() + PROMPT_CANCEL_SETTLE_BUDGET;
        let mut observed: Vec<Option<PromptStatus>> = vec![None; prompt_ids.len()];
        let mut sweep_failure_logged = false;
        loop {
            // A failed sweep is left to the next pass rather than ending the
            // wait early: the route answers on the settle verdict, and a
            // failure that persists reaches the caller as that verdict once the
            // budget expires. Logged once per wait so a persistent failure
            // does not warn on every 50ms pass.
            if let Err(error) = permissions
                .cancel_pending_for_session(session_id, CANCELLED_SESSION_PERMISSION_REASON)
                .await
                && !sweep_failure_logged
            {
                tracing::warn!(
                    error = %error,
                    session_id,
                    "failed to settle pending permissions while waiting out a session cancel"
                );
                sweep_failure_logged = true;
            }
            {
                let guard = state.lock().await;
                for (slot, prompt_id) in observed.iter_mut().zip(prompt_ids) {
                    if slot.is_some() {
                        continue;
                    }
                    let record =
                        guard
                            .get_prompt(prompt_id)?
                            .ok_or_else(|| StackError::PromptNotFound {
                                id: prompt_id.clone(),
                            })?;
                    let status: PromptStatus = record.status.parse()?;
                    if status.terminal() {
                        *slot = Some(status);
                    }
                }
            }
            if observed.iter().all(Option::is_some) || tokio::time::Instant::now() >= deadline {
                return Ok(observed);
            }
            tokio::time::sleep(PROMPT_CANCEL_SETTLE_POLL_INTERVAL).await;
        }
    }

    /// Drop settled prompts from the registry. Their terminal row and its
    /// companion session event are already durable (both are written under one
    /// state guard, which we had to take to observe the status), so detaching
    /// the task by dropping its `JoinHandle` loses nothing.
    async fn forget_prompts(&self, prompt_ids: &[String]) {
        let mut prompts = self.prompts.lock().await;
        for prompt_id in prompt_ids {
            prompts.remove(prompt_id);
        }
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

/// What the prompt rows observed after `session/cancel` say about the agent's
/// response to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CancelSettleVerdict {
    /// Every live prompt ended as `cancelled`: the agent honored the request.
    Cancelled,
    /// A prompt reached a terminal status that is not `cancelled`, so the turn
    /// ended on the agent's own terms rather than because of the cancel.
    SettledOtherwise,
    /// A prompt was still running when the budget expired.
    TimedOut,
}

impl CancelSettleVerdict {
    fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::SettledOtherwise => "settled_otherwise",
            Self::TimedOut => "timed_out",
        }
    }
}

/// Decide a cancel from the statuses observed for its live prompts, where
/// `None` is a prompt that never reached a terminal status. No live prompts
/// means the notification had nothing to interrupt, which is a success: the
/// route is idempotent.
fn cancel_settle_verdict(observed: &[Option<PromptStatus>]) -> CancelSettleVerdict {
    if observed.iter().any(Option::is_none) {
        return CancelSettleVerdict::TimedOut;
    }
    if observed
        .iter()
        .all(|status| *status == Some(PromptStatus::Cancelled))
    {
        CancelSettleVerdict::Cancelled
    } else {
        CancelSettleVerdict::SettledOtherwise
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cancel_with_no_live_prompt_succeeds() {
        assert_eq!(cancel_settle_verdict(&[]), CancelSettleVerdict::Cancelled);
    }

    #[test]
    fn every_live_prompt_must_end_cancelled() {
        assert_eq!(
            cancel_settle_verdict(&[Some(PromptStatus::Cancelled), Some(PromptStatus::Cancelled)]),
            CancelSettleVerdict::Cancelled
        );
    }

    #[test]
    fn a_turn_the_agent_finished_on_its_own_terms_is_not_a_cancel() {
        for status in [
            PromptStatus::Completed,
            PromptStatus::Errored,
            PromptStatus::Stalled,
        ] {
            assert_eq!(
                cancel_settle_verdict(&[Some(status)]),
                CancelSettleVerdict::SettledOtherwise,
                "{} must not read as a cancel",
                status.as_str()
            );
        }
    }

    #[test]
    fn a_prompt_that_never_settled_outranks_its_cancelled_siblings() {
        assert_eq!(
            cancel_settle_verdict(&[Some(PromptStatus::Cancelled), None]),
            CancelSettleVerdict::TimedOut
        );
    }
}
