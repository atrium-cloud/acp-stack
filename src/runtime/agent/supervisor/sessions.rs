//! ACP `session/*` dispatch and the durable `sessions` rows that mirror it. Every method
//! persists only after the agent confirms; a local row must never exist for a rejected session.

use super::*;
use crate::runtime::agent::acp_codec::session_config_option_is_current;

impl AgentSupervisor {
    /// Sync ACP `session/list` results into local state; discovery only, rows land `available`.
    pub async fn sync_listed_sessions(
        &self,
        target_id: &str,
        agent: &AgentConfig,
        workspace_root: &str,
        state: &StateHandle,
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
        let target_id = target_id.to_owned();
        let agent_id = agent.id.clone();
        let counts = state
            .write(move |store| {
                store.record_session_list_sync(&target_id, &agent_id, records, skipped_invalid_cwd)
            })
            .await?;
        Ok(SessionListSyncResult {
            attempted: true,
            status: SessionListSyncStatus::Synced,
            upserted: counts.upserted,
            updated: counts.updated,
        })
    }

    /// `POST /v1/sessions`. Dispatches ACP `session/new` and persists a `sessions` row;
    /// mode/model/effort values the agent did not advertise land in `ignored`, not an error.
    pub async fn create_session(
        &self,
        target_id: &str,
        agent: &AgentConfig,
        workspace_root: &str,
        cwd: Option<String>,
        mcp_servers: Vec<McpServer>,
        state: &StateHandle,
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
        // The snapshot must reflect what was applied, not the pre-provisioning
        // `session/new` state.
        let mut latest_options: Option<Vec<SessionConfigOption>> = None;
        if let Some(mode) = agent.mode.as_deref() {
            match session_mode_selection_for_value(&response, mode) {
                Ok(AgentSessionModeSelection::ConfigOption { config_id }) => {
                    if let Some(refreshed) = provision_session_option(
                        &bridge,
                        &response.session_id,
                        response.config_options.as_deref(),
                        Ok(config_id),
                        mode,
                        IGNORED_FEATURE_AGENT_MODE,
                        "sessionConfig.mode",
                        &mut ignored,
                    )
                    .await?
                    {
                        latest_options = Some(refreshed);
                    }
                }
                Ok(AgentSessionModeSelection::NativeMode { mode_id }) => {
                    let already_current = response
                        .modes
                        .as_ref()
                        .is_some_and(|modes| modes.current_mode_id.0.as_ref() == mode_id);
                    // `session/set_mode` returns no config options, so the
                    // snapshot the model/effort lanes read stays untouched.
                    if already_current {
                        tracing::debug!(
                            mode_id,
                            "session/new already reports the configured mode; skipping session/set_mode"
                        );
                    } else {
                        bridge
                            .set_session_mode(response.session_id.clone(), &mode_id)
                            .await?;
                    }
                }
                // Same softened-miss contract as the config-option lanes: an
                // unadvertised mode lands in `ignored`, a real error propagates.
                Err(error) => {
                    provision_session_option(
                        &bridge,
                        &response.session_id,
                        response.config_options.as_deref(),
                        Err(error),
                        mode,
                        IGNORED_FEATURE_AGENT_MODE,
                        "sessionConfig.mode",
                        &mut ignored,
                    )
                    .await?;
                }
            }
        }
        if let Some(model) = agent.model.as_deref().or_else(|| {
            agent
                .provider
                .as_ref()
                .and_then(|provider| provider.model.as_deref())
        }) {
            if model_applies_from_disk_only(agent) {
                // The harness reads this pin from on-disk config at process start, so the
                // adapter's advertised list is an echo at best and a set can only fail spuriously.
                tracing::debug!(
                    model,
                    "model provisioned on disk; skipping session/set_config_option"
                );
            } else {
                let advertised = latest_options
                    .as_deref()
                    .or(response.config_options.as_deref());
                let lookup = session_config_id_for_value(
                    advertised,
                    AgentSessionConfigCategory::Model,
                    model,
                );
                if let Some(refreshed) = provision_session_option(
                    &bridge,
                    &response.session_id,
                    advertised,
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
        // Must run after the model: adapters advertise effort levels for the currently
        // selected model, so an effort option can appear only once the model is set.
        if let Some(effort) = agent.effort.as_deref()
            && effort_value_is_explicit_without_discovery(agent)
        {
            // The pin lives in the harness's on-disk config; the adapter advertises no
            // effort option for this model, so a set would only land in `ignored`.
            tracing::debug!(
                effort,
                "effort provisioned on disk; skipping session/set_config_option"
            );
        } else if let Some(effort) = agent.effort.as_deref() {
            let advertised = latest_options
                .as_deref()
                .or(response.config_options.as_deref());
            if let Some(refreshed) = provision_session_option(
                &bridge,
                &response.session_id,
                advertised,
                session_config_id_for_value(advertised, AgentSessionConfigCategory::Effort, effort),
                effort,
                IGNORED_FEATURE_AGENT_EFFORT,
                "sessionConfig.effort",
                &mut ignored,
            )
            .await?
            {
                latest_options = Some(refreshed);
            }
        }
        // The generic map applies last so the typed lanes stay authoritative for their
        // categories; any miss is an ignored record, never an error.
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
                advertised,
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

        // Persist only after the agent confirms; inserting first would leave a phantom row.
        let record = NewSessionRecord {
            id: next_session_id(),
            agent_id: agent.id.clone(),
            cwd: resolved_cwd,
            title: None,
            metadata_json: "{}".to_owned(),
        };
        let target_id = target_id.to_owned();
        let agent_id = agent.id.clone();
        let advertised = latest_options
            .or(response.config_options)
            .unwrap_or_default();
        let job_ignored = ignored.clone();
        // Only the row insert and its read-back propagate: once the agent confirmed
        // `session/new`, a failed audit or snapshot write must not drop the session row.
        let record = state
            .write(move |store| {
                let inserted =
                    store.insert_session_for_target(&target_id, agent_session_id.clone(), record)?;
                if let Err(error) = store.append_session_event(
                    &inserted.id,
                    "info",
                    "session.created",
                    "session created",
                    &json!({
                        "target_id": &target_id,
                        "agent_id": &agent_id,
                        "agent_session_id": &agent_session_id,
                        "cwd": &inserted.cwd,
                    })
                    .to_string(),
                ) {
                    tracing::warn!(%error, session = %inserted.id, "session.created event write failed");
                }
                if let Err(error) = append_mcp_skipped_event(store, &inserted.id, &skipped) {
                    tracing::warn!(%error, session = %inserted.id, "mcp skipped event write failed");
                }
                if let Err(error) =
                    append_capability_ignored_event(store, &inserted.id, &job_ignored)
                {
                    tracing::warn!(%error, session = %inserted.id, "capability ignored event write failed");
                }
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
                        if let Err(error) = store.replace_session_config_options(&inserted.id, value)
                        {
                            tracing::warn!(%error, session = %inserted.id, "config-option snapshot write failed");
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%error, session = %inserted.id, "config-option snapshot serialize failed");
                    }
                }
                // Re-read so the response carries the change and event positions the
                // writes above advanced.
                store
                    .get_session(&inserted.id)?
                    .ok_or_else(|| StackError::SessionNotFound {
                        id: inserted.id.clone(),
                    })
            })
            .await?;
        Ok(SessionAttachOutcome {
            record,
            attached_mcp: accepted_names,
            ignored,
        })
    }

    /// `POST /v1/sessions/{id}/load`. `ignored` is always empty: provisioning happens at create.
    pub async fn load_session(
        &self,
        session_id: &str,
        cwd: Option<String>,
        mcp_servers: Vec<McpServer>,
        workspace_root: &str,
        state: &StateHandle,
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

    /// `POST /v1/sessions/{id}/resume`. `ignored` is always empty: provisioning happens at create.
    pub async fn resume_session(
        &self,
        session_id: &str,
        cwd: Option<String>,
        mcp_servers: Vec<McpServer>,
        workspace_root: &str,
        state: &StateHandle,
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

    /// Shared body of [`Self::load_session`] and [`Self::resume_session`], also
    /// used by the prompt path to re-attach a session the running adapter has
    /// not opened.
    pub(super) async fn attach_session(
        &self,
        session_id: &str,
        cwd: Option<String>,
        mcp_servers: Vec<McpServer>,
        workspace_root: &str,
        state: &StateHandle,
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
        let (event_kind, event_message) = match kind {
            SessionAttachKind::Load => ("session.loaded", "session loaded"),
            SessionAttachKind::Resume => ("session.resumed", "session resumed"),
        };
        let session_id = session_id.to_owned();
        // Only the status flip and its read-back propagate: once the agent attached the
        // session, a failed audit write must not leave the row out of date.
        let record = state
            .write(move |store| {
                if explicit_cwd {
                    store.update_session_status_and_cwd(
                        &session_id,
                        SESSION_STATUS_ACTIVE,
                        &resolved_cwd,
                    )?;
                } else {
                    store.update_session_status(&session_id, SESSION_STATUS_ACTIVE)?;
                }
                if let Err(error) = append_mcp_skipped_event(store, &session_id, &skipped) {
                    tracing::warn!(%error, session = %session_id, "mcp skipped event write failed");
                }
                if let Err(error) = store.append_session_event(
                    &session_id,
                    "info",
                    event_kind,
                    event_message,
                    &json!({ "agent_session_id": record.agent_session_id, "cwd": resolved_cwd })
                        .to_string(),
                ) {
                    tracing::warn!(%error, session = %session_id, event_kind, "session attach event write failed");
                }
                store
                    .get_session(&session_id)?
                    .ok_or_else(|| StackError::SessionNotFound { id: session_id })
            })
            .await?;
        Ok(SessionAttachOutcome {
            record,
            attached_mcp: accepted_names,
            ignored: Vec::new(),
        })
    }

    /// Translate the public `message_id` (always an acp-stack prompt message
    /// id) into the fork point the running adapter reads.
    ///
    /// The contract is the same either way: the fork ends just before the named
    /// prompt. The in-house adapters implement that cut themselves from the
    /// acpStack key. The AIR adapters keep the point they are given, so they are
    /// handed the anchor from the turn before instead, which leaves the named
    /// prompt and everything after it out of the fork.
    async fn resolve_fork_point(
        &self,
        parent_session_id: &str,
        message_id: &str,
        state: &StateHandle,
    ) -> Result<BreakpointFork> {
        let job_parent_session_id = parent_session_id.to_owned();
        let job_message_id = message_id.to_owned();
        let (prompt, preceding) = state
            .read(move |store| {
                let parent_session_id = job_parent_session_id;
                let message_id = job_message_id;
                let prompt = store
                    .get_prompt_by_message_id(&parent_session_id, &message_id)?
                    .ok_or_else(|| StackError::InvalidParam {
                        field: "message_id",
                        reason: format!(
                            "session `{parent_session_id}` has no prompt with message id `{message_id}`"
                        ),
                    })?;
                let preceding = store.preceding_prompt(&parent_session_id, &prompt.id)?;
                Ok((prompt, preceding))
            })
            .await?;
        if !prompt.message_id_acknowledged {
            return Err(StackError::InvalidParam {
                field: "message_id",
                reason: format!("message id `{message_id}` was not acknowledged by the agent"),
            });
        }
        let fork_point = match self.fork_point_dialect().await {
            ForkPointDialect::AcpStack => ForkPoint::AcpStackMessageId(message_id.to_owned()),
            ForkPointDialect::JetbrainsAir => {
                // The named prompt opens the session, so there is no turn to
                // keep. A fork of nothing is a new session, which the client
                // asks for through `POST /v1/sessions` instead.
                let preceding = preceding.as_ref().ok_or_else(|| StackError::InvalidParam {
                    field: "message_id",
                    reason: format!(
                        "message id `{message_id}` names the first prompt of session `{parent_session_id}`, and this agent cannot fork at a point with no preceding turn"
                    ),
                })?;
                preceding
                    .agent_message_id
                    .clone()
                    .map(ForkPoint::AirMessageId)
                    .ok_or_else(|| StackError::InvalidParam {
                        field: "message_id",
                        reason: format!(
                            "the turn before message id `{message_id}` produced no agent message this agent can be forked at"
                        ),
                    })?
            }
        };
        Ok(BreakpointFork {
            fork_point,
            held_through_prompt_id: preceding.map(|preceding| preceding.id),
        })
    }

    /// `POST /v1/sessions/{id}/fork`. `ignored` is always empty: provisioning happens at create.
    ///
    /// ACP `session/fork` streams no history, so the child's durable record is
    /// written here from the parent's: the turns the fork holds, cut at the
    /// same prompt the adapter cuts at (see `StateStore::insert_forked_session`).
    pub async fn fork_session(
        &self,
        parent_session_id: &str,
        cwd: Option<String>,
        mcp_servers: Vec<McpServer>,
        workspace_root: &str,
        message_id: Option<String>,
        state: &StateHandle,
    ) -> Result<SessionAttachOutcome> {
        let bridge = self.bridge().await?;
        let parent = fetch_open_session(state, parent_session_id).await?;
        let breakpoint_message_id = message_id;
        // Taken before dispatch: a head fork holds the turns settled by the
        // time the adapter is asked, so a turn still in flight, or one
        // submitted while the fork is outstanding, stays with the parent.
        let (fork_point, held_through_prompt_id) = match breakpoint_message_id.as_deref() {
            Some(message_id) => {
                let breakpoint = self
                    .resolve_fork_point(parent_session_id, message_id, state)
                    .await?;
                (
                    Some(breakpoint.fork_point),
                    breakpoint.held_through_prompt_id,
                )
            }
            None => {
                let job_parent_session_id = parent_session_id.to_owned();
                let held_through_prompt_id = state
                    .read(move |store| store.newest_settled_prompt_id(&job_parent_session_id))
                    .await?;
                (None, held_through_prompt_id)
            }
        };
        let resolved_cwd = resolve_session_cwd(
            Some(cwd.unwrap_or_else(|| stored_or_workspace_cwd(&parent.cwd, workspace_root))),
            workspace_root,
        )?;
        let parent_agent_session_id = parent.agent_session_id.clone();
        let PartitionedMcpServers { accepted, skipped } =
            bridge.capabilities().partition_mcp_servers(mcp_servers)?;
        let accepted_names = crate::runtime::agent::mcp::server_names(&accepted);
        // The held turns have settled, so their trailing notifications are
        // already queued; they must be durable before the parent's log is
        // copied.
        bridge.drain_session_events().await;
        let response = bridge
            .fork_session(
                AcpSessionId::new(parent_agent_session_id.clone()),
                PathBuf::from(&resolved_cwd),
                accepted,
                fork_point,
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
        let parent_target_id = parent.target_id.clone();
        let parent_session_id = parent_session_id.to_owned();
        // Only the child row insert and its read-back propagate: once the agent
        // confirmed `session/fork`, a failed audit write must not drop the child row.
        let record = state
            .write(move |store| {
                let inserted = store.insert_forked_session(
                    &parent_target_id,
                    child_agent_session_id.clone(),
                    record,
                    &parent_session_id,
                    held_through_prompt_id.as_deref(),
                )?;
                let payload = json!({
                    "target_id": &parent_target_id,
                    "parent_session_id": &parent_session_id,
                    "parent_agent_session_id": &parent_agent_session_id,
                    "child_session_id": &child_session_id,
                    "child_agent_session_id": &child_agent_session_id,
                    "strategy": "acp_native",
                    "message_id": &breakpoint_message_id,
                    "cwd": &inserted.cwd,
                })
                .to_string();
                if let Err(error) = store.append_session_event(
                    &inserted.id,
                    "info",
                    "session.forked",
                    "session forked",
                    &payload,
                ) {
                    tracing::warn!(%error, session = %inserted.id, "session.forked event write failed");
                }
                if let Err(error) = store.append_session_event(
                    &parent_session_id,
                    "info",
                    "session.fork.created_child",
                    "session fork child created",
                    &payload,
                ) {
                    tracing::warn!(%error, session = %parent_session_id, "session.fork.created_child event write failed");
                }
                if let Err(error) = append_mcp_skipped_event(store, &inserted.id, &skipped) {
                    tracing::warn!(%error, session = %inserted.id, "mcp skipped event write failed");
                }
                store
                    .get_session(&inserted.id)?
                    .ok_or_else(|| StackError::SessionNotFound {
                        id: inserted.id.clone(),
                    })
            })
            .await?;
        Ok(SessionAttachOutcome {
            record,
            attached_mcp: accepted_names,
            ignored: Vec::new(),
        })
    }

    /// `DELETE /v1/sessions/{id}`. Ordering matters: `session/close` goes to the agent first,
    /// and only on success are local prompts cancelled and the row marked closed.
    pub async fn close_session(
        &self,
        session_id: &str,
        state: &StateHandle,
    ) -> Result<SessionRecord> {
        let bridge = self.bridge().await?;
        let lookup_session_id = session_id.to_owned();
        let record = state
            .read(move |store| store.get_session(&lookup_session_id))
            .await?
            .ok_or_else(|| StackError::SessionNotFound {
                id: session_id.to_owned(),
            })?;
        let agent_session_id = record.agent_session_id;
        bridge
            .close_session(AcpSessionId::new(agent_session_id.clone()))
            .await?;
        self.cancel_prompts_for_session(session_id).await;
        let session_id = session_id.to_owned();
        state
            .write(move |store| {
                store.update_session_status(&session_id, SESSION_STATUS_CLOSED)?;
                if let Err(error) = store.append_session_event(
                    &session_id,
                    "info",
                    "session.closed",
                    "session closed",
                    &json!({ "agent_session_id": agent_session_id }).to_string(),
                ) {
                    tracing::warn!(%error, session = %session_id, "session.closed event write failed");
                }
                store
                    .get_session(&session_id)?
                    .ok_or_else(|| StackError::SessionNotFound { id: session_id })
            })
            .await
    }

    /// `POST /v1/sessions/{id}/delete`. The agent confirms first, then the local row and its
    /// prompts/events are hard-deleted; an unknown id returns `Ok(None)` per ACP's silent-repeat rule.
    pub async fn delete_session(
        &self,
        session_id: &str,
        state: &StateHandle,
        permissions: &PermissionService,
        permission_reason: &str,
    ) -> Result<Option<SessionRecord>> {
        let lookup_session_id = session_id.to_owned();
        let agent_session_id = match state
            .read(move |store| store.get_session(&lookup_session_id))
            .await?
        {
            Some(record) => record.agent_session_id,
            None => return Ok(None),
        };
        let bridge = self.bridge().await?;
        bridge
            .delete_session(AcpSessionId::new(agent_session_id))
            .await?;
        self.cancel_prompts_for_session(session_id).await;
        // Settled before the wipe, so the decision events land in the log the
        // delete removes instead of opening a new one under the deleted id.
        if let Err(error) = permissions
            .cancel_pending_for_session(session_id, permission_reason)
            .await
        {
            tracing::warn!(
                %error,
                session_id,
                "failed to cancel pending ACP permissions before session delete"
            );
        }
        let session_id = session_id.to_owned();
        state
            .write(move |store| store.delete_session(&session_id))
            .await
    }

    /// `POST /v1/sessions/{id}/config-options`. Forwards one `session/set_config_option` and
    /// rewrites the stored snapshot; unlike create-time provisioning, a failure is not softened.
    pub async fn set_session_config_option(
        &self,
        session_id: &str,
        config_id: &str,
        value: SessionConfigOptionValue,
        state: &StateHandle,
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
        // Lax adapters return an empty list after a successful set and refresh via
        // `config_option_update` instead; that must not wipe the snapshot.
        let snapshot = (!response.config_options.is_empty()).then(|| {
            let mut snapshot = crate::runtime::agent::config_options::project_config_options(
                &response.config_options,
            );
            snapshot.truncate(crate::state::MAX_SESSION_CONFIG_OPTIONS);
            snapshot
        });
        let options_value = match snapshot.as_ref().map(serde_json::to_value) {
            Some(Ok(options_value)) => Some(options_value),
            Some(Err(error)) => {
                tracing::warn!(%error, session = %session_id, "config-option snapshot serialize failed");
                None
            }
            None => None,
        };
        let session_id = session_id.to_owned();
        let event_payload = json!({ "config_id": config_id }).to_string();
        state
            .write(move |store| {
                store.append_session_event(
                    &session_id,
                    "info",
                    "session.config_option_set",
                    "session config option set",
                    &event_payload,
                )?;
                if let Some(options_value) = options_value
                    && let Err(error) =
                        store.replace_session_config_options(&session_id, options_value)
                {
                    tracing::warn!(%error, session = %session_id, "config-option snapshot write failed");
                }
                Ok(())
            })
            .await?;
        Ok(snapshot.unwrap_or_default())
    }
}

/// A breakpoint resolved for the running adapter.
struct BreakpointFork {
    fork_point: ForkPoint,
    /// The parent's last prompt the fork holds: the one before the named
    /// prompt, `None` when the named prompt opens the session.
    held_through_prompt_id: Option<String>,
}

/// Which ACP method [`AgentSupervisor::attach_session`] sends for an existing session.
#[derive(Clone, Copy)]
pub(super) enum SessionAttachKind {
    Load,
    Resume,
}

impl SessionAttachKind {
    pub(super) fn acp_method(self) -> &'static str {
        match self {
            Self::Load => "session/load",
            Self::Resume => "session/resume",
        }
    }
}

/// Fetch a session row and refuse closed ones, before any bridge call is made.
async fn fetch_open_session(state: &StateHandle, session_id: &str) -> Result<SessionRecord> {
    let lookup_session_id = session_id.to_owned();
    let record = state
        .read(move |store| store.get_session(&lookup_session_id))
        .await?
        .ok_or_else(|| StackError::SessionNotFound {
            id: session_id.to_owned(),
        })?;
    reject_closed_session(&record)?;
    Ok(record)
}

/// Apply one resolved session config option to a freshly created session. Only an
/// `AgentConfigProvision` lookup failure is softened into `ignored`; a failed RPC still errors.
#[allow(clippy::too_many_arguments)]
async fn provision_session_option(
    bridge: &AcpBridge,
    session_id: &AcpSessionId,
    advertised: Option<&[SessionConfigOption]>,
    lookup: Result<String>,
    value: &str,
    feature: &'static str,
    capability: &'static str,
    ignored: &mut Vec<IgnoredFeature>,
) -> Result<Option<Vec<SessionConfigOption>>> {
    provision_session_option_value(
        bridge,
        session_id,
        advertised,
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

/// Resolve one `[agent.config_options]` entry against the advertised list; every miss shape
/// is an `AgentConfigProvision` error, which the provisioning path softens into `ignored`.
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

/// Kind-generic twin of `provision_session_option`, shared with the `[agent.config_options]` map.
/// A value `advertised` already reports as current is not set again.
#[allow(clippy::too_many_arguments)]
async fn provision_session_option_value(
    bridge: &AcpBridge,
    session_id: &AcpSessionId,
    advertised: Option<&[SessionConfigOption]>,
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
            if session_config_option_is_current(advertised, &config_id, &value) {
                tracing::debug!(
                    config_id,
                    value = %display_value,
                    "session/new already reports the configured value; skipping session/set_config_option"
                );
                return Ok(None);
            }
            let response = bridge
                .set_session_config_option_value(session_id.clone(), &config_id, value)
                .await?;
            // Lax adapters answer with an empty list and carry the refreshed state only in a
            // `config_option_update`; that must not become the snapshot.
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
