use super::*;

pub(crate) async fn handle_initialize(
    state: SharedState,
    request: InitializeRequest,
    responder: Responder<InitializeResponse>,
    _connection: ConnectionTo<Client>,
) -> agent_client_protocol::Result<()> {
    let mut state = state.lock().await;
    if state.args.initialize_error {
        return responder.respond_with_error(Error::new(-32000, "fake initialize failure"));
    }
    if state.args.require_client_info {
        let Some(client_info) = request.client_info.as_ref() else {
            return responder.respond_with_error(Error::new(-32000, "missing clientInfo"));
        };
        if client_info.name != "acp-stack" || client_info.version != env!("CARGO_PKG_VERSION") {
            return responder.respond_with_error(Error::new(-32000, "unexpected clientInfo"));
        }
    }
    state.client_capabilities = Some(request.client_capabilities.clone());
    let mut session_capabilities = SessionCapabilities::new();
    if !state.args.no_cap_list_session {
        session_capabilities = session_capabilities.list(SessionListCapabilities::new());
    }
    if !state.args.no_cap_resume_session {
        session_capabilities = session_capabilities.resume(SessionResumeCapabilities::new());
    }
    if !state.args.no_cap_close_session {
        session_capabilities = session_capabilities.close(SessionCloseCapabilities::new());
    }
    if !state.args.no_cap_delete_session {
        session_capabilities = session_capabilities.delete(SessionDeleteCapabilities::new());
    }
    if !state.args.no_cap_fork_session {
        let mut fork = SessionForkCapabilities::new();
        if !state.args.no_cap_fork_message_id {
            let mut stack = serde_json::Map::new();
            stack.insert("messageId".to_owned(), serde_json::json!({}));
            let mut meta = serde_json::Map::new();
            meta.insert("acpStack".to_owned(), serde_json::Value::Object(stack));
            fork = fork.meta(meta);
        }
        session_capabilities = session_capabilities.fork(fork);
    }
    let mut capabilities = AgentCapabilities::new()
        .load_session(!state.args.no_cap_load_session)
        .prompt_capabilities(PromptCapabilities::new())
        .session_capabilities(session_capabilities);
    if state.args.cap_mcp_http {
        capabilities = capabilities.mcp_capabilities(McpCapabilities::new().http(true));
    }
    responder.respond(
        InitializeResponse::new(if state.args.initialize_protocol_v0 {
            ProtocolVersion::V0
        } else {
            ProtocolVersion::V1
        })
        .agent_capabilities(capabilities)
        .agent_info(
            Implementation::new("placebo-agent", env!("CARGO_PKG_VERSION"))
                .title(state.title.clone()),
        ),
    )
}

pub(crate) async fn handle_new_session(
    state: SharedState,
    request: NewSessionRequest,
    responder: Responder<NewSessionResponse>,
    _connection: ConnectionTo<Client>,
) -> agent_client_protocol::Result<()> {
    let mut state = state.lock().await;
    if state.args.session_new_error {
        return responder.respond_with_error(Error::new(-32000, "fake session/new failure"));
    }
    if state.args.session_new_stall {
        drop(state);
        loop {
            tokio::time::sleep(STALL_SLEEP).await;
        }
    }
    let session_id = format!("{}{}", FIXTURE_SESSION_PREFIX, state.next_session);
    state.next_session += 1;
    state.created_sessions.push(CreatedSession {
        id: session_id.clone(),
        cwd: request.cwd,
    });
    let response = NewSessionResponse::new(session_id.clone())
        .config_options(state.config_options(&session_id));
    responder.respond(response)
}

pub(crate) async fn handle_list_sessions(
    state: SharedState,
    request: ListSessionsRequest,
    responder: Responder<ListSessionsResponse>,
    _connection: ConnectionTo<Client>,
) -> agent_client_protocol::Result<()> {
    let state = state.lock().await;
    if state.args.session_list_repeated_cursor {
        return responder.respond(
            ListSessionsResponse::new(Vec::new()).next_cursor(REPEATED_CURSOR.to_owned()),
        );
    }
    if state.args.session_list_paginated && request.cursor.is_none() {
        let listed_cwd = state.args.listed_cwd.to_string_lossy();
        return responder.respond(
            ListSessionsResponse::new(vec![
                listed_session(
                    LISTED_PAGE_1_SESSION_ID,
                    &listed_cwd,
                    "listed page 1",
                    LISTED_UPDATED_AT,
                )
                .meta(origin_meta()),
            ])
            .next_cursor(LIST_PAGE_2_CURSOR.to_owned()),
        );
    }
    if state.args.session_list_paginated && request.cursor.as_deref() == Some(LIST_PAGE_2_CURSOR) {
        let listed_cwd = state.args.listed_cwd.to_string_lossy();
        return responder.respond(ListSessionsResponse::new(vec![
            listed_session(
                LISTED_PAGE_2_SESSION_ID,
                &listed_cwd,
                "listed page 2",
                LISTED_PAGE_2_UPDATED_AT,
            )
            .meta(origin_meta()),
        ]));
    }

    let listed_cwd = state.args.listed_cwd.to_string_lossy();
    let mut sessions = vec![
        listed_session(
            LISTED_SESSION_ID,
            &listed_cwd,
            "listed session",
            LISTED_UPDATED_AT,
        )
        .meta(origin_meta()),
    ];
    sessions.extend(state.created_sessions.iter().map(|session| {
        SessionInfo::new(session.id.clone(), session.cwd.clone())
            .title(format!("created {}", session.id))
            .updated_at(CREATED_UPDATED_AT.to_owned())
    }));
    responder.respond(ListSessionsResponse::new(sessions))
}

pub(crate) async fn handle_set_config_option(
    state: SharedState,
    request: SetSessionConfigOptionRequest,
    responder: Responder<SetSessionConfigOptionResponse>,
    connection: ConnectionTo<Client>,
) -> agent_client_protocol::Result<()> {
    let mut state = state.lock().await;
    if let SessionConfigOptionValue::ValueId { value } = &request.value
        && state.args.expect_model_config.as_deref() == Some(value.0.as_ref())
        && request.config_id.0.as_ref() == state.args.model_config_option_id.as_str()
    {
        state.model_configured = true;
    }
    state.config_option_values.insert(
        (
            request.session_id.0.to_string(),
            request.config_id.0.to_string(),
        ),
        request.value.clone(),
    );
    let refreshed = state
        .config_options(request.session_id.0.as_ref())
        .unwrap_or_default();
    if state.args.emit_config_option_update {
        connection.send_notification(SessionNotification::new(
            request.session_id.clone(),
            SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(refreshed.clone())),
        ))?;
    }
    let responded = if state.args.set_config_option_responds_empty {
        Vec::new()
    } else {
        refreshed
    };
    responder.respond(SetSessionConfigOptionResponse::new(responded))
}

pub(crate) async fn handle_load_session(
    _state: SharedState,
    _request: LoadSessionRequest,
    responder: Responder<LoadSessionResponse>,
    _connection: ConnectionTo<Client>,
) -> agent_client_protocol::Result<()> {
    responder.respond(LoadSessionResponse::new())
}

pub(crate) async fn handle_resume_session(
    state: SharedState,
    request: ResumeSessionRequest,
    responder: Responder<ResumeSessionResponse>,
    _connection: ConnectionTo<Client>,
) -> agent_client_protocol::Result<()> {
    let state = state.lock().await;
    responder.respond(
        ResumeSessionResponse::new()
            .config_options(state.config_options(request.session_id.0.as_ref())),
    )
}

pub(crate) async fn handle_close_session(
    _state: SharedState,
    _request: CloseSessionRequest,
    responder: Responder<CloseSessionResponse>,
    _connection: ConnectionTo<Client>,
) -> agent_client_protocol::Result<()> {
    responder.respond(CloseSessionResponse::new())
}

pub(crate) async fn handle_delete_session(
    state: SharedState,
    _request: DeleteSessionRequest,
    responder: Responder<DeleteSessionResponse>,
    _connection: ConnectionTo<Client>,
) -> agent_client_protocol::Result<()> {
    if state.lock().await.args.fail_delete_session {
        return responder.respond_with_error(Error::new(
            -32000,
            "placebo agent refuses session/delete".to_owned(),
        ));
    }
    responder.respond(DeleteSessionResponse::new())
}

pub(crate) async fn handle_fork_session(
    state: SharedState,
    request: ForkSessionRequest,
    responder: Responder<ForkSessionResponse>,
    _connection: ConnectionTo<Client>,
) -> agent_client_protocol::Result<()> {
    let ForkSessionRequest {
        session_id: _parent_session_id,
        cwd,
        additional_directories: _additional_directories,
        mcp_servers: _mcp_servers,
        meta,
        ..
    } = request;
    let message_id = meta
        .as_ref()
        .and_then(|meta| meta.get("acpStack"))
        .and_then(|stack| stack.get("messageId"))
        .and_then(serde_json::Value::as_str);
    let mut state = state.lock().await;
    if let Some(expected) = state.args.expect_fork_message_id.as_deref()
        && message_id != Some(expected)
    {
        return responder.respond_with_error(Error::new(
            -32000,
            format!("expected fork message id {expected}"),
        ));
    }
    let session_id = format!("{}{}", FIXTURE_SESSION_PREFIX, state.next_session);
    state.next_session += 1;
    state.created_sessions.push(CreatedSession {
        id: session_id.clone(),
        cwd,
    });
    responder.respond(ForkSessionResponse::new(session_id))
}

pub(crate) async fn handle_cancel(
    state: SharedState,
    notification: CancelNotification,
    _connection: ConnectionTo<Client>,
) -> agent_client_protocol::Result<()> {
    let mut state = state.lock().await;
    state
        .cancelled_sessions
        .insert(notification.session_id.0.to_string());
    Ok(())
}

fn listed_session(id: &str, cwd: &str, title: &str, updated_at: &str) -> SessionInfo {
    SessionInfo::new(id.to_owned(), PathBuf::from(cwd))
        .title(title.to_owned())
        .updated_at(updated_at.to_owned())
}

fn origin_meta() -> serde_json::Map<String, serde_json::Value> {
    let mut meta = serde_json::Map::new();
    meta.insert(
        "origin".to_owned(),
        serde_json::Value::String(FIXTURE_ORIGIN.to_owned()),
    );
    meta
}
