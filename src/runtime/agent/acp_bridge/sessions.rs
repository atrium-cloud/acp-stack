//! Session RPC methods dispatched over the live ACP connection, plus the
//! prompt-failure mapping they share.

use super::*;

impl AcpBridge {
    pub(super) async fn connection(&self) -> Result<ConnectionTo<Agent>> {
        let guard = self.connection.lock().await;
        guard.as_ref().cloned().ok_or(StackError::AgentNotRunning)
    }

    /// `session/new`. Always supported per ACP baseline.
    pub async fn new_session(
        &self,
        cwd: PathBuf,
        mcp_servers: Vec<McpServer>,
    ) -> Result<NewSessionResponse> {
        self.capabilities
            .reject_unmodeled_mcp_servers(&mcp_servers)?;
        let connection = self.connection().await?;
        let mut request = NewSessionRequest::new(cwd);
        request.mcp_servers = mcp_servers;
        let response = connection
            .send_request(request)
            .block_task()
            .await
            .map_err(|err| StackError::AgentRequestFailed {
                method: "session/new",
                message: err.to_string(),
            })?;
        Ok(response)
    }

    pub async fn fork_session(
        &self,
        session_id: SessionId,
        cwd: PathBuf,
        mcp_servers: Vec<McpServer>,
        message_id: Option<String>,
    ) -> Result<ForkSessionResponse> {
        if !self.capabilities.supports_fork_session() {
            return Err(StackError::AgentUnsupportedCapability {
                name: "session/fork",
            });
        }
        if message_id.is_some() && !self.capabilities.supports_fork_message_id() {
            return Err(StackError::AgentUnsupportedCapability {
                name: "session/fork.messageId",
            });
        }
        self.capabilities
            .reject_unmodeled_mcp_servers(&mcp_servers)?;
        let connection = self.connection().await?;
        let mut request = ForkSessionRequest::new(session_id, cwd).mcp_servers(mcp_servers);
        if let Some(message_id) = message_id {
            request = request.meta(prompt_message_id_meta(&message_id));
        }
        connection
            .send_request(request)
            .block_task()
            .await
            .map_err(|err| StackError::AgentRequestFailed {
                method: "session/fork",
                message: err.to_string(),
            })
    }

    /// `session/list`. Requires the `sessionCapabilities.list` capability.
    pub async fn list_sessions(&self) -> Result<Vec<SessionInfo>> {
        if !self.capabilities.supports_list_sessions() {
            return Err(StackError::AgentUnsupportedCapability {
                name: "session/list",
            });
        }
        let connection = self.connection().await?;
        let mut sessions = Vec::new();
        let mut cursor = None;
        let mut seen_cursors = HashSet::new();
        loop {
            let request = ListSessionsRequest::new().cursor(cursor.clone());
            let response: ListSessionsResponse = connection
                .send_request(request)
                .block_task()
                .await
                .map_err(|err| StackError::AgentRequestFailed {
                    method: "session/list",
                    message: err.to_string(),
                })?;
            sessions.extend(response.sessions);
            let Some(next_cursor) = response.next_cursor else {
                return Ok(sessions);
            };
            if !seen_cursors.insert(next_cursor.clone()) {
                return Err(StackError::AgentRequestFailed {
                    method: "session/list",
                    message: format!("agent returned repeated pagination cursor `{next_cursor}`"),
                });
            }
            cursor = Some(next_cursor);
        }
    }

    pub async fn set_session_config_option(
        &self,
        session_id: SessionId,
        config_id: &str,
        value: &str,
    ) -> Result<SetSessionConfigOptionResponse> {
        let connection = self.connection().await?;
        let request = SetSessionConfigOptionRequest::new(
            session_id,
            config_id.to_owned(),
            SessionConfigValueId::new(value.to_owned()),
        );
        connection
            .send_request(request)
            .block_task()
            .await
            .map_err(|err| StackError::AgentRequestFailed {
                method: "session/set_config_option",
                message: err.to_string(),
            })
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
        let request = LoadSessionRequest::new(session_id, cwd).mcp_servers(mcp_servers);
        connection
            .send_request(request)
            .block_task()
            .await
            .map_err(|err| StackError::AgentRequestFailed {
                method: "session/load",
                message: err.to_string(),
            })?;
        Ok(())
    }

    /// `session/resume`. Stable in ACP v1; gated only by the agent's
    /// advertised capability. The agent may still reject if it does not
    /// implement resume — that surfaces as `agent.request_failed`.
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
        let request = ResumeSessionRequest::new(session_id, cwd).mcp_servers(mcp_servers);
        connection
            .send_request(request)
            .block_task()
            .await
            .map_err(|err| StackError::AgentRequestFailed {
                method: "session/resume",
                message: err.to_string(),
            })?;
        Ok(())
    }

    /// `session/close`. Stable in ACP v1; gated only by the agent's
    /// advertised capability.
    pub async fn close_session(&self, session_id: SessionId) -> Result<()> {
        if !self.capabilities.supports_close_session() {
            return Err(StackError::AgentUnsupportedCapability {
                name: "session/close",
            });
        }
        let connection = self.connection().await?;
        let request = CloseSessionRequest::new(session_id);
        connection
            .send_request(request)
            .block_task()
            .await
            .map_err(|err| StackError::AgentRequestFailed {
                method: "session/close",
                message: err.to_string(),
            })?;
        Ok(())
    }

    /// `session/delete`. Requires the `sessionCapabilities.delete`
    /// capability. Unlike close, the agent removes the session from its own
    /// history; repeat deletes are specified to succeed silently.
    pub async fn delete_session(&self, session_id: SessionId) -> Result<()> {
        if !self.capabilities.supports_delete_session() {
            return Err(StackError::AgentUnsupportedCapability {
                name: "session/delete",
            });
        }
        let connection = self.connection().await?;
        let request = DeleteSessionRequest::new(session_id);
        connection
            .send_request(request)
            .block_task()
            .await
            .map_err(|err| StackError::AgentRequestFailed {
                method: "session/delete",
                message: err.to_string(),
            })?;
        Ok(())
    }

    /// `session/prompt`. Awaits the turn's final response.
    ///
    /// On error, runs the inference-failure classifier so upstream HTTP
    /// failures (5xx, 429, etc.) become a typed `InferenceRequestFailed`
    /// variant; everything else falls back to `AgentRequestFailed`. The raw
    /// `err.to_string()` is never persisted: 4xx/5xx paths surface only the
    /// vetted reason label, and the generic fallback uses a sanitized message
    /// to avoid leaking URLs / headers / bodies / secrets into the state row.
    pub async fn prompt_session(&self, request: PromptRequest) -> Result<PromptResponse> {
        self.capabilities.validate_prompt(&request.prompt)?;
        let connection = self.connection().await?;
        match connection.send_request(request).block_task().await {
            Ok(response) => Ok(response),
            Err(err) => {
                let classified = inference_failure::classify(&err);
                Err(map_prompt_error(classified))
            }
        }
    }

    /// `session/cancel` is a fire-and-forget notification.
    pub async fn cancel_session(&self, session_id: SessionId) -> Result<()> {
        let connection = self.connection().await?;
        connection
            .send_notification(CancelNotification::new(session_id))
            .map_err(|err| StackError::AgentRequestFailed {
                method: "session/cancel",
                message: err.to_string(),
            })?;
        Ok(())
    }
}

/// Translate a classified prompt failure into the appropriate `StackError`
/// variant. Only the classifier's vetted fields (status code + static reason
/// label) cross into the error; the raw upstream message is dropped so the
/// state row carries no URLs / headers / bodies / secrets.
fn map_prompt_error(classified: Classified) -> StackError {
    match classified.class {
        FailureClass::Inference5xx | FailureClass::Inference4xx => match classified.status_code {
            Some(code) if code != 0 => StackError::InferenceRequestFailed {
                status_code: code,
                reason_category: classified.reason_category,
            },
            // Defensive fallback: classifier returned an inference class but no
            // status code. Treat as a generic agent failure rather than
            // persisting `status_code = 0`, which would be a meaningless row.
            _ => StackError::AgentRequestFailed {
                method: "session/prompt",
                message: "prompt request failed".to_owned(),
            },
        },
        _ => StackError::AgentRequestFailed {
            method: "session/prompt",
            message: "prompt request failed".to_owned(),
        },
    }
}
