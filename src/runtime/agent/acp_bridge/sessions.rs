//! Session RPC methods dispatched over the live ACP connection, plus the
//! prompt-failure mapping they share.

use super::*;

use std::time::Instant;

use agent_client_protocol::Error as AcpError;
use agent_client_protocol::{JsonRpcRequest, RequestCancellationHandle};
use serde_json::Value;

use crate::error::{AcpErrorDetail, AgentRequestContext};
use crate::redaction::{bounded, redact_json, redact_text};
use crate::tracing_init::{LATENCY_TARGET, latency_lines_enabled};

// CONSTANTS

/// Cap on the adapter's error message and, separately, on its serialized `data`, as carried
/// into the API error details and the daemon log.
const ACP_ERROR_DETAIL_MAX_BYTES: usize = 2048;

/// Longest a whole paginated `session/list` may take. The list sync holds a single-flight
/// gate across it, so one reply the agent never sends must not stall every later sync.
const SESSION_LIST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// A resolved breakpoint for `session/fork`, already translated into the
/// dialect the running adapter reads. The supervisor owns the translation
/// because only it can map an acp-stack prompt message id onto the adapter's
/// own transcript ids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForkPoint {
    /// `_meta.acpStack.messageId`, carrying acp-stack's prompt message id
    /// verbatim. The in-house adapters cut just before that prompt themselves.
    AcpStackMessageId(String),
    /// `_meta.jetbrains.air.fork`, carrying an adapter-emitted message id. Both
    /// vendor adapters keep the named point, so the id named here is the one
    /// from the turn before the prompt the client asked to cut at.
    AirMessageId(String),
}

/// Asks the agent to cancel a session-opening request whose caller stopped waiting, as
/// dropping an awaited request does; the ordered callback owns the request itself.
struct CancelUnansweredRequest(Option<RequestCancellationHandle>);

impl CancelUnansweredRequest {
    fn disarm(mut self) {
        self.0 = None;
    }
}

impl Drop for CancelUnansweredRequest {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take()
            && let Err(error) = handle.cancel()
        {
            tracing::debug!(error = %error, "could not ask the agent to cancel an abandoned session open");
        }
    }
}

impl AcpBridge {
    pub(super) async fn connection(&self) -> Result<ConnectionTo<Agent>> {
        let guard = self.connection.lock().await;
        guard.as_ref().cloned().ok_or(StackError::AgentNotRunning)
    }

    /// Send one non-prompt session request and log how long the agent took to answer on the
    /// latency target, so slow session RPCs are visible without the opt-in frame trace.
    async fn timed_request<Request: JsonRpcRequest>(
        &self,
        connection: &ConnectionTo<Agent>,
        method: &'static str,
        acp_session_id: Option<&str>,
        request: Request,
    ) -> std::result::Result<Request::Response, AcpError> {
        let started_at = Instant::now();
        let response = connection.send_request(request).block_task().await;
        if latency_lines_enabled() {
            tracing::info!(
                target: LATENCY_TARGET,
                method,
                elapsed_ms = u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
                ok = response.is_ok(),
                agent_id = %self.agent_id,
                target_id = self.target_id.as_deref().unwrap_or_default(),
                acp_session_id = acp_session_id.unwrap_or_default(),
                "agent request answered"
            );
        }
        response
    }

    /// Send a request that opens a session (`session/new`, `session/fork`). The agent may
    /// stream updates for the new session right after answering, before the caller writes
    /// its local row, so the session is registered with the notification worker inside the
    /// SDK's ordered response callback: the dispatch loop releases no later message until
    /// the callback returns, and every later update for the session is held.
    async fn open_session_request<Request>(
        &self,
        connection: &ConnectionTo<Agent>,
        method: &'static str,
        acp_session_id: Option<&str>,
        request: Request,
        opened_session_id: fn(&Request::Response) -> &SessionId,
    ) -> Result<(Request::Response, PendingSessionUpdates)>
    where
        Request: JsonRpcRequest,
        Request::Response: Send + 'static,
    {
        let started_at = Instant::now();
        let holds = self.sink.holds_new_session_updates();
        let queue = self.notification_queue.clone();
        let (answer_tx, answer_rx) = oneshot::channel();
        // Prepared rather than sent: ordering must be selected before publication, or a
        // fast answer routed in between releases the next update ahead of `Expect`.
        let prepared = connection.prepare_request(request);
        let cancel_on_drop = CancelUnansweredRequest(Some(prepared.cancellation_handle()));
        prepared
            .on_receiving_result(move |result| {
                let pending = match &result {
                    Ok(response) if holds => {
                        let agent_session_id = opened_session_id(response).0.to_string();
                        let expect = SessionQueueItem::Expect {
                            agent_session_id: agent_session_id.clone(),
                        };
                        if queue.send(expect).is_err() {
                            tracing::debug!(
                                agent_session_id,
                                "session/update worker already stopped; nothing to hold"
                            );
                        }
                        PendingSessionUpdates::held(queue, agent_session_id)
                    }
                    _ => PendingSessionUpdates::inert(),
                };
                // A caller that stopped waiting drops the claim here, which abandons
                // whatever the worker holds for the session.
                if answer_tx.send((result, pending)).is_err() {
                    tracing::debug!(method, "session open caller stopped waiting for the answer");
                }
                std::future::ready(Ok(()))
            })
            .map_err(|err| self.request_failed(method, acp_session_id, err))?;
        let answer = answer_rx.await;
        cancel_on_drop.disarm();
        if latency_lines_enabled() {
            tracing::info!(
                target: LATENCY_TARGET,
                method,
                elapsed_ms = u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
                ok = matches!(answer, Ok((Ok(_), _))),
                agent_id = %self.agent_id,
                target_id = self.target_id.as_deref().unwrap_or_default(),
                acp_session_id = acp_session_id.unwrap_or_default(),
                "agent request answered"
            );
        }
        // The SDK skips the callback only when no response arrives at all.
        let Ok((result, pending)) = answer else {
            return Err(self.local_request_failure(
                method,
                acp_session_id,
                "agent connection closed before the agent answered".to_owned(),
            ));
        };
        let response = result.map_err(|err| self.request_failed(method, acp_session_id, err))?;
        Ok((response, pending))
    }

    fn request_context(&self, acp_session_id: Option<&str>) -> AgentRequestContext {
        AgentRequestContext {
            agent_id: Some(self.agent_id.clone()),
            target_id: self.target_id.clone(),
            acp_session_id: acp_session_id.map(str::to_owned),
            acp_error: None,
        }
    }

    /// `AgentRequestFailed` for a failure acp-stack detected itself, naming the agent and session.
    fn local_request_failure(
        &self,
        method: &'static str,
        acp_session_id: Option<&str>,
        message: String,
    ) -> StackError {
        StackError::AgentRequestFailed {
            method,
            message,
            context: Box::new(self.request_context(acp_session_id)),
        }
    }

    /// `AgentRequestFailed` carrying the adapter's JSON-RPC error, redacted and bounded.
    fn request_failed(
        &self,
        method: &'static str,
        acp_session_id: Option<&str>,
        error: AcpError,
    ) -> StackError {
        // The SDK answers a pending request itself when the adapter's stdout closes; that error
        // is not the adapter's reply, so it must not read as the adapter refusing the request.
        if agent_client_protocol::is_incoming_transport_closed(&error) {
            return self.local_request_failure(
                method,
                acp_session_id,
                "agent connection closed before the agent answered".to_owned(),
            );
        }
        let detail = acp_error_detail(error);
        StackError::AgentRequestFailed {
            method,
            message: detail.message.clone(),
            context: Box::new(AgentRequestContext {
                acp_error: Some(detail),
                ..self.request_context(acp_session_id)
            }),
        }
    }

    /// `session/new`. Always supported per ACP baseline. The returned claim holds the
    /// session's updates until the caller releases them under its local row.
    pub async fn new_session(
        &self,
        cwd: PathBuf,
        mcp_servers: Vec<McpServer>,
    ) -> Result<(NewSessionResponse, PendingSessionUpdates)> {
        self.capabilities
            .reject_unmodeled_mcp_servers(&mcp_servers)?;
        let connection = self.connection().await?;
        let mut request = NewSessionRequest::new(cwd);
        request.mcp_servers = mcp_servers;
        let (response, pending) = self
            .open_session_request(&connection, "session/new", None, request, |response| {
                &response.session_id
            })
            .await?;
        self.mark_session_attached(response.session_id.0.as_ref())
            .await;
        Ok((response, pending))
    }

    pub async fn fork_session(
        &self,
        session_id: SessionId,
        cwd: PathBuf,
        mcp_servers: Vec<McpServer>,
        fork_point: Option<ForkPoint>,
    ) -> Result<(ForkSessionResponse, PendingSessionUpdates)> {
        if !self.capabilities.supports_fork_session() {
            return Err(StackError::AgentUnsupportedCapability {
                name: "session/fork",
            });
        }
        // Only the acp-stack dialect is advertised as a sub-capability. An AIR
        // fork point is reachable on the catalog declaration alone, which the
        // supervisor already resolved before building this fork point.
        if matches!(fork_point, Some(ForkPoint::AcpStackMessageId(_)))
            && !self.capabilities.supports_fork_message_id()
        {
            return Err(StackError::AgentUnsupportedCapability {
                name: "session/fork.messageId",
            });
        }
        self.capabilities
            .reject_unmodeled_mcp_servers(&mcp_servers)?;
        let connection = self.connection().await?;
        let source_session_id = session_id.0.to_string();
        let mut request = ForkSessionRequest::new(session_id, cwd).mcp_servers(mcp_servers);
        match &fork_point {
            Some(ForkPoint::AcpStackMessageId(message_id)) => {
                request = request.meta(prompt_message_id_meta(message_id));
            }
            Some(ForkPoint::AirMessageId(message_id)) => {
                request = request.meta(air_fork_point_meta(message_id));
            }
            None => {}
        }
        let (response, pending) = self
            .open_session_request(
                &connection,
                "session/fork",
                Some(&source_session_id),
                request,
                |response: &ForkSessionResponse| &response.session_id,
            )
            .await?;
        self.mark_session_attached(response.session_id.0.as_ref())
            .await;
        Ok((response, pending))
    }

    /// `session/list`. Requires the `sessionCapabilities.list` capability.
    pub async fn list_sessions(&self) -> Result<Vec<SessionInfo>> {
        if !self.capabilities.supports_list_sessions() {
            return Err(StackError::AgentUnsupportedCapability {
                name: "session/list",
            });
        }
        match tokio::time::timeout(SESSION_LIST_TIMEOUT, self.list_all_session_pages()).await {
            Ok(listed) => listed,
            Err(_) => Err(self.local_request_failure(
                "session/list",
                None,
                format!(
                    "agent did not finish answering session/list within {}s",
                    SESSION_LIST_TIMEOUT.as_secs()
                ),
            )),
        }
    }

    async fn list_all_session_pages(&self) -> Result<Vec<SessionInfo>> {
        let connection = self.connection().await?;
        let started_at = Instant::now();
        let mut pages = 0_u32;
        let mut sessions = Vec::new();
        let mut cursor = None;
        let mut seen_cursors = HashSet::new();
        loop {
            let request = ListSessionsRequest::new().cursor(cursor.clone());
            let response: ListSessionsResponse = self
                .timed_request(&connection, "session/list", None, request)
                .await
                .map_err(|err| self.request_failed("session/list", None, err))?;
            pages += 1;
            sessions.extend(response.sessions);
            let Some(next_cursor) = response.next_cursor else {
                if pages > 1 && latency_lines_enabled() {
                    tracing::info!(
                        target: LATENCY_TARGET,
                        method = "session/list",
                        elapsed_ms =
                            u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
                        pages,
                        sessions = sessions.len(),
                        agent_id = %self.agent_id,
                        target_id = self.target_id.as_deref().unwrap_or_default(),
                        "agent session list paged through"
                    );
                }
                return Ok(sessions);
            };
            if !seen_cursors.insert(next_cursor.clone()) {
                return Err(self.local_request_failure(
                    "session/list",
                    None,
                    format!("agent returned repeated pagination cursor `{next_cursor}`"),
                ));
            }
            cursor = Some(next_cursor);
        }
    }

    /// Typed lane for mode/model/effort, which are always `ValueId` strings.
    pub async fn set_session_config_option(
        &self,
        session_id: SessionId,
        config_id: &str,
        value: &str,
    ) -> Result<SetSessionConfigOptionResponse> {
        self.set_session_config_option_value(
            session_id,
            config_id,
            SessionConfigOptionValue::ValueId {
                value: SessionConfigValueId::new(value.to_owned()),
            },
        )
        .await
    }

    pub async fn set_session_config_option_value(
        &self,
        session_id: SessionId,
        config_id: &str,
        value: SessionConfigOptionValue,
    ) -> Result<SetSessionConfigOptionResponse> {
        let connection = self.connection().await?;
        let acp_session_id = session_id.0.to_string();
        let request = SetSessionConfigOptionRequest::new(session_id, config_id.to_owned(), value);
        self.timed_request(
            &connection,
            "session/set_config_option",
            Some(&acp_session_id),
            request,
        )
        .await
        .map_err(|err| self.request_failed("session/set_config_option", Some(&acp_session_id), err))
    }

    /// `session/set_mode`. The native modes lane, used when a mode is advertised
    /// in `NewSessionResponse.modes` rather than as a `config_options` select.
    pub async fn set_session_mode(
        &self,
        session_id: SessionId,
        mode_id: &str,
    ) -> Result<SetSessionModeResponse> {
        let connection = self.connection().await?;
        let acp_session_id = session_id.0.to_string();
        let request =
            SetSessionModeRequest::new(session_id, SessionModeId::new(mode_id.to_owned()));
        self.timed_request(
            &connection,
            "session/set_mode",
            Some(&acp_session_id),
            request,
        )
        .await
        .map_err(|err| self.request_failed("session/set_mode", Some(&acp_session_id), err))
    }

    /// `session/load`. Requires the `loadSession` capability.
    pub async fn load_session(
        &self,
        session_id: SessionId,
        cwd: PathBuf,
        mcp_servers: Vec<McpServer>,
    ) -> Result<()> {
        if !self.capabilities.supports_load_session() {
            return Err(StackError::AgentUnsupportedCapability {
                name: "session/load",
            });
        }
        self.capabilities
            .reject_unmodeled_mcp_servers(&mcp_servers)?;
        let connection = self.connection().await?;
        let attached_id = session_id.0.to_string();
        let request = LoadSessionRequest::new(session_id, cwd).mcp_servers(mcp_servers);
        self.timed_request(&connection, "session/load", Some(&attached_id), request)
            .await
            .map_err(|err| self.request_failed("session/load", Some(&attached_id), err))?;
        self.mark_session_attached(&attached_id).await;
        Ok(())
    }

    /// `session/resume`, gated only by the agent's advertised capability.
    pub async fn resume_session(
        &self,
        session_id: SessionId,
        cwd: PathBuf,
        mcp_servers: Vec<McpServer>,
    ) -> Result<()> {
        if !self.capabilities.supports_resume_session() {
            return Err(StackError::AgentUnsupportedCapability {
                name: "session/resume",
            });
        }
        self.capabilities
            .reject_unmodeled_mcp_servers(&mcp_servers)?;
        let connection = self.connection().await?;
        let attached_id = session_id.0.to_string();
        let request = ResumeSessionRequest::new(session_id, cwd).mcp_servers(mcp_servers);
        self.timed_request(&connection, "session/resume", Some(&attached_id), request)
            .await
            .map_err(|err| self.request_failed("session/resume", Some(&attached_id), err))?;
        self.mark_session_attached(&attached_id).await;
        Ok(())
    }

    /// `session/close`, gated only by the agent's advertised capability.
    pub async fn close_session(&self, session_id: SessionId) -> Result<()> {
        if !self.capabilities.supports_close_session() {
            return Err(StackError::AgentUnsupportedCapability {
                name: "session/close",
            });
        }
        let connection = self.connection().await?;
        let attached_id = session_id.0.to_string();
        let request = CloseSessionRequest::new(session_id);
        self.timed_request(&connection, "session/close", Some(&attached_id), request)
            .await
            .map_err(|err| self.request_failed("session/close", Some(&attached_id), err))?;
        self.forget_attached_session(&attached_id).await;
        Ok(())
    }

    /// `session/delete`. Requires `sessionCapabilities.delete`; the spec makes
    /// repeat deletes succeed silently.
    pub async fn delete_session(&self, session_id: SessionId) -> Result<()> {
        if !self.capabilities.supports_delete_session() {
            return Err(StackError::AgentUnsupportedCapability {
                name: "session/delete",
            });
        }
        let connection = self.connection().await?;
        let attached_id = session_id.0.to_string();
        let request = DeleteSessionRequest::new(session_id);
        self.timed_request(&connection, "session/delete", Some(&attached_id), request)
            .await
            .map_err(|err| self.request_failed("session/delete", Some(&attached_id), err))?;
        self.forget_attached_session(&attached_id).await;
        Ok(())
    }

    /// `session/prompt`, awaiting the turn's final response. The raw
    /// `err.to_string()` is never persisted, so no URL, header, body, or secret
    /// reaches the state row.
    pub async fn prompt_session(&self, request: PromptRequest) -> Result<PromptResponse> {
        self.capabilities.validate_prompt(&request.prompt)?;
        let connection = self.connection().await?;
        let agent_session_id = request.session_id.0.to_string();
        match connection.send_request(request).block_task().await {
            Ok(response) => Ok(response),
            Err(err) => {
                // The persisted message stays static, so the local process log
                // is the only place the adapter's own words survive. Without
                // this an adapter rejecting the session id reads exactly like
                // an adapter rejecting the prompt content.
                tracing::warn!(
                    method = "session/prompt",
                    agent_id = %self.agent_id,
                    target_id = self.target_id.as_deref().unwrap_or_default(),
                    agent_session_id = %agent_session_id,
                    acp_error_code = i32::from(err.code),
                    error = %err,
                    "agent rejected the prompt request"
                );
                let classified = inference_failure::classify(&err);
                Err(map_prompt_error(classified))
            }
        }
    }

    /// `session/cancel` is a fire-and-forget notification.
    pub async fn cancel_session(&self, session_id: SessionId) -> Result<()> {
        let connection = self.connection().await?;
        let acp_session_id = session_id.0.to_string();
        connection
            .send_notification(CancelNotification::new(session_id))
            .map_err(|err| {
                // A notification gets no reply, so this is a local send failure.
                self.local_request_failure("session/cancel", Some(&acp_session_id), err.to_string())
            })?;
        Ok(())
    }
}

/// Translate a classified prompt failure into a `StackError`. Only the
/// classifier's vetted fields cross over; the raw upstream message is dropped.
fn map_prompt_error(classified: Classified) -> StackError {
    match classified.class {
        FailureClass::Inference5xx | FailureClass::Inference4xx => match classified.status_code {
            Some(code) if code != 0 => StackError::InferenceRequestFailed {
                status_code: code,
                reason_category: classified.reason_category,
            },
            // An inference class with no status code would persist
            // `status_code = 0`, a meaningless row.
            _ => StackError::agent_request_failed("session/prompt", "prompt request failed"),
        },
        _ => StackError::agent_request_failed("session/prompt", "prompt request failed"),
    }
}

/// The adapter's JSON-RPC error with secrets redacted and `message` and `data` each bounded at
/// [`ACP_ERROR_DETAIL_MAX_BYTES`]. A `data` that serializes past the cap becomes a truncated
/// JSON string, since a cut JSON value would no longer parse.
fn acp_error_detail(error: AcpError) -> AcpErrorDetail {
    let message = bounded(&redact_text(&error.message), ACP_ERROR_DETAIL_MAX_BYTES).into_owned();
    let data = error.data.map(|mut data| {
        redact_json(&mut data);
        if let Value::String(text) = &data {
            return Value::String(bounded(text, ACP_ERROR_DETAIL_MAX_BYTES).into_owned());
        }
        let serialized = data.to_string();
        if serialized.len() <= ACP_ERROR_DETAIL_MAX_BYTES {
            data
        } else {
            Value::String(bounded(&serialized, ACP_ERROR_DETAIL_MAX_BYTES).into_owned())
        }
    });
    AcpErrorDetail {
        code: i32::from(error.code),
        message,
        data,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acp_error_detail_keeps_small_data_as_json() {
        let error = AcpError::new(-32000, "refused").data(serde_json::json!({ "reason": "quota" }));
        let detail = acp_error_detail(error);
        assert_eq!(detail.code, -32000);
        assert_eq!(detail.message, "refused");
        assert_eq!(detail.data, Some(serde_json::json!({ "reason": "quota" })));
    }

    #[test]
    fn acp_error_detail_bounds_message_and_turns_oversized_data_into_a_cut_string() {
        let long = "x".repeat(ACP_ERROR_DETAIL_MAX_BYTES * 2);
        let error = AcpError::new(-32603, long.clone()).data(serde_json::json!({ "trace": long }));
        let detail = acp_error_detail(error);
        assert!(
            detail
                .message
                .starts_with(&"x".repeat(ACP_ERROR_DETAIL_MAX_BYTES))
        );
        assert!(
            detail.message.ends_with("[truncated 2048 bytes]"),
            "{}",
            detail.message
        );
        let Some(Value::String(data)) = &detail.data else {
            panic!("oversized data must become a string: {:?}", detail.data);
        };
        assert!(data.starts_with("{\"trace\":\"xxx"), "{data}");
        assert!(data.contains("[truncated "), "{data}");
    }

    #[test]
    fn acp_error_detail_bounds_a_string_data_without_reserializing_it() {
        let long = "y".repeat(ACP_ERROR_DETAIL_MAX_BYTES + 10);
        let detail = acp_error_detail(AcpError::new(-32000, "refused").data(long));
        assert_eq!(
            detail.data,
            Some(Value::String(format!(
                "{} [truncated 10 bytes]",
                "y".repeat(ACP_ERROR_DETAIL_MAX_BYTES)
            )))
        );
    }
}
