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
        .route("/v1/init/sessions/{id}/input", post(session_input_handler))
        .route(
            "/v1/init/sessions/{id}/cancel",
            post(session_cancel_handler),
        )
        .route(
            "/v1/init/sessions/{id}/native-config/cancel",
            post(session_native_config_cancel_handler),
        )
        .route("/v1/init/sessions/{id}/ws", get(session_ws_handler))
        // In-stream provider-credential deposit: the hosting platform pushes
        // the opaque capsule secret and the managed selection mid-session so
        // model discovery resolves refs live instead of soft-passing them.
        .route("/v1/init/credential", post(deposit_credential_handler))
        // Same picker data as the session-tier `/v1/models` (shared handler
        // logic in `api::routes::providers`), served here so a hosted backend
        // can render model/mode choices while init is still running.
        .route("/v1/models", get(bootstrap_models_handler))
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
    /// The serve process's single writer-visible store handle, shared with the
    /// session wizard thread. Deliberately not the agent-config mutation
    /// flock: the wizard holds that flock for its whole run, so the deposit
    /// route serializes against the wizard through this mutex alone.
    pub(super) secret_store: SharedSecretStore,
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
    // Only authenticated calls count as activity, so probes cannot keep an abandoned server alive.
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
    let mut new_response = ApiError::for_status(status).into_response_with(status);
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
            // Snapshot before touching, so `last_activity_age_secs` reports the idle time leading
            // up to this poll rather than the ~0 the poll itself would produce.
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

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(super) struct EventsQuery {
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

/// REST twin of the WebSocket `input` client frame: both land in
/// `submit_answer`, so the wizard thread parses the answer through the same
/// prompt-driver logic regardless of transport. The socket reports a
/// rejection as an `init.input_rejected` protocol-error frame; here it is a
/// 409 with the same code, matching the router's other state conflicts.
async fn session_input_handler(
    State(state): State<BootstrapState>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<SessionInputRequest>,
) -> Response {
    let Some(session) = state.manager.session(&id) else {
        return api_error(
            StatusCode::NOT_FOUND,
            "init.session_not_found",
            "init session not found",
        );
    };
    session.touch();
    let answer = HostedAnswer {
        value: request.value,
        deferred: request.deferred,
    };
    match session.submit_answer(&request.request_id, answer) {
        Ok(()) => ApiSuccess::new(InputAcceptedResponse {
            request_id: request.request_id,
        })
        .into_response(),
        Err(message) => api_error(StatusCode::CONFLICT, "init.input_rejected", message),
    }
}

/// Init-tier `GET /v1/models`: the session-tier discovery with the same
/// fresh-from-disk config read, resolved without an `AppState` (the bootstrap
/// server has none). `?target_id=` (alias `target`) picks the Array target
/// exactly as on the session tier.
async fn bootstrap_models_handler(Query(query): Query<ModelsParams>) -> Response {
    let config_path = match config::default_config_path() {
        Ok(path) => path,
        Err(error) => return error.into_response(),
    };
    // Fresh init writes the runtime config early in the run; a picker call
    // before that stage would read a missing file. Report it as a retryable
    // not-ready state instead of an opaque 500.
    if !config_path.exists() {
        return api_error(
            StatusCode::CONFLICT,
            "init.config_not_ready",
            "init has not written the runtime config yet; retry once setup progresses past config staging",
        );
    }
    // The bootstrap init server runs without an `AppState`, so HOME is resolved here.
    let home = match home_dir() {
        Ok(home) => home,
        Err(error) => return error.into_response(),
    };
    let config = match load_runtime_config_from_disk(&config_path, &home) {
        Ok(config) => config,
        Err(error) => return error.into_response(),
    };
    let config = match resolve_models_target_config(config, query.target_id.as_deref()) {
        Ok(config) => config,
        Err(error) => return error.into_response(),
    };
    match models_response_for_config(&home, &config).await {
        Ok(models) => ApiSuccess::new(models).into_response(),
        Err(error) => error.into_response(),
    }
}

/// `POST /v1/init/credential`: write flat secrets and apply a managed
/// credential selection under one store lock, so fresh-from-disk readers
/// (model discovery) observe the capsule env ref and the managed endpoint
/// override together. Revision, identical-replay, and ownership semantics are
/// the store's, identical to the admin-tier apply route; the bootstrap token
/// is the only authorization difference.
async fn deposit_credential_handler(
    State(state): State<BootstrapState>,
    Json(request): Json<DepositCredentialRequest>,
) -> Response {
    if let Err(error) = request.validate() {
        return error.into_response();
    }
    let config_path = match config::default_config_path() {
        Ok(path) => path,
        Err(error) => return error.into_response(),
    };
    // The apply validates the selection against the configured agent/provider,
    // which exists only after init stages the starter config. Report the early
    // window as a retryable not-ready, matching the models route.
    if !config_path.exists() {
        return api_error(
            StatusCode::CONFLICT,
            "init.config_not_ready",
            "init has not written the runtime config yet; retry once setup progresses past config staging",
        );
    }
    let home = match home_dir() {
        Ok(home) => home,
        Err(error) => return error.into_response(),
    };
    let runtime_config = match load_runtime_config_from_disk(&config_path, &home) {
        Ok(config) => config,
        Err(error) => return error.into_response(),
    };
    if let Err(error) =
        crate::extensions::require_managed_state(&runtime_config, &request.namespace)
    {
        return error.into_response();
    }
    let secrets_written = request.secrets.len();
    let namespace = request.namespace.clone();
    let store = state.secret_store.clone();
    // The locked section blocks (whole-file encrypt + persist), so it runs off
    // the async workers; the closure never awaits while holding the guard. The
    // wizard locks per store operation, so this deposit never queues behind a
    // parked prompt for longer than one store write.
    let outcome = tokio::task::spawn_blocking(move || -> Result<ApplyResponse> {
        let previous_provider_id = lock_shared_secret_store(&store)
            .managed_state_record(&request.namespace)
            .and_then(|record| record.provider_id.clone());
        let response = {
            let mut guard = lock_shared_secret_store(&store);
            // One transaction: the flat secrets and the managed-state apply commit together, so a
            // validation failure (stale revision, ownership, invalid selection) cannot leave the
            // deposited secrets orphaned. `source_refs` still see the just-deposited secrets.
            crate::extensions::managed_state::deposit_and_apply(
                &home,
                &mut guard,
                &runtime_config,
                &request.namespace,
                request
                    .secrets
                    .iter()
                    .map(|entry| (entry.name.as_str(), entry.value.as_str())),
                request.apply,
            )?
        };
        // A picker call that raced the deposit may have cached a failed model
        // listing; drop it for the outgoing and incoming provider, mirroring
        // the admin-tier apply route.
        if response.outcome != "noop" {
            let new_provider_id = lock_shared_secret_store(&store)
                .managed_state_record(&request.namespace)
                .and_then(|record| record.provider_id.clone());
            for provider_id in [previous_provider_id, new_provider_id]
                .into_iter()
                .flatten()
                .collect::<std::collections::BTreeSet<_>>()
            {
                if let Err(error) =
                    crate::runtime::agent::provider_model_catalog::invalidate_provider_models(
                        &home,
                        &provider_id,
                    )
                {
                    tracing::warn!(
                        error = %error,
                        provider = %provider_id,
                        "provider model catalog invalidation failed after credential deposit"
                    );
                }
            }
        }
        Ok(response)
    })
    .await;
    match outcome {
        Ok(Ok(response)) => {
            // The deposit route's success audit: names and counts only, never values.
            tracing::info!(
                namespace = %namespace,
                applied_revision = response.applied_revision,
                outcome = %response.outcome,
                secrets_written,
                "init credential deposit applied"
            );
            ApiSuccess::new(DepositCredentialResponse {
                secrets_written,
                applied_revision: response.applied_revision,
                outcome: response.outcome,
            })
            .into_response()
        }
        Ok(Err(error)) => error.into_response(),
        Err(error) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            if error.is_panic() {
                "init.credential_deposit_panicked"
            } else {
                "init.credential_deposit_cancelled"
            },
            "credential deposit task failed unexpectedly",
        ),
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct NativeConfigCancelRequest {
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
                "agent.native_config_lock_task_panicked"
            } else {
                "agent.native_config_lock_task_cancelled"
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
    // A connected client is liveness on its own, so the idle reaper must not fire while a backend
    // holds the socket; the guard keeps the count correct on every early return below.
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
    // Subscribe before snapshotting hello: a frame emitted in that window then rides the live
    // stream and dedups by seq, while the reverse order drops it from both with no later snapshot.
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
                        // A terminal session means the server is on its way down; close rather
                        // than let a hung backend pin the process past --max-lifetime.
                        if !session.is_active() {
                            let _ = sender.send(Message::Close(None)).await;
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        // Re-send hello so the client re-folds the full signal-log replay: with
                        // raw signals a skipped frame is otherwise a permanent hole.
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
                        // A lagged receiver can miss the terminal event itself.
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

/// The `init.ws_lagged` frame, extracted so tests can reach it without falling a broadcast buffer
/// behind.
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

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(super) struct ClientFrame {
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
