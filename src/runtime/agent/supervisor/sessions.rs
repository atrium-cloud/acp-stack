//! Session management: ACP `session/*` dispatch plus the durable `sessions`
//! rows that mirror it.
//!
//! Everything here runs against a live bridge obtained from the supervisor's
//! state machine, and every method persists only after the agent confirms —
//! the agent's `session_id` is authoritative, so a local row must never exist
//! for a session the agent rejected.

use super::*;

impl AgentSupervisor {
    /// Sync sessions discoverable via ACP `session/list` into durable local
    /// state. This is discovery only: newly learned sessions are marked
    /// `available` until a caller explicitly loads or resumes them.
    pub async fn sync_listed_sessions(
        &self,
        target_id: &str,
        agent: &AgentConfig,
        workspace_root: &str,
        state: &Arc<TokioMutex<StateStore>>,
    ) -> Result<SessionListSyncResult> {
        let bridge = {
            let guard = self.state.lock().await;
            match &*guard {
                AgentState::Running(bridge) => Arc::clone(bridge),
                AgentState::Stopped
                | AgentState::Starting
                | AgentState::Stopping
                | AgentState::Updating => {
                    return Ok(SessionListSyncResult {
                        attempted: false,
                        status: SessionListSyncStatus::NotRunning,
                        upserted: 0,
                        updated: 0,
                    });
                }
            }
        };
        if !bridge.capabilities().supports_list_sessions() {
            return Ok(SessionListSyncResult {
                attempted: false,
                status: SessionListSyncStatus::Unsupported,
                upserted: 0,
                updated: 0,
            });
        }
        let sessions = bridge.list_sessions().await?;
        let mut skipped_invalid_cwd = 0_u32;
        let mut records = Vec::new();
        for session in sessions {
            let raw_cwd = session.cwd.to_string_lossy().into_owned();
            let cwd = match resolve_session_cwd(Some(raw_cwd.clone()), workspace_root) {
                Ok(cwd) => cwd,
                Err(err) => {
                    skipped_invalid_cwd += 1;
                    tracing::warn!(
                        error = %err,
                        session_id = %session.session_id.0,
                        cwd = %raw_cwd,
                        "skipping ACP-listed session with invalid cwd"
                    );
                    continue;
                }
            };
            let agent_session_id = session.session_id.0.to_string();
            let updated_at = session.updated_at.clone();
            let metadata_json = serde_json::json!({
                "source": "agent_list",
                "agent_session_id": &agent_session_id,
                "agent_updated_at": &updated_at,
                "agent_meta": session.meta,
            })
            .to_string();
            records.push(ListedSessionRecord {
                id: next_session_id(),
                agent_session_id,
                agent_id: agent.id.clone(),
                cwd,
                title: session.title,
                updated_at: session.updated_at,
                metadata_json,
            });
        }
        let guard = state.lock().await;
        let counts = guard.upsert_listed_sessions_for_target(target_id, records)?;
        let payload = serde_json::json!({
            "target_id": target_id,
            "agent_id": agent.id,
            "upserted": counts.upserted,
            "updated": counts.updated,
            "skipped_invalid_cwd": skipped_invalid_cwd,
        })
        .to_string();
        guard.append_event(
            "info",
            "session.list_synced",
            "ACP session list synced",
            &payload,
        )?;
        Ok(SessionListSyncResult {
            attempted: true,
            status: SessionListSyncStatus::Synced,
            upserted: counts.upserted,
            updated: counts.updated,
        })
    }

    /// `POST /v1/sessions`. Dispatches ACP `session/new`, persists a new
    /// `sessions` row, and returns a `SessionAttachOutcome`: the record, the
    /// names of the MCP servers actually sent to the agent (after transport
    /// partitioning, so the caller's `mcp.session_attached` event cannot
    /// claim skipped servers), and an `ignored` list of configured mode/model/effort
    /// values the agent did not advertise — those sessions proceed on the
    /// agent's default. `cwd` defaults to `workspace.root` when the client
    /// omits it.
    pub async fn create_session(
        &self,
        target_id: &str,
        agent: &AgentConfig,
        workspace_root: &str,
        cwd: Option<String>,
        mcp_servers: Vec<McpServer>,
        state: &Arc<TokioMutex<StateStore>>,
    ) -> Result<SessionAttachOutcome> {
        let bridge = self.bridge().await?;
        let resolved_cwd = resolve_session_cwd(cwd, workspace_root)?;
        let cwd_path = PathBuf::from(&resolved_cwd);
        let PartitionedMcpServers { accepted, skipped } =
            bridge.capabilities().partition_mcp_servers(mcp_servers)?;
        let accepted_names = crate::runtime::agent::mcp::server_names(&accepted);
        let response = bridge.new_session(cwd_path, accepted).await?;
        let agent_session_id = response.session_id.0.to_string();
        let mut ignored: Vec<IgnoredFeature> = Vec::new();
        // The last refreshed option list from a successful set; the stored
        // per-session snapshot must reflect what was actually applied, not
        // the pre-provisioning `session/new` state.
        let mut latest_options: Option<Vec<SessionConfigOption>> = None;
        if let Some(mode) = agent.mode.as_deref()
            && let Some(refreshed) = provision_session_option(
                &bridge,
                &response.session_id,
                session_config_id_for_value(
                    response.config_options.as_deref(),
                    AgentSessionConfigCategory::Mode,
                    mode,
                ),
                mode,
                IGNORED_FEATURE_AGENT_MODE,
                "sessionConfig.mode",
                &mut ignored,
            )
            .await?
        {
            latest_options = Some(refreshed);
        }
        if let Some(model) = agent.model.as_deref().or_else(|| {
            agent
                .provider
                .as_ref()
                .and_then(|provider| provider.model.as_deref())
        }) {
            if model_value_is_explicit_without_discovery(agent) {
                // The harness reads this pin from its on-disk config at
                // process start; the adapter's advertised list is an echo of
                // it at best, so an exact-match set here can only fail
                // spuriously.
                tracing::debug!(
                    model,
                    "model provisioned on disk; skipping session/set_config_option"
                );
            } else {
                // Judged against the freshest advertised list, like the effort
                // lane and generic map below: the mode set above may have
                // refreshed the option set.
                let lookup = session_config_id_for_value(
                    latest_options
                        .as_deref()
                        .or(response.config_options.as_deref()),
                    AgentSessionConfigCategory::Model,
                    model,
                );
                if let Some(refreshed) = provision_session_option(
                    &bridge,
                    &response.session_id,
                    lookup,
                    model,
                    IGNORED_FEATURE_AGENT_MODEL,
                    "sessionConfig.model",
                    &mut ignored,
                )
                .await?
                {
                    latest_options = Some(refreshed);
                }
            }
        }
        // Applied after the model and judged against the freshest advertised
        // list: adapters advertise effort levels for the currently selected
        // model, so an effort option that appears only once the configured
        // model is set must be visible here. A miss still lands in `ignored`.
        if let Some(effort) = agent.effort.as_deref()
            && let Some(refreshed) = provision_session_option(
                &bridge,
                &response.session_id,
                session_config_id_for_value(
                    latest_options
                        .as_deref()
                        .or(response.config_options.as_deref()),
                    AgentSessionConfigCategory::Effort,
                    effort,
                ),
                effort,
                IGNORED_FEATURE_AGENT_EFFORT,
                "sessionConfig.effort",
                &mut ignored,
            )
            .await?
        {
            latest_options = Some(refreshed);
        }
        // The generic `[agent.config_options]` map applies last, deliberately:
        // the typed lanes stay authoritative for their categories, and each
        // map entry is judged against the freshest advertised list so a
        // model-dependent option observes the model set above. Any miss —
        // unknown id, unadvertised select value, kind mismatch — is an
        // ignored record, never an error.
        for (option_id, configured) in &agent.config_options {
            let advertised = latest_options
                .as_deref()
                .or(response.config_options.as_deref());
            let lookup = resolve_generic_config_option(advertised, option_id, configured);
            let (value, display_value) = match configured {
                crate::config::AgentConfigOptionValue::Bool(value) => (
                    SessionConfigOptionValue::Boolean { value: *value },
                    value.to_string(),
                ),
                crate::config::AgentConfigOptionValue::Text(text) => (
                    SessionConfigOptionValue::ValueId {
                        value: SessionConfigValueId::new(text.clone()),
                    },
                    text.clone(),
                ),
            };
            if let Some(refreshed) = provision_session_option_value(
                &bridge,
                &response.session_id,
                lookup,
                value,
                display_value,
                IGNORED_FEATURE_AGENT_CONFIG_OPTION,
                "sessionConfig.configOption",
                Some(option_id.clone()),
                &mut ignored,
            )
            .await?
            {
                latest_options = Some(refreshed);
            }
        }

        // Persist after the agent confirms. If we inserted first and the
        // agent rejected, we'd leave a phantom row. The agent's `session_id`
        // is authoritative; we mirror it into our `sessions` table.
        let record = NewSessionRecord {
            id: next_session_id(),
            agent_id: agent.id.clone(),
            cwd: resolved_cwd,
            title: None,
            metadata_json: "{}".to_owned(),
        };
        let guard = state.lock().await;
        let inserted =
            guard.insert_session_for_target(target_id, agent_session_id.clone(), record)?;
        guard.append_session_event(
            &inserted.id,
            "info",
            "session.created",
            "session created",
            &json!({
                "target_id": target_id,
                "agent_id": agent.id,
                "agent_session_id": &agent_session_id,
                "cwd": &inserted.cwd,
            })
            .to_string(),
        )?;
        append_mcp_skipped_event(&guard, &inserted.id, &skipped)?;
        append_capability_ignored_event(&guard, &inserted.id, &ignored)?;
        // Seed the per-session config-option snapshot from the freshest list
        // in hand. A snapshot failure must not undo an already-confirmed
        // session; the raw events remain the source of truth.
        let advertised = latest_options
            .or(response.config_options)
            .unwrap_or_default();
        let mut snapshot =
            crate::runtime::agent::config_options::project_config_options(&advertised);
        if snapshot.len() > crate::state::MAX_SESSION_CONFIG_OPTIONS {
            tracing::warn!(
                session = %inserted.id,
                advertised = advertised.len(),
                stored = crate::state::MAX_SESSION_CONFIG_OPTIONS,
                "agent advertised more config options than the stored cap; list truncated"
            );
            snapshot.truncate(crate::state::MAX_SESSION_CONFIG_OPTIONS);
        }
        match serde_json::to_value(&snapshot) {
            Ok(value) => {
                if let Err(error) = guard.replace_session_config_options(&inserted.id, value) {
                    tracing::warn!(%error, session = %inserted.id, "config-option snapshot write failed");
                }
            }
            Err(error) => {
                tracing::warn!(%error, session = %inserted.id, "config-option snapshot serialize failed");
            }
        }
        Ok(SessionAttachOutcome {
            record: inserted,
            attached_mcp: accepted_names,
            ignored,
        })
    }

    /// `POST /v1/sessions/{id}/load`. Capability-gated by the bridge. Returns
    /// the session record plus the MCP server names actually sent to the
    /// agent. `ignored` is always empty: mode/model/effort provisioning happens only
    /// at create.
    pub async fn load_session(
        &self,
        session_id: &str,
        cwd: Option<String>,
        mcp_servers: Vec<McpServer>,
        workspace_root: &str,
        state: &Arc<TokioMutex<StateStore>>,
    ) -> Result<SessionAttachOutcome> {
        self.attach_session(
            session_id,
            cwd,
            mcp_servers,
            workspace_root,
            state,
            SessionAttachKind::Load,
        )
        .await
    }

    /// `POST /v1/sessions/{id}/resume`. Returns the session record plus the
    /// MCP server names actually sent to the agent. `ignored` is always
    /// empty: mode/model/effort provisioning happens only at create.
    pub async fn resume_session(
        &self,
        session_id: &str,
        cwd: Option<String>,
        mcp_servers: Vec<McpServer>,
        workspace_root: &str,
        state: &Arc<TokioMutex<StateStore>>,
    ) -> Result<SessionAttachOutcome> {
        self.attach_session(
            session_id,
            cwd,
            mcp_servers,
            workspace_root,
            state,
            SessionAttachKind::Resume,
        )
        .await
    }

    /// Shared body of [`Self::load_session`] and [`Self::resume_session`],
    /// which differ only in the ACP method sent and the event recorded.
    async fn attach_session(
        &self,
        session_id: &str,
        cwd: Option<String>,
        mcp_servers: Vec<McpServer>,
        workspace_root: &str,
        state: &Arc<TokioMutex<StateStore>>,
        kind: SessionAttachKind,
    ) -> Result<SessionAttachOutcome> {
        let bridge = self.bridge().await?;
        let record = fetch_open_session(state, session_id).await?;
        let explicit_cwd = cwd.is_some();
        let requested_cwd =
            cwd.unwrap_or_else(|| stored_or_workspace_cwd(&record.cwd, workspace_root));
        let resolved_cwd = resolve_session_cwd(Some(requested_cwd), workspace_root)?;
        let PartitionedMcpServers { accepted, skipped } =
            bridge.capabilities().partition_mcp_servers(mcp_servers)?;
        let accepted_names = crate::runtime::agent::mcp::server_names(&accepted);
        let acp_session_id = AcpSessionId::new(record.agent_session_id.clone());
        let acp_cwd = PathBuf::from(&resolved_cwd);
        match kind {
            SessionAttachKind::Load => {
                bridge
                    .load_session(acp_session_id, acp_cwd, accepted)
                    .await?;
            }
            SessionAttachKind::Resume => {
                bridge
                    .resume_session(acp_session_id, acp_cwd, accepted)
                    .await?;
            }
        }
        let guard = state.lock().await;
        append_mcp_skipped_event(&guard, session_id, &skipped)?;
        if explicit_cwd {
            guard.update_session_status_and_cwd(
                session_id,
                SESSION_STATUS_ACTIVE,
                &resolved_cwd,
            )?;
        } else {
            guard.update_session_status(session_id, SESSION_STATUS_ACTIVE)?;
        }
        let (event_kind, event_message) = match kind {
            SessionAttachKind::Load => ("session.loaded", "session loaded"),
            SessionAttachKind::Resume => ("session.resumed", "session resumed"),
        };
        guard.append_session_event(
            session_id,
            "info",
            event_kind,
            event_message,
            &json!({ "agent_session_id": record.agent_session_id, "cwd": resolved_cwd })
                .to_string(),
        )?;
        let record = guard
            .get_session(session_id)?
            .ok_or_else(|| StackError::SessionNotFound {
                id: session_id.to_owned(),
            })?;
        Ok(SessionAttachOutcome {
            record,
            attached_mcp: accepted_names,
            ignored: Vec::new(),
        })
    }

    /// `POST /v1/sessions/{id}/fork`. Returns the child session record plus
    /// the MCP server names actually sent to the agent. `ignored` is always
    /// empty: mode/model/effort provisioning happens only at create.
    pub async fn fork_session(
        &self,
        parent_session_id: &str,
        cwd: Option<String>,
        mcp_servers: Vec<McpServer>,
        workspace_root: &str,
        message_id: Option<String>,
        state: &Arc<TokioMutex<StateStore>>,
    ) -> Result<SessionAttachOutcome> {
        let bridge = self.bridge().await?;
        let parent = fetch_open_session(state, parent_session_id).await?;
        let breakpoint_message_id = if let Some(message_id) = message_id {
            let prompt = {
                let guard = state.lock().await;
                guard.get_prompt_by_message_id(parent_session_id, &message_id)?
            }
            .ok_or_else(|| StackError::InvalidParam {
                field: "message_id",
                reason: format!(
                    "session `{parent_session_id}` has no prompt with message id `{message_id}`"
                ),
            })?;
            if !prompt.message_id_acknowledged {
                return Err(StackError::InvalidParam {
                    field: "message_id",
                    reason: format!("message id `{message_id}` was not acknowledged by the agent"),
                });
            }
            Some(message_id)
        } else {
            None
        };
        let resolved_cwd = resolve_session_cwd(
            Some(cwd.unwrap_or_else(|| stored_or_workspace_cwd(&parent.cwd, workspace_root))),
            workspace_root,
        )?;
        let parent_agent_session_id = parent.agent_session_id.clone();
        let PartitionedMcpServers { accepted, skipped } =
            bridge.capabilities().partition_mcp_servers(mcp_servers)?;
        let accepted_names = crate::runtime::agent::mcp::server_names(&accepted);
        let response = bridge
            .fork_session(
                AcpSessionId::new(parent_agent_session_id.clone()),
                PathBuf::from(&resolved_cwd),
                accepted,
                breakpoint_message_id.clone(),
            )
            .await?;
        let child_agent_session_id = response.session_id.0.to_string();
        let child_session_id = next_session_id();
        let metadata_json = json!({
            "fork": {
                "parent_session_id": parent_session_id,
                "parent_agent_session_id": &parent_agent_session_id,
                "agent_session_id": &child_agent_session_id,
                "strategy": "acp_native",
                "message_id": &breakpoint_message_id,
            }
        })
        .to_string();
        let record = NewSessionRecord {
            id: child_session_id.clone(),
            agent_id: parent.agent_id.clone(),
            cwd: resolved_cwd,
            title: parent.title.clone(),
            metadata_json,
        };
        let guard = state.lock().await;
        let inserted = guard.insert_session_for_target(
            &parent.target_id,
            child_agent_session_id.clone(),
            record,
        )?;
        let payload = json!({
            "target_id": &parent.target_id,
            "parent_session_id": parent_session_id,
            "parent_agent_session_id": &parent_agent_session_id,
            "child_session_id": &child_session_id,
            "child_agent_session_id": &child_agent_session_id,
            "strategy": "acp_native",
            "message_id": &breakpoint_message_id,
            "cwd": &inserted.cwd,
        })
        .to_string();
        guard.append_session_event(
            &inserted.id,
            "info",
            "session.forked",
            "session forked",
            &payload,
        )?;
        guard.append_session_event(
            parent_session_id,
            "info",
            "session.fork.created_child",
            "session fork child created",
            &payload,
        )?;
        append_mcp_skipped_event(&guard, &inserted.id, &skipped)?;
        Ok(SessionAttachOutcome {
            record: inserted,
            attached_mcp: accepted_names,
            ignored: Vec::new(),
        })
    }

    /// `DELETE /v1/sessions/{id}`. Closes the agent-side session and marks
    /// the local row `closed`.
    ///
    /// Order matters: send `session/close` to the agent first, and only on
    /// success cancel local in-flight prompts and mark the row closed.
    /// Otherwise a failed bridge call would leave the agent still running
    /// the session while we mark it closed locally.
    pub async fn close_session(
        &self,
        session_id: &str,
        state: &Arc<TokioMutex<StateStore>>,
    ) -> Result<SessionRecord> {
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
            .close_session(AcpSessionId::new(agent_session_id.clone()))
            .await?;
        // Bridge confirmed the close — now it's safe to settle local state.
        self.cancel_prompts_for_session(session_id).await;
        let guard = state.lock().await;
        guard.update_session_status(session_id, SESSION_STATUS_CLOSED)?;
        guard.append_session_event(
            session_id,
            "info",
            "session.closed",
            "session closed",
            &json!({ "agent_session_id": agent_session_id }).to_string(),
        )?;
        guard
            .get_session(session_id)?
            .ok_or_else(|| StackError::SessionNotFound {
                id: session_id.to_owned(),
            })
    }

    /// `POST /v1/sessions/{id}/delete`. Forwards ACP `session/delete` to the
    /// agent, then hard-deletes the local session row with its prompts and
    /// events. An unknown id returns `Ok(None)` without touching the agent —
    /// ACP specifies repeat deletes succeed silently.
    ///
    /// Same ordering rule as close: the agent confirms the delete first, and
    /// only then is local state settled. A failed bridge call must not strand
    /// a session the agent still lists without any local record of it.
    pub async fn delete_session(
        &self,
        session_id: &str,
        state: &Arc<TokioMutex<StateStore>>,
    ) -> Result<Option<SessionRecord>> {
        let agent_session_id = {
            let guard = state.lock().await;
            match guard.get_session(session_id)? {
                Some(record) => record.agent_session_id,
                None => return Ok(None),
            }
        };
        let bridge = self.bridge().await?;
        bridge
            .delete_session(AcpSessionId::new(agent_session_id))
            .await?;
        self.cancel_prompts_for_session(session_id).await;
        let guard = state.lock().await;
        guard.delete_session(session_id)
    }

    /// `POST /v1/sessions/{id}/config-options`. Forwards one
    /// `session/set_config_option` to the live agent and rewrites the stored
    /// per-session snapshot from the refreshed list in the response. Unlike
    /// session-create provisioning there is no ignored softening here: the
    /// caller asked for exactly this set, so a failure is the answer.
    pub async fn set_session_config_option(
        &self,
        session_id: &str,
        config_id: &str,
        value: SessionConfigOptionValue,
        state: &Arc<TokioMutex<StateStore>>,
    ) -> Result<Vec<crate::runtime::agent::config_options::SessionConfigOptionSnapshot>> {
        let record = fetch_open_session(state, session_id).await?;
        let bridge = self.bridge().await?;
        let response = bridge
            .set_session_config_option_value(
                AcpSessionId::new(record.agent_session_id.clone()),
                config_id,
                value,
            )
            .await?;
        let guard = state.lock().await;
        // A live set changes agent behavior for the rest of the session, so
        // it leaves a durable trace like create-time provisioning does.
        guard.append_session_event(
            session_id,
            "info",
            "session.config_option_set",
            "session config option set",
            &json!({ "config_id": config_id }).to_string(),
        )?;
        // Same lax-adapter guard as session-create provisioning: an empty
        // response right after a successful set must not wipe the snapshot —
        // such agents refresh through `config_option_update` notifications
        // instead, which the session sink projects as they arrive.
        if response.config_options.is_empty() {
            return Ok(Vec::new());
        }
        let mut snapshot =
            crate::runtime::agent::config_options::project_config_options(&response.config_options);
        snapshot.truncate(crate::state::MAX_SESSION_CONFIG_OPTIONS);
        match serde_json::to_value(&snapshot) {
            Ok(options_value) => {
                if let Err(error) = guard.replace_session_config_options(session_id, options_value)
                {
                    tracing::warn!(%error, session = %session_id, "config-option snapshot write failed");
                }
            }
            Err(error) => {
                tracing::warn!(%error, session = %session_id, "config-option snapshot serialize failed");
            }
        }
        Ok(snapshot)
    }
}

/// Which ACP method [`AgentSupervisor::attach_session`] sends for an existing
/// session and which event it records afterwards.
#[derive(Clone, Copy)]
enum SessionAttachKind {
    Load,
    Resume,
}

/// Fetch a session row and refuse closed ones. Runs before any bridge call:
/// returning 404 for an unknown id beats letting the agent reject with an
/// opaque error.
async fn fetch_open_session(
    state: &Arc<TokioMutex<StateStore>>,
    session_id: &str,
) -> Result<SessionRecord> {
    let record = {
        let guard = state.lock().await;
        guard.get_session(session_id)?
    }
    .ok_or_else(|| StackError::SessionNotFound {
        id: session_id.to_owned(),
    })?;
    reject_closed_session(&record)?;
    Ok(record)
}

/// Apply one resolved session config option (mode, model, or effort) to a freshly
/// created session.
///
/// Mode/model provisioning must not make sessions uncreatable when the agent
/// simply does not advertise the option: the session proceeds on the agent's
/// default and the omission is recorded in `ignored`. Only the
/// `AgentConfigProvision` lookup failure is softened — an error from
/// `set_session_config_option` itself means the agent advertised the option and
/// then failed the RPC, which stays a hard failure.
///
/// Returns the refreshed option list from the agent's response (`None` when
/// the set was skipped as ignored), so the caller's config-option snapshot
/// can reflect what was actually applied.
async fn provision_session_option(
    bridge: &AcpBridge,
    session_id: &AcpSessionId,
    lookup: Result<String>,
    value: &str,
    feature: &'static str,
    capability: &'static str,
    ignored: &mut Vec<IgnoredFeature>,
) -> Result<Option<Vec<SessionConfigOption>>> {
    provision_session_option_value(
        bridge,
        session_id,
        lookup,
        SessionConfigOptionValue::ValueId {
            value: SessionConfigValueId::new(value.to_owned()),
        },
        value.to_owned(),
        feature,
        capability,
        None,
        ignored,
    )
    .await
}

/// Resolve one `[agent.config_options]` entry against the advertised list.
/// Every miss shape — unknown id, kind mismatch, unadvertised select value —
/// is an `AgentConfigProvision` error, which the provisioning path softens
/// into an ignored record.
fn resolve_generic_config_option(
    advertised: Option<&[SessionConfigOption]>,
    option_id: &str,
    configured: &crate::config::AgentConfigOptionValue,
) -> Result<String> {
    use agent_client_protocol::schema::v1::SessionConfigKind;

    let provision_error = |reason: String| StackError::AgentConfigProvision {
        path: std::path::PathBuf::from("ACP session config options"),
        reason,
    };
    let option = advertised
        .unwrap_or_default()
        .iter()
        .find(|option| option.id.0.as_ref() == option_id)
        .ok_or_else(|| {
            provision_error(format!(
                "agent did not advertise config option `{option_id}`"
            ))
        })?;
    match (configured, &option.kind) {
        (crate::config::AgentConfigOptionValue::Bool(_), SessionConfigKind::Boolean(_)) => {
            Ok(option_id.to_owned())
        }
        (crate::config::AgentConfigOptionValue::Text(text), SessionConfigKind::Select(_)) => {
            if crate::runtime::agent::acp_codec::session_config_option_contains_value(option, text)
            {
                Ok(option_id.to_owned())
            } else {
                Err(provision_error(format!(
                    "agent did not advertise `{text}` as a value of config option `{option_id}`"
                )))
            }
        }
        (configured, kind) => Err(provision_error(format!(
            "config option `{option_id}` is configured as {} but advertised as {}",
            match configured {
                crate::config::AgentConfigOptionValue::Bool(_) => "a boolean",
                crate::config::AgentConfigOptionValue::Text(_) => "a select value",
            },
            match kind {
                SessionConfigKind::Boolean(_) => "a boolean option",
                SessionConfigKind::Select(_) => "a select option",
                _ => "an unsupported option kind",
            },
        ))),
    }
}

/// Kind-generic twin of `provision_session_option`, shared with the
/// `[agent.config_options]` map apply. `option_id` rides into the ignored
/// record for map entries, where `feature` alone cannot say which option was
/// dropped.
#[allow(clippy::too_many_arguments)]
async fn provision_session_option_value(
    bridge: &AcpBridge,
    session_id: &AcpSessionId,
    lookup: Result<String>,
    value: SessionConfigOptionValue,
    display_value: String,
    feature: &'static str,
    capability: &'static str,
    option_id: Option<String>,
    ignored: &mut Vec<IgnoredFeature>,
) -> Result<Option<Vec<SessionConfigOption>>> {
    match lookup {
        Ok(config_id) => {
            let response = bridge
                .set_session_config_option_value(session_id.clone(), &config_id, value)
                .await?;
            // A lax adapter may respond with an empty list and carry the
            // refreshed state only in a `config_option_update` notification;
            // an empty "full set" right after a successful set is
            // contradictory, so it must not become the snapshot.
            Ok((!response.config_options.is_empty()).then_some(response.config_options))
        }
        Err(StackError::AgentConfigProvision { reason, .. }) => {
            ignored.push(IgnoredFeature {
                feature,
                value: display_value,
                capability,
                reason,
                option_id,
            });
            Ok(None)
        }
        Err(other) => Err(other),
    }
}
