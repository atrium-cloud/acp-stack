use super::*;

pub(super) fn build_bootstrap_router(state: BootstrapState, max_request_bytes: u64) -> Router {
    Router::new()
        .route(
            "/v1/init/sessions",
            post(create_session_handler).layer(RequestBodyLimitLayer::new(
                config::IMPORT_REQUEST_SIZE_LIMIT,
            )),
        )
        .route("/v1/init/sessions/{id}", get(session_status_handler))
        .route("/v1/init/sessions/{id}/events", get(session_events_handler))
        .route(
            "/v1/init/sessions/{id}/cancel",
            post(session_cancel_handler),
        )
        .route(
            "/v1/init/sessions/{id}/native-config/cancel",
            post(session_native_config_cancel_handler),
        )
        .route("/v1/init/sessions/{id}/ws", get(session_ws_handler))
        .layer(RequestBodyLimitLayer::new(max_request_bytes as usize))
        .layer(axum::extract::DefaultBodyLimit::disable())
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_bootstrap_auth,
        ))
        .layer(middleware::from_fn(bootstrap_ensure_envelope))
        .with_state(state)
}

#[derive(Clone)]
pub(super) struct BootstrapState {
    pub(super) token: Arc<String>,
    pub(super) allowed_origins: Arc<Vec<String>>,
    pub(super) manager: Arc<HostedInitManager>,
    pub(super) native_config_mutation: Arc<TokioMutex<()>>,
}

async fn require_bootstrap_auth(
    State(state): State<BootstrapState>,
    req: Request<Body>,
    next: Next,
) -> Response {
    if !bootstrap_origin_allowed(req.headers(), &state.allowed_origins) {
        return api_error(
            StatusCode::FORBIDDEN,
            "auth.origin_not_allowed",
            "Origin is not in the configured allowlist",
        );
    }
    let mut auth_values = req.headers().get_all(http::header::AUTHORIZATION).iter();
    let header = match (auth_values.next(), auth_values.next()) {
        (None, _) => {
            return api_error(
                StatusCode::UNAUTHORIZED,
                "auth.missing",
                "missing bearer token",
            );
        }
        (Some(_), Some(_)) => {
            return api_error(
                StatusCode::UNAUTHORIZED,
                "auth.malformed_header",
                "duplicate Authorization headers are not allowed",
            );
        }
        (Some(only), None) => only,
    };
    let presented = parse_bearer(header);
    let Some(presented) = presented else {
        return api_error(
            StatusCode::UNAUTHORIZED,
            "auth.malformed_header",
            "Authorization header must be `Bearer <token>` with a single ASCII token",
        );
    };
    if !constant_time_eq(presented.as_bytes(), state.token.as_bytes()) {
        return api_error(
            StatusCode::UNAUTHORIZED,
            "auth.invalid",
            "invalid bearer token",
        );
    }
    // Only authenticated calls count as API activity; unauthenticated probes
    // must not keep an abandoned bootstrap server alive.
    state.manager.touch_activity();
    next.run(req).await
}

async fn bootstrap_ensure_envelope(req: Request<Body>, next: Next) -> Response {
    let response = next.run(req).await;
    let status = response.status();
    if !status.is_client_error() && !status.is_server_error() {
        return response;
    }
    let is_json = response
        .headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|content_type| content_type.starts_with("application/json"))
        .unwrap_or(false);
    if is_json {
        return response;
    }

    let (parts, _body) = response.into_parts();
    let mut new_response = ApiError::new(error_code_for_status(status), message_for_status(status))
        .into_response_with(status);
    for (name, value) in parts.headers.iter() {
        if name == http::header::CONTENT_TYPE || name == http::header::CONTENT_LENGTH {
            continue;
        }
        new_response
            .headers_mut()
            .append(name.clone(), value.clone());
    }
    new_response
}

fn parse_bearer(header: &http::HeaderValue) -> Option<String> {
    let text = header.to_str().ok()?;
    let token = text.strip_prefix("Bearer ")?;
    if token.is_empty() || token.chars().any(|character| character.is_whitespace()) {
        return None;
    }
    Some(token.to_owned())
}

fn bootstrap_origin_allowed(headers: &http::HeaderMap, allowed: &[String]) -> bool {
    let Some(origin) = headers
        .get(http::header::ORIGIN)
        .and_then(|value| value.to_str().ok())
    else {
        return true;
    };
    allowed
        .iter()
        .any(|allowed| allowed == "*" || allowed == origin)
}

async fn create_session_handler(
    State(state): State<BootstrapState>,
    body: Option<Json<StartInitRequest>>,
) -> Response {
    let init_args = match body.map(|body| body.0).unwrap_or_default().into_init_args() {
        Ok(init_args) => init_args,
        Err(error) => return error.into_response(),
    };
    match state.manager.start_session(init_args) {
        Ok(response) => ApiSuccess::new(response).into_response(),
        Err(StartSessionError::Active) => api_error(
            StatusCode::CONFLICT,
            "init.session_active",
            "an init session is already active",
        ),
    }
}

async fn session_status_handler(
    State(state): State<BootstrapState>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    match state.manager.session(&id) {
        Some(session) => {
            // Snapshot before touching: the reported `last_activity_age_secs`
            // is the idle time leading up to this poll, not the ~0 the poll's
            // own activity would produce.
            let snapshot = session.status_snapshot();
            session.touch();
            ApiSuccess::new(snapshot).into_response()
        }
        None => api_error(
            StatusCode::NOT_FOUND,
            "init.session_not_found",
            "init session not found",
        ),
    }
}

#[derive(Debug, Deserialize)]
struct EventsQuery {
    after_seq: Option<u64>,
}

async fn session_events_handler(
    State(state): State<BootstrapState>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<EventsQuery>,
) -> Response {
    match state.manager.session(&id) {
        Some(session) => {
            session.touch();
            ApiSuccess::new(InitEventsResponse {
                session_id: id,
                events: session.events_after(query.after_seq.unwrap_or(0)),
            })
            .into_response()
        }
        None => api_error(
            StatusCode::NOT_FOUND,
            "init.session_not_found",
            "init session not found",
        ),
    }
}

async fn session_cancel_handler(
    State(state): State<BootstrapState>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    match state.manager.session(&id) {
        Some(session) => {
            session.cancel("backend_cancel");
            ApiSuccess::new(SimpleSessionResponse {
                session_id: id,
                status: session.status(),
            })
            .into_response()
        }
        None => api_error(
            StatusCode::NOT_FOUND,
            "init.session_not_found",
            "init session not found",
        ),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeConfigCancelRequest {
    operation_id: String,
    revision: String,
}

async fn session_native_config_cancel_handler(
    State(state): State<BootstrapState>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<NativeConfigCancelRequest>,
) -> Response {
    let Some(session) = state.manager.session(&id) else {
        return api_error(
            StatusCode::NOT_FOUND,
            "init.session_not_found",
            "init session not found",
        );
    };
    session.touch();
    if session.status() != "completed_awaiting_ack" {
        return api_error(
            StatusCode::CONFLICT,
            "init.result_unavailable",
            "init session has no result awaiting acknowledgement",
        );
    }
    let _mutation = state.native_config_mutation.lock().await;
    let outcome = tokio::task::spawn_blocking(move || {
        let home = home_dir()?;
        let config_path = config::default_config_path()?;
        let state_path = default_state_path(&home);
        let _lock = acquire_agent_config_mutation_file_lock(&config_path)?;
        super::super::native_config::cancel_applied_for_init(
            &request.operation_id,
            &request.revision,
            &config_path,
            &state_path,
            &home,
        )
    })
    .await;
    match outcome {
        Ok(Ok(operation)) => ApiSuccess::new(operation).into_response(),
        Ok(Err(error)) => error.into_response(),
        Err(error) => StackError::NativeAgentConfig {
            code: if error.is_panic() {
                "native_config_lock_task_panicked"
            } else {
                "native_config_lock_task_cancelled"
            },
        }
        .into_response(),
    }
}

async fn session_ws_handler(
    State(state): State<BootstrapState>,
    AxumPath(id): AxumPath<String>,
    ws: WebSocketUpgrade,
) -> Response {
    let Some(session) = state.manager.session(&id) else {
        return api_error(
            StatusCode::NOT_FOUND,
            "init.session_not_found",
            "init session not found",
        );
    };
    ws.on_upgrade(move |socket| init_ws_connection(socket, session))
        .into_response()
}

async fn init_ws_connection(socket: WebSocket, session: Arc<HostedInitSession>) {
    // A connected client is liveness on its own: the idle reaper must not
    // fire while a backend holds the socket, even if it only listens during
    // long init steps. The guard keeps the count correct on every early
    // return below.
    struct ConnectionGuard {
        session: Arc<HostedInitSession>,
    }
    impl Drop for ConnectionGuard {
        fn drop(&mut self) {
            self.session.ws_disconnected();
        }
    }
    session.ws_connected();
    let _guard = ConnectionGuard {
        session: session.clone(),
    };
    let (mut sender, mut receiver) = socket.split();
    // Subscribe before snapshotting hello so no signal can slip between the two.
    // A frame emitted in that window rides the live stream and the client dedups
    // it against the hello replay by seq; the alternative order drops it from
    // both, and a raw-signal stream has no later full snapshot to self-correct.
    let mut events = session.subscribe();
    let hello = session.hello_frame();
    if sender.send(Message::Text(hello.into())).await.is_err() {
        return;
    }
    loop {
        tokio::select! {
            inbound = receiver.next() => {
                let Some(Ok(message)) = inbound else {
                    break;
                };
                if let Message::Text(text) = message {
                    session.touch();
                    let response = handle_client_frame(&session, text.as_str());
                    match response {
                        ClientFrameOutcome::None => {}
                        ClientFrameOutcome::Send(frame) => {
                            if sender.send(Message::Text(frame.into())).await.is_err() {
                                break;
                            }
                        }
                        ClientFrameOutcome::Close(frame) => {
                            let _ = sender.send(Message::Text(frame.into())).await;
                            let _ = sender.send(Message::Close(None)).await;
                            break;
                        }
                    }
                }
            }
            event = events.recv() => {
                match event {
                    Ok(frame) => {
                        if sender.send(Message::Text(frame.into())).await.is_err() {
                            break;
                        }
                        // A terminal session (reaper-cancelled, errored, or
                        // acked on another connection) means the server is on
                        // its way down; close instead of waiting for the
                        // client so a hung backend holding the socket cannot
                        // pin the process past --max-lifetime.
                        if !session.is_active() {
                            let _ = sender.send(Message::Close(None)).await;
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        // A lagged receiver skipped frames it can never get back
                        // from this stream. Re-send hello so the client re-folds
                        // the full signal-log replay: with raw signals a skipped
                        // frame is otherwise a permanent hole. The lag notice
                        // goes first so the client knows why a fresh hello landed.
                        if sender
                            .send(Message::Text(ws_lagged_frame().into()))
                            .await
                            .is_err()
                            || sender
                                .send(Message::Text(session.hello_frame().into()))
                                .await
                                .is_err()
                        {
                            break;
                        }
                        // A lagged receiver can miss the terminal event itself;
                        // check the session state directly.
                        if !session.is_active() {
                            let _ = sender.send(Message::Close(None)).await;
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
}

/// Extracted from the WebSocket loop because the only way to reach it there is
/// to fall a full broadcast buffer behind, which no test can drive reliably.
pub(super) fn ws_lagged_frame() -> String {
    frame_json(ServerFrame::ProtocolError {
        code: "init.ws_lagged",
        message: "websocket client lagged behind init event stream",
    })
}

pub(super) enum ClientFrameOutcome {
    None,
    Send(String),
    Close(String),
}

pub(super) fn handle_client_frame(
    session: &Arc<HostedInitSession>,
    text: &str,
) -> ClientFrameOutcome {
    let frame = match serde_json::from_str::<ClientFrame>(text) {
        Ok(frame) => frame,
        Err(error) => {
            return protocol_error("init.bad_frame", &format!("invalid client frame: {error}"));
        }
    };
    match frame.frame_type.as_str() {
        "input" => {
            let Some(request_id) = frame.request_id else {
                return protocol_error(
                    "init.missing_request_id",
                    "input frame requires request_id",
                );
            };
            let answer = HostedAnswer {
                value: frame.value.unwrap_or(Value::Null),
                deferred: frame.deferred.unwrap_or(false),
            };
            match session.submit_answer(&request_id, answer) {
                Ok(()) => ClientFrameOutcome::None,
                Err(message) => protocol_error("init.input_rejected", &message),
            }
        }
        "cancel" => {
            session.cancel(frame.reason.as_deref().unwrap_or("backend_cancel"));
            ClientFrameOutcome::None
        }
        "replay_result" => match session.result_frame() {
            Some(frame) => ClientFrameOutcome::Send(frame),
            None => protocol_error("init.result_unavailable", "init result is not available"),
        },
        "ack_result" => match session.ack_result() {
            Ok(()) => ClientFrameOutcome::Close(frame_json(ServerFrame::AckAccepted {
                session_id: &session.id,
            })),
            Err(message) => protocol_error("init.ack_rejected", &message),
        },
        "replay_error" => match session.error_replay_frame() {
            Some(frame) => ClientFrameOutcome::Send(frame),
            None => protocol_error(
                "init.error_unavailable",
                "no init error is recorded for this session",
            ),
        },
        "ack_error" => match session.ack_error() {
            Ok(()) => ClientFrameOutcome::Close(frame_json(ServerFrame::ErrorAckedClose {
                session_id: &session.id,
            })),
            Err(message) => protocol_error("init.ack_rejected", &message),
        },
        _ => protocol_error(
            "init.unsupported_frame",
            &format!("unsupported client frame `{}`", frame.frame_type),
        ),
    }
}

fn protocol_error(code: &str, message: &str) -> ClientFrameOutcome {
    ClientFrameOutcome::Send(frame_json(ServerFrame::ProtocolError { code, message }))
}

#[derive(Debug, Deserialize)]
struct ClientFrame {
    #[serde(rename = "type")]
    frame_type: String,
    request_id: Option<String>,
    value: Option<Value>,
    /// Sibling of `value` on an `input` frame: the answer is `false` because
    /// the hosting backend will run the confirmed work itself later, not
    /// because the operator declined. Only the testflight confirm reads it.
    deferred: Option<bool>,
    reason: Option<String>,
}

fn api_error(status: StatusCode, code: impl Into<String>, message: impl Into<String>) -> Response {
    ApiError::new(code, message).into_response_with(status)
}

fn error_code_for_status(status: StatusCode) -> &'static str {
    match status {
        StatusCode::BAD_REQUEST => "request.invalid",
        StatusCode::UNAUTHORIZED => "auth.invalid",
        StatusCode::FORBIDDEN => "auth.forbidden",
        StatusCode::NOT_FOUND => "not_found",
        StatusCode::METHOD_NOT_ALLOWED => "method_not_allowed",
        StatusCode::PAYLOAD_TOO_LARGE => "request.too_large",
        StatusCode::UNSUPPORTED_MEDIA_TYPE => "request.unsupported_media_type",
        _ if status.is_server_error() => "internal_error",
        _ => "request.rejected",
    }
}

fn message_for_status(status: StatusCode) -> &'static str {
    match status {
        StatusCode::BAD_REQUEST => "bad request",
        StatusCode::UNAUTHORIZED => "authentication required",
        StatusCode::FORBIDDEN => "forbidden",
        StatusCode::NOT_FOUND => "not found",
        StatusCode::METHOD_NOT_ALLOWED => "method not allowed",
        StatusCode::PAYLOAD_TOO_LARGE => "request body exceeds configured size limit",
        StatusCode::UNSUPPORTED_MEDIA_TYPE => "unsupported media type",
        _ if status.is_server_error() => "internal server error",
        _ => "request rejected",
    }
}
