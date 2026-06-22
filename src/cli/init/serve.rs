use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path as AxumPath, Query, Request, State};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use clap::Args;
use futures::{SinkExt, StreamExt};
use http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Notify, broadcast};
use tower_http::limit::RequestBodyLimitLayer;
use zeroize::Zeroize;

use crate::auth::constant_time_eq;
use crate::config;
use crate::envelope::{ApiError, ApiSuccess};
use crate::error::{Result, StackError};

use super::prompt::{
    self, HostedPromptDriver, HostedPromptOutcome, HostedPromptRequest, HostedPromptStyle,
};
use super::{CloudflareModeArg, CloudflaredDeploymentArg, InitArgs, InitMode, run_hosted_init};

const DEFAULT_INIT_TOKEN_ENV: &str = "ACP_STACK_INIT_TOKEN";
const INIT_BOOTSTRAP_TOKEN_FIELD: &str = "bootstrap token";
const INIT_WS_CHANNEL_CAPACITY: usize = 128;
const INIT_EVENT_HISTORY_LIMIT: usize = 256;

#[derive(Debug, Args)]
pub(super) struct InitServeArgs {
    /// Bootstrap HTTP bind address. Defaults to the normal API bind default.
    #[arg(long, default_value = config::DEFAULT_API_BIND)]
    bind: String,
    /// Environment variable containing the bootstrap bearer token.
    #[arg(long = "token-env", default_value = DEFAULT_INIT_TOKEN_ENV)]
    token_env: String,
    /// File containing the bootstrap bearer token. Overrides --token-env.
    #[arg(long = "token-file", value_name = "PATH")]
    token_file: Option<PathBuf>,
    /// Allowed browser Origin for bootstrap calls. Repeatable.
    #[arg(long = "allowed-origin")]
    allowed_origin: Vec<String>,
    /// Request body size limit for bootstrap HTTP routes.
    #[arg(long, default_value_t = super::STARTER_MAX_REQUEST_BYTES)]
    max_request_bytes: u64,
}

pub(super) fn run_init_serve(args: InitServeArgs) -> Result<()> {
    let token = resolve_bootstrap_token(&args)?;
    let bind = args.bind.clone();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|source| StackError::ServeIo { source })?;

    runtime.block_on(async move {
        let listener = tokio::net::TcpListener::bind(&bind)
            .await
            .map_err(|source| StackError::ServeBind {
                bind: bind.clone(),
                source,
            })?;
        let local = listener
            .local_addr()
            .map(|addr| addr.to_string())
            .unwrap_or(bind);
        let state = BootstrapState {
            token: Arc::new(token),
            allowed_origins: Arc::new(args.allowed_origin),
            manager: HostedInitManager::new(),
        };
        let manager = state.manager.clone();
        let shutdown_manager = state.manager.clone();
        let app = build_bootstrap_router(state.clone(), args.max_request_bytes);
        eprintln!("acps init serve: listening on {local}");
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                shutdown_manager.wait_for_terminal().await;
            })
            .await
            .map_err(|source| StackError::ServeIo { source })?;
        manager.terminal_result()
    })
}

fn resolve_bootstrap_token(args: &InitServeArgs) -> Result<String> {
    let token = if let Some(path) = args.token_file.as_ref() {
        std::fs::read_to_string(path).map_err(|source| StackError::ConfigRead {
            path: path.clone(),
            source,
        })?
    } else {
        std::env::var(&args.token_env).map_err(|_| StackError::MissingField {
            field: INIT_BOOTSTRAP_TOKEN_FIELD,
        })?
    };
    let token = token.trim().to_owned();
    if token.is_empty() {
        return Err(StackError::InvalidParam {
            field: INIT_BOOTSTRAP_TOKEN_FIELD,
            reason: "bootstrap token must not be empty".to_owned(),
        });
    }
    Ok(token)
}

fn build_bootstrap_router(state: BootstrapState, max_request_bytes: u64) -> Router {
    Router::new()
        .route("/v1/init/sessions", post(create_session_handler))
        .route("/v1/init/sessions/{id}", get(session_status_handler))
        .route("/v1/init/sessions/{id}/events", get(session_events_handler))
        .route(
            "/v1/init/sessions/{id}/cancel",
            post(session_cancel_handler),
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
struct BootstrapState {
    token: Arc<String>,
    allowed_origins: Arc<Vec<String>>,
    manager: Arc<HostedInitManager>,
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
    match state
        .manager
        .start_session(body.map(|body| body.0).unwrap_or_default())
    {
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
        Some(session) => ApiSuccess::new(session.status_snapshot()).into_response(),
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
        Some(session) => ApiSuccess::new(InitEventsResponse {
            session_id: id,
            events: session.events_after(query.after_seq.unwrap_or(0)),
        })
        .into_response(),
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
    let (mut sender, mut receiver) = socket.split();
    let hello = session.hello_frame();
    if sender.send(Message::Text(hello.into())).await.is_err() {
        return;
    }
    let mut events = session.subscribe();
    loop {
        tokio::select! {
            inbound = receiver.next() => {
                let Some(Ok(message)) = inbound else {
                    break;
                };
                if let Message::Text(text) = message {
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
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let frame = json!({
                            "type": "error",
                            "code": "init.ws_lagged",
                            "message": "websocket client lagged behind init event stream"
                        }).to_string();
                        let _ = sender.send(Message::Text(frame.into())).await;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
}

enum ClientFrameOutcome {
    None,
    Send(String),
    Close(String),
}

fn handle_client_frame(session: &Arc<HostedInitSession>, text: &str) -> ClientFrameOutcome {
    let frame = match serde_json::from_str::<ClientFrame>(text) {
        Ok(frame) => frame,
        Err(error) => {
            return ClientFrameOutcome::Send(
                json!({
                    "type": "error",
                    "code": "init.bad_frame",
                    "message": format!("invalid client frame: {error}")
                })
                .to_string(),
            );
        }
    };
    match frame.frame_type.as_str() {
        "input" => {
            let Some(request_id) = frame.request_id else {
                return ClientFrameOutcome::Send(
                    json!({
                        "type": "error",
                        "code": "init.missing_request_id",
                        "message": "input frame requires request_id"
                    })
                    .to_string(),
                );
            };
            match session.submit_input(&request_id, frame.value.unwrap_or(Value::Null)) {
                Ok(()) => ClientFrameOutcome::None,
                Err(message) => ClientFrameOutcome::Send(
                    json!({
                        "type": "error",
                        "code": "init.input_rejected",
                        "message": message
                    })
                    .to_string(),
                ),
            }
        }
        "cancel" => {
            session.cancel(frame.reason.as_deref().unwrap_or("backend_cancel"));
            ClientFrameOutcome::None
        }
        "replay_result" => match session.result_frame() {
            Some(frame) => ClientFrameOutcome::Send(frame),
            None => ClientFrameOutcome::Send(
                json!({
                    "type": "error",
                    "code": "init.result_unavailable",
                    "message": "init result is not available"
                })
                .to_string(),
            ),
        },
        "ack_result" => match session.ack_result() {
            Ok(()) => ClientFrameOutcome::Close(
                json!({
                    "type": "ack_accepted",
                    "session_id": session.id
                })
                .to_string(),
            ),
            Err(message) => ClientFrameOutcome::Send(
                json!({
                    "type": "error",
                    "code": "init.ack_rejected",
                    "message": message
                })
                .to_string(),
            ),
        },
        _ => ClientFrameOutcome::Send(
            json!({
                "type": "error",
                "code": "init.unsupported_frame",
                "message": format!("unsupported client frame `{}`", frame.frame_type)
            })
            .to_string(),
        ),
    }
}

#[derive(Debug, Deserialize)]
struct ClientFrame {
    #[serde(rename = "type")]
    frame_type: String,
    request_id: Option<String>,
    value: Option<Value>,
    reason: Option<String>,
}

#[derive(Default, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StartInitRequest {
    agent: Option<String>,
    provider: Option<String>,
    api_key_ref: Option<String>,
    model: Option<String>,
    custom_provider: Option<bool>,
    provider_name: Option<String>,
    base_url: Option<String>,
    provider_api: Option<String>,
    model_name: Option<String>,
    context: Option<String>,
    output_max_tokens: Option<String>,
    workspace_root: Option<String>,
    workspace_uploads: Option<String>,
    runtime_user: Option<String>,
    #[serde(default)]
    code_from: Vec<String>,
    #[serde(default)]
    data_from: Vec<String>,
    skip_testflight: Option<bool>,
    testflight: Option<bool>,
}

impl StartInitRequest {
    fn into_init_args(self) -> InitArgs {
        let mut args = empty_init_args();
        args.agent = self.agent;
        args.provider = self.provider;
        args.api_key_ref = self.api_key_ref;
        args.model = self.model;
        args.custom_provider = self.custom_provider.unwrap_or(false);
        args.provider_name = self.provider_name;
        args.base_url = self.base_url;
        args.provider_api = self.provider_api;
        args.model_name = self.model_name;
        args.context = self.context;
        args.output_max_tokens = self.output_max_tokens;
        args.workspace_root = self.workspace_root;
        args.workspace_uploads = self.workspace_uploads;
        args.runtime_user = self.runtime_user;
        args.code_from = self.code_from;
        args.data_from = self.data_from;
        args.skip_testflight = self.skip_testflight.unwrap_or(false);
        args.testflight = self.testflight.unwrap_or(false);
        args
    }
}

fn empty_init_args() -> InitArgs {
    InitArgs {
        agent: None,
        custom_agent_id: None,
        custom_agent_name: None,
        custom_agent_command: None,
        custom_agent_arg: Vec::new(),
        custom_agent_install: None,
        custom_agent_creates: None,
        agent_env_ref: Vec::new(),
        dep: Vec::new(),
        dep_system: Vec::new(),
        deps_apply: false,
        deps_apply_yes: false,
        stack_update: None,
        stack_update_frequency: None,
        non_interactive: false,
        handoff_json: false,
        from_file: None,
        from_toml: None,
        from_base64: None,
        provider: None,
        api_key_ref: None,
        custom_provider: false,
        provider_name: None,
        base_url: None,
        provider_api: None,
        model: None,
        model_name: None,
        context: None,
        output_max_tokens: None,
        skills_source: None,
        skills: Vec::new(),
        no_skills: true,
        edge: None,
        exposure: None,
        hostname: None,
        cloudflare_mode: CloudflareModeArg::Generated,
        cloudflare_api_token_ref: None,
        cloudflare_account_id_ref: None,
        cloudflared_deployment: CloudflaredDeploymentArg::Host,
        workspace_root: None,
        workspace_uploads: None,
        runtime_user: None,
        code_from: Vec::new(),
        data_from: Vec::new(),
        mcp_preset: Vec::new(),
        mcp_stdio: Vec::new(),
        mcp_stdio_env: Vec::new(),
        mcp_http: Vec::new(),
        mcp_http_header: Vec::new(),
        supabase_url: None,
        supabase_schema: None,
        supabase_api_key_ref: None,
        no_supabase: false,
        #[cfg(feature = "dev-tools")]
        skip_workspace_init: false,
        testflight: false,
        skip_testflight: false,
        standard_agent_work_deps: false,
        browser_use_profile: false,
        prompt_agent_env_refs: false,
        prompt_skills: false,
        prompt_data_sources: Vec::new(),
        prompt_mcp_stdio: Vec::new(),
        prompt_mcp_http: Vec::new(),
        resume: false,
        fresh: false,
        run_id: None,
    }
}

#[derive(Debug, Serialize)]
struct StartInitResponse {
    session_id: String,
    status: String,
}

#[derive(Debug, Serialize)]
struct SimpleSessionResponse {
    session_id: String,
    status: String,
}

#[derive(Debug, Serialize)]
struct InitEventsResponse {
    session_id: String,
    events: Vec<Value>,
}

#[derive(Debug, Serialize)]
struct InitStatusResponse {
    session_id: String,
    status: String,
    last_seq: u64,
    pending_input: Option<PublicInputRequest>,
    recent_events: Vec<Value>,
    result_available: bool,
    error: Option<PublicError>,
}

#[derive(Debug, Clone, Serialize)]
struct PublicError {
    code: String,
    message: String,
}

#[derive(Debug, Clone, Serialize)]
struct PublicInputRequest {
    request_id: String,
    style: String,
    prompt: String,
    required: bool,
    default: Option<bool>,
    options: Vec<PublicInputOption>,
}

#[derive(Debug, Clone, Serialize)]
struct PublicInputOption {
    index: usize,
    label: String,
    hint: String,
}

struct HostedInitManager {
    active: Mutex<Option<Arc<HostedInitSession>>>,
    shutdown: Arc<Notify>,
}

enum StartSessionError {
    Active,
}

impl HostedInitManager {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            active: Mutex::new(None),
            shutdown: Arc::new(Notify::new()),
        })
    }

    fn start_session(
        self: &Arc<Self>,
        request: StartInitRequest,
    ) -> std::result::Result<StartInitResponse, StartSessionError> {
        let mut active = lock_unpoisoned(&self.active);
        if let Some(session) = active.as_ref()
            && session.is_active()
        {
            return Err(StartSessionError::Active);
        }

        let session = HostedInitSession::new(next_bootstrap_session_id(), self.shutdown.clone());
        let response = StartInitResponse {
            session_id: session.id.clone(),
            status: session.status(),
        };
        *active = Some(session.clone());
        let init_args = request.into_init_args();
        let driver: Arc<dyn HostedPromptDriver> = Arc::new(SessionPromptDriver {
            session: session.clone(),
        });
        std::thread::spawn(move || {
            let result = prompt::with_hosted_driver(driver, || {
                run_hosted_init(init_args, InitMode::Operator)
            });
            if let Err(error) = result
                && !session.has_result()
            {
                session.set_error(error.error_code(), error.public_message());
            }
        });
        Ok(response)
    }

    fn session(&self, id: &str) -> Option<Arc<HostedInitSession>> {
        lock_unpoisoned(&self.active)
            .as_ref()
            .filter(|session| session.id == id)
            .cloned()
    }

    async fn wait_for_terminal(&self) {
        self.shutdown.notified().await;
    }

    fn terminal_result(&self) -> Result<()> {
        let Some(session) = self.session_current() else {
            return Ok(());
        };
        match session.status().as_str() {
            "canceled" => Err(StackError::InvalidParam {
                field: "init",
                reason: "hosted init session was cancelled".to_owned(),
            }),
            "errored" => {
                let snapshot = session.status_snapshot();
                let reason = snapshot
                    .error
                    .map(|error| format!("{}: {}", error.code, error.message))
                    .unwrap_or_else(|| "hosted init session failed".to_owned());
                Err(StackError::InvalidParam {
                    field: "init",
                    reason,
                })
            }
            _ => Ok(()),
        }
    }

    fn session_current(&self) -> Option<Arc<HostedInitSession>> {
        lock_unpoisoned(&self.active).as_ref().cloned()
    }
}

struct HostedInitSession {
    id: String,
    inner: Mutex<SessionInner>,
    input_ready: Condvar,
    events: broadcast::Sender<String>,
    shutdown: Arc<Notify>,
}

struct SessionInner {
    status: String,
    next_seq: u64,
    history: Vec<Value>,
    pending_input: Option<PublicInputRequest>,
    pending_response: Option<(String, Value)>,
    result_json: Option<String>,
    error: Option<PublicError>,
}

impl HostedInitSession {
    fn new(id: String, shutdown: Arc<Notify>) -> Arc<Self> {
        let (events, _) = broadcast::channel(INIT_WS_CHANNEL_CAPACITY);
        let session = Arc::new(Self {
            id,
            inner: Mutex::new(SessionInner {
                status: "running".to_owned(),
                next_seq: 0,
                history: Vec::new(),
                pending_input: None,
                pending_response: None,
                result_json: None,
                error: None,
            }),
            input_ready: Condvar::new(),
            events,
            shutdown,
        });
        session.push_event("progress", json!({"message": "init session started"}));
        session
    }

    fn subscribe(&self) -> broadcast::Receiver<String> {
        self.events.subscribe()
    }

    fn status(&self) -> String {
        lock_unpoisoned(&self.inner).status.clone()
    }

    fn is_active(&self) -> bool {
        matches!(
            self.status().as_str(),
            "running" | "waiting_for_input" | "completed_awaiting_ack"
        )
    }

    fn status_snapshot(&self) -> InitStatusResponse {
        let inner = lock_unpoisoned(&self.inner);
        InitStatusResponse {
            session_id: self.id.clone(),
            status: inner.status.clone(),
            last_seq: inner.next_seq,
            pending_input: inner.pending_input.clone(),
            recent_events: inner.history.iter().rev().take(50).cloned().collect(),
            result_available: inner.result_json.is_some(),
            error: inner.error.clone(),
        }
    }

    fn hello_frame(&self) -> String {
        let snapshot = self.status_snapshot();
        json!({
            "type": "hello",
            "session_id": self.id,
            "status": snapshot.status,
            "last_seq": snapshot.last_seq,
            "pending_input": snapshot.pending_input,
            "result_available": snapshot.result_available
        })
        .to_string()
    }

    fn events_after(&self, after_seq: u64) -> Vec<Value> {
        lock_unpoisoned(&self.inner)
            .history
            .iter()
            .filter(|event| event.get("seq").and_then(Value::as_u64).unwrap_or(0) > after_seq)
            .cloned()
            .collect()
    }

    fn push_event(&self, event_type: &str, mut payload: Value) {
        let frame = {
            let mut inner = lock_unpoisoned(&self.inner);
            self.record_event_locked(&mut inner, event_type, &mut payload)
        };
        let _ = self.events.send(frame.to_string());
    }

    fn record_event_locked(
        &self,
        inner: &mut SessionInner,
        event_type: &str,
        payload: &mut Value,
    ) -> Value {
        inner.next_seq = inner.next_seq.saturating_add(1);
        let seq = inner.next_seq;
        let mut object = BTreeMap::new();
        object.insert("type".to_owned(), Value::String(event_type.to_owned()));
        object.insert("seq".to_owned(), Value::Number(seq.into()));
        object.insert("session_id".to_owned(), Value::String(self.id.clone()));
        if let Some(map) = payload.as_object_mut() {
            for (key, value) in std::mem::take(map) {
                object.insert(key, value);
            }
        } else {
            object.insert("data".to_owned(), payload.clone());
        }
        let frame = Value::Object(object.into_iter().collect());
        inner.history.push(frame.clone());
        if inner.history.len() > INIT_EVENT_HISTORY_LIMIT {
            inner.history.remove(0);
        }
        frame
    }

    fn request_input(&self, request: HostedPromptRequest) -> Result<Option<Value>> {
        if !should_handle_hosted_prompt(&request) {
            return Ok(None);
        }
        let public = public_input_request(request);
        let frame = {
            let mut inner = lock_unpoisoned(&self.inner);
            if matches!(inner.status.as_str(), "canceled" | "closed") {
                return Err(StackError::InvalidParam {
                    field: "init",
                    reason: "hosted init session was cancelled".to_owned(),
                });
            }
            inner.status = "waiting_for_input".to_owned();
            inner.pending_response = None;
            inner.pending_input = Some(public.clone());
            let mut payload = json!({ "input": public });
            self.record_event_locked(&mut inner, "input_required", &mut payload)
        };
        let _ = self.events.send(frame.to_string());

        let mut inner = lock_unpoisoned(&self.inner);
        loop {
            if matches!(inner.status.as_str(), "canceled" | "closed") {
                return Err(StackError::InvalidParam {
                    field: "init",
                    reason: "hosted init session was cancelled".to_owned(),
                });
            }
            if let Some((request_id, value)) = inner.pending_response.take()
                && request_id == public.request_id
            {
                inner.status = "running".to_owned();
                inner.pending_input = None;
                return Ok(Some(value));
            }
            inner = self
                .input_ready
                .wait(inner)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    fn submit_input(&self, request_id: &str, value: Value) -> std::result::Result<(), String> {
        let frame = {
            let mut inner = lock_unpoisoned(&self.inner);
            let Some(pending) = inner.pending_input.as_ref() else {
                return Err("no input request is pending".to_owned());
            };
            if pending.request_id != request_id {
                return Err(format!(
                    "stale request_id `{request_id}`; current request_id is `{}`",
                    pending.request_id
                ));
            }
            inner.pending_response = Some((request_id.to_owned(), value));
            let mut payload = json!({ "request_id": request_id });
            self.record_event_locked(&mut inner, "input_accepted", &mut payload)
        };
        let _ = self.events.send(frame.to_string());
        self.input_ready.notify_all();
        Ok(())
    }

    fn set_result(&self, payload: Value) {
        let mut result_json = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_owned());
        {
            let mut inner = lock_unpoisoned(&self.inner);
            if matches!(inner.status.as_str(), "canceled" | "closed") {
                result_json.zeroize();
                return;
            }
            inner.status = "completed_awaiting_ack".to_owned();
            inner.result_json = Some(result_json);
            inner.pending_input = None;
            let mut payload = json!({ "status": "completed_awaiting_ack" });
            let frame = self.record_event_locked(&mut inner, "result_ready", &mut payload);
            let _ = self.events.send(frame.to_string());
        }
        if let Some(frame) = self.result_frame() {
            let _ = self.events.send(frame);
        }
        self.input_ready.notify_all();
    }

    fn result_frame(&self) -> Option<String> {
        let inner = lock_unpoisoned(&self.inner);
        let result = inner.result_json.as_ref()?;
        Some(format!(
            r#"{{"type":"result","session_id":"{}","payload":{}}}"#,
            self.id, result
        ))
    }

    fn has_result(&self) -> bool {
        lock_unpoisoned(&self.inner).result_json.is_some()
    }

    fn ack_result(&self) -> std::result::Result<(), String> {
        let frame = {
            let mut inner = lock_unpoisoned(&self.inner);
            let Some(mut result) = inner.result_json.take() else {
                return Err("no init result is awaiting acknowledgement".to_owned());
            };
            result.zeroize();
            inner.status = "closed".to_owned();
            inner.pending_input = None;
            let mut payload = json!({ "status": "closed" });
            self.record_event_locked(&mut inner, "result_acked", &mut payload)
        };
        let _ = self.events.send(frame.to_string());
        self.input_ready.notify_all();
        self.shutdown.notify_one();
        Ok(())
    }

    fn cancel(&self, reason: &str) {
        let Some(frame) = ({
            let mut inner = lock_unpoisoned(&self.inner);
            if matches!(
                inner.status.as_str(),
                "completed_awaiting_ack" | "closed" | "canceled"
            ) {
                return;
            }
            inner.status = "canceled".to_owned();
            inner.pending_input = None;
            inner.pending_response = None;
            let mut payload = json!({ "reason": reason });
            Some(self.record_event_locked(&mut inner, "canceled", &mut payload))
        }) else {
            return;
        };
        let _ = self.events.send(frame.to_string());
        self.input_ready.notify_all();
        self.shutdown.notify_one();
    }

    fn set_error(&self, code: &str, message: String) {
        let Some(frame) = ({
            let mut inner = lock_unpoisoned(&self.inner);
            if matches!(
                inner.status.as_str(),
                "canceled" | "closed" | "completed_awaiting_ack"
            ) {
                return;
            }
            inner.status = "errored".to_owned();
            inner.pending_input = None;
            inner.error = Some(PublicError {
                code: code.to_owned(),
                message: message.clone(),
            });
            let mut payload = json!({ "code": code, "message": message });
            Some(self.record_event_locked(&mut inner, "error", &mut payload))
        }) else {
            return;
        };
        let _ = self.events.send(frame.to_string());
        self.input_ready.notify_all();
        self.shutdown.notify_one();
    }
}

struct SessionPromptDriver {
    session: Arc<HostedInitSession>,
}

impl HostedPromptDriver for SessionPromptDriver {
    fn select(&self, request: HostedPromptRequest) -> Result<HostedPromptOutcome<Option<usize>>> {
        let Some(value) = self.session.request_input(request.clone())? else {
            return Ok(HostedPromptOutcome::Unhandled);
        };
        let selection = parse_optional_index(&value, &request)?;
        Ok(HostedPromptOutcome::Handled(selection))
    }

    fn confirm(&self, request: HostedPromptRequest) -> Result<HostedPromptOutcome<bool>> {
        let Some(value) = self.session.request_input(request.clone())? else {
            return Ok(HostedPromptOutcome::Unhandled);
        };
        let Some(value) = value.as_bool() else {
            return Err(StackError::InvalidParam {
                field: "init",
                reason: "confirm input must be a boolean".to_owned(),
            });
        };
        Ok(HostedPromptOutcome::Handled(value))
    }

    fn text(&self, request: HostedPromptRequest) -> Result<HostedPromptOutcome<Option<String>>> {
        let Some(value) = self.session.request_input(request)? else {
            return Ok(HostedPromptOutcome::Unhandled);
        };
        if value.is_null() {
            return Ok(HostedPromptOutcome::Handled(None));
        }
        let Some(value) = value.as_str() else {
            return Err(StackError::InvalidParam {
                field: "init",
                reason: "text input must be a string".to_owned(),
            });
        };
        Ok(HostedPromptOutcome::Handled(Some(value.to_owned())))
    }

    fn password(
        &self,
        request: HostedPromptRequest,
    ) -> Result<HostedPromptOutcome<Option<String>>> {
        self.text(request)
    }

    fn progress(&self, message: String) {
        self.session
            .push_event("progress", json!({ "message": message }));
    }

    fn result(&self, payload: Value) {
        self.session.set_result(payload);
    }
}

fn should_handle_hosted_prompt(request: &HostedPromptRequest) -> bool {
    match request.style {
        HostedPromptStyle::Select | HostedPromptStyle::SearchableSelect => {
            request.prompt == "Agent"
                || request.prompt.starts_with("provider for ")
                || request.prompt.starts_with("select ")
        }
        HostedPromptStyle::Confirm => {
            request.prompt.contains("configure it as a custom provider")
                || request.prompt == "run testflight now?"
        }
        HostedPromptStyle::Text => matches!(
            request.prompt.as_str(),
            "provider id" | "provider-name" | "base-url" | "api-key-ref" | "model"
        ),
        HostedPromptStyle::Password => true,
    }
}

fn public_input_request(request: HostedPromptRequest) -> PublicInputRequest {
    PublicInputRequest {
        request_id: next_input_request_id(),
        style: prompt_style_label(request.style).to_owned(),
        prompt: request.prompt,
        required: request.required,
        default: request.default,
        options: request
            .items
            .into_iter()
            .enumerate()
            .map(|(index, item)| PublicInputOption {
                index,
                label: item.label,
                hint: item.hint,
            })
            .collect(),
    }
}

fn prompt_style_label(style: HostedPromptStyle) -> &'static str {
    match style {
        HostedPromptStyle::Select => "select",
        HostedPromptStyle::SearchableSelect => "searchable_select",
        HostedPromptStyle::Confirm => "confirm",
        HostedPromptStyle::Text => "text",
        HostedPromptStyle::Password => "password",
    }
}

fn parse_optional_index(value: &Value, request: &HostedPromptRequest) -> Result<Option<usize>> {
    if value.is_null() {
        return Ok(None);
    }
    if let Some(index) = value.as_u64() {
        return validate_index(index as usize, request);
    }
    if let Some(index) = value.get("index").and_then(Value::as_u64) {
        return validate_index(index as usize, request);
    }
    if let Some(label) = value.as_str() {
        let index = request
            .items
            .iter()
            .position(|item| item.label == label)
            .ok_or_else(|| StackError::InvalidParam {
                field: "init",
                reason: format!("selection `{label}` does not match any option"),
            })?;
        return Ok(Some(index));
    }
    Err(StackError::InvalidParam {
        field: "init",
        reason: "select input must be an index, label, or null".to_owned(),
    })
}

fn validate_index(index: usize, request: &HostedPromptRequest) -> Result<Option<usize>> {
    if index >= request.items.len() {
        return Err(StackError::InvalidParam {
            field: "init",
            reason: format!("selection index {index} is out of range"),
        });
    }
    Ok(Some(index))
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

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn next_bootstrap_session_id() -> String {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0).max(0) as u128;
    let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let pid = std::process::id();
    format!("init_{nanos:020}_{sequence:010}_{pid:010}")
}

fn next_input_request_id() -> String {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0).max(0) as u128;
    let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let pid = std::process::id();
    format!("ireq_{nanos:020}_{sequence:010}_{pid:010}")
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::body::to_bytes;
    use http::{Method, Request};
    use std::time::Duration;
    use tower::ServiceExt;

    const TEST_TOKEN: &str = "test_bootstrap_token";

    fn test_session(id: &str) -> Arc<HostedInitSession> {
        HostedInitSession::new(id.to_owned(), Arc::new(Notify::new()))
    }

    fn wait_for_pending_input(session: &HostedInitSession) -> PublicInputRequest {
        for _ in 0..100 {
            if let Some(input) = lock_unpoisoned(&session.inner).pending_input.clone() {
                return input;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for hosted init input request");
    }

    fn hosted_items(labels: &[&str]) -> Vec<(usize, String, String)> {
        labels
            .iter()
            .enumerate()
            .map(|(index, label)| (index, (*label).to_owned(), String::new()))
            .collect()
    }

    fn hosted_test_request(
        style: HostedPromptStyle,
        prompt: &str,
        labels: &[&str],
    ) -> HostedPromptRequest {
        HostedPromptRequest {
            style,
            prompt: prompt.to_owned(),
            required: false,
            default: None,
            items: hosted_items(labels)
                .into_iter()
                .map(|(_, label, hint)| prompt::HostedPromptItem { label, hint })
                .collect(),
        }
    }

    fn send_select_response(
        prompt: &str,
        labels: &[&str],
        response: Value,
    ) -> HostedPromptOutcome<Option<usize>> {
        let session = test_session("init_driver_select");
        let driver = SessionPromptDriver {
            session: session.clone(),
        };
        let request = hosted_test_request(HostedPromptStyle::SearchableSelect, prompt, labels);
        let handle = std::thread::spawn(move || driver.select(request));
        let pending = wait_for_pending_input(&session);
        session
            .submit_input(&pending.request_id, response)
            .expect("submit input");
        handle
            .join()
            .expect("driver thread")
            .expect("driver result")
    }

    #[test]
    fn hosted_driver_accepts_provider_password_and_model_responses() {
        let provider = send_select_response(
            "provider for opencode",
            &["OpenRouter (openrouter)", "DeepSeek (deepseek)"],
            json!("OpenRouter (openrouter)"),
        );
        assert_eq!(provider, HostedPromptOutcome::Handled(Some(0)));

        let model = send_select_response(
            "select model",
            &["deepseek-v4-flash", "openai/gpt-5-mini"],
            json!({ "index": 1 }),
        );
        assert_eq!(model, HostedPromptOutcome::Handled(Some(1)));

        let session = test_session("init_driver_password");
        let driver = SessionPromptDriver {
            session: session.clone(),
        };
        let request = HostedPromptRequest {
            style: HostedPromptStyle::Password,
            prompt: "OPENROUTER_API_KEY".to_owned(),
            required: true,
            default: None,
            items: Vec::new(),
        };
        let handle = std::thread::spawn(move || driver.password(request));
        let pending = wait_for_pending_input(&session);
        session
            .submit_input(&pending.request_id, json!("sk-hosted-secret"))
            .expect("submit password");
        let password = handle.join().expect("driver thread").expect("password");
        assert_eq!(
            password,
            HostedPromptOutcome::Handled(Some("sk-hosted-secret".to_owned()))
        );
        let events = serde_json::to_string(&session.events_after(0)).expect("events");
        assert!(!events.contains("sk-hosted-secret"));
    }

    #[test]
    fn hosted_driver_streams_testflight_confirmation() {
        let session = test_session("init_driver_testflight");
        let driver = SessionPromptDriver {
            session: session.clone(),
        };
        let request = HostedPromptRequest {
            style: HostedPromptStyle::Confirm,
            prompt: "run testflight now?".to_owned(),
            required: true,
            default: Some(false),
            items: Vec::new(),
        };
        let handle = std::thread::spawn(move || driver.confirm(request));
        let pending = wait_for_pending_input(&session);
        assert_eq!(pending.prompt, "run testflight now?");
        session
            .submit_input(&pending.request_id, json!(true))
            .expect("submit confirm");
        let confirm = handle.join().expect("driver thread").expect("confirm");
        assert_eq!(confirm, HostedPromptOutcome::Handled(true));
    }

    #[test]
    fn hosted_driver_leaves_non_bootstrap_text_prompts_unhandled() {
        let session = test_session("init_driver_text");
        let driver = SessionPromptDriver { session };
        let request = HostedPromptRequest {
            style: HostedPromptStyle::Text,
            prompt: "acps-config.toml path".to_owned(),
            required: true,
            default: None,
            items: Vec::new(),
        };
        let outcome = driver.text(request).expect("text");
        assert_eq!(outcome, HostedPromptOutcome::Unhandled);
    }

    #[test]
    fn stale_input_request_id_is_rejected() {
        let session = test_session("init_stale_input");
        let driver = SessionPromptDriver {
            session: session.clone(),
        };
        let request = HostedPromptRequest {
            style: HostedPromptStyle::Password,
            prompt: "OPENROUTER_API_KEY".to_owned(),
            required: true,
            default: None,
            items: Vec::new(),
        };
        let handle = std::thread::spawn(move || driver.password(request));
        let pending = wait_for_pending_input(&session);

        let stale_frame = json!({
            "type": "input",
            "request_id": "stale_request",
            "value": "sk-hosted-secret"
        })
        .to_string();
        match handle_client_frame(&session, &stale_frame) {
            ClientFrameOutcome::Send(frame) => {
                let value: Value = serde_json::from_str(&frame).expect("error frame");
                assert_eq!(value["type"], "error");
                assert_eq!(value["code"], "init.input_rejected");
            }
            _ => panic!("stale input should be rejected with an error frame"),
        }

        session
            .submit_input(&pending.request_id, json!("sk-hosted-secret"))
            .expect("submit correct input");
        let password = handle.join().expect("driver thread").expect("password");
        assert_eq!(
            password,
            HostedPromptOutcome::Handled(Some("sk-hosted-secret".to_owned()))
        );
    }

    #[test]
    fn result_is_replay_only_and_ack_is_terminal() {
        let session = test_session("init_result");
        session.set_result(json!({
            "status": "initialized",
            "session_key": "acps_session_secret",
            "admin_key": "acps_admin_secret"
        }));

        let snapshot = serde_json::to_string(&session.status_snapshot()).expect("snapshot");
        assert!(snapshot.contains("completed_awaiting_ack"));
        assert!(!snapshot.contains("acps_session_secret"));
        assert!(!snapshot.contains("acps_admin_secret"));

        let replay = match handle_client_frame(&session, r#"{"type":"replay_result"}"#) {
            ClientFrameOutcome::Send(frame) => frame,
            _ => panic!("replay_result should return a result frame"),
        };
        assert!(replay.contains("acps_session_secret"));
        assert!(replay.contains("acps_admin_secret"));

        match handle_client_frame(&session, r#"{"type":"ack_result"}"#) {
            ClientFrameOutcome::Close(frame) => {
                let value: Value = serde_json::from_str(&frame).expect("ack frame");
                assert_eq!(value["type"], "ack_accepted");
            }
            _ => panic!("ack_result should close the session"),
        }

        assert_eq!(session.status(), "closed");
        assert!(session.result_frame().is_none());
        assert!(!session.is_active());
    }

    #[test]
    fn cancel_prevents_late_result_publication() {
        let session = test_session("init_cancel");
        session.cancel("backend_cancel");
        session.set_result(json!({
            "status": "initialized",
            "session_key": "acps_session_after_cancel",
            "admin_key": "acps_admin_after_cancel"
        }));
        session.set_error("init.failed", "should not replace cancel".to_owned());

        assert_eq!(session.status(), "canceled");
        assert!(session.result_frame().is_none());
        let snapshot = serde_json::to_string(&session.status_snapshot()).expect("snapshot");
        assert!(!snapshot.contains("acps_session_after_cancel"));
        assert!(!snapshot.contains("should not replace cancel"));
    }

    #[tokio::test]
    async fn error_notifies_terminal_waiter_and_returns_failure() {
        let manager = HostedInitManager::new();
        let session = HostedInitSession::new("init_error".to_owned(), manager.shutdown.clone());
        *lock_unpoisoned(&manager.active) = Some(session.clone());

        {
            let waiter = manager.wait_for_terminal();
            tokio::pin!(waiter);
            session.set_error("init.failed", "provider setup failed".to_owned());

            tokio::time::timeout(Duration::from_secs(1), &mut waiter)
                .await
                .expect("terminal waiter should be notified");
        }
        let error = manager
            .terminal_result()
            .expect_err("errored session should return failure");
        assert!(
            error
                .public_message()
                .contains("init.failed: provider setup failed")
        );
    }

    fn app_with_session(session: Arc<HostedInitSession>) -> Router {
        let manager = HostedInitManager::new();
        *lock_unpoisoned(&manager.active) = Some(session);
        build_bootstrap_router(
            BootstrapState {
                token: Arc::new(TEST_TOKEN.to_owned()),
                allowed_origins: Arc::new(vec!["https://backend.example".to_owned()]),
                manager,
            },
            super::super::STARTER_MAX_REQUEST_BYTES,
        )
    }

    async fn request_json(
        app: Router,
        method: Method,
        uri: &str,
        body: Option<Value>,
        token: Option<&str>,
    ) -> (StatusCode, Value) {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(token) = token {
            builder = builder.header(http::header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let body = match body {
            Some(value) => Body::from(value.to_string()),
            None => Body::empty(),
        };
        let response = app
            .oneshot(builder.body(body).expect("request"))
            .await
            .expect("response");
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body bytes");
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).expect("json body")
        };
        (status, value)
    }

    async fn request_raw_json(
        app: Router,
        method: Method,
        uri: &str,
        body: &'static str,
        token: Option<&str>,
    ) -> (StatusCode, Value) {
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header(http::header::CONTENT_TYPE, "application/json");
        if let Some(token) = token {
            builder = builder.header(http::header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let response = app
            .oneshot(builder.body(Body::from(body)).expect("request"))
            .await
            .expect("response");
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body bytes");
        let value = serde_json::from_slice(&bytes).expect("json body");
        (status, value)
    }

    #[tokio::test]
    async fn bootstrap_api_auth_conflict_status_and_event_replay_are_non_secret() {
        let session = test_session("init_api");
        session.push_event("progress", json!({ "message": "first" }));
        session.push_event("progress", json!({ "message": "second" }));
        session.set_result(json!({
            "status": "initialized",
            "session_key": "acps_session_api_secret",
            "admin_key": "acps_admin_api_secret"
        }));
        let app = app_with_session(session);

        let (status, _) = request_json(
            app.clone(),
            Method::GET,
            "/v1/init/sessions/init_api",
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let (status, body) = request_json(
            app.clone(),
            Method::GET,
            "/v1/init/sessions/init_api",
            None,
            Some(TEST_TOKEN),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["data"]["status"], "completed_awaiting_ack");
        assert_eq!(body["data"]["result_available"], true);
        let status_body = body.to_string();
        assert!(!status_body.contains("acps_session_api_secret"));
        assert!(!status_body.contains("acps_admin_api_secret"));

        let (status, _) = request_json(
            app.clone(),
            Method::POST,
            "/v1/init/sessions",
            Some(json!({})),
            Some(TEST_TOKEN),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);

        let (status, body) = request_json(
            app,
            Method::GET,
            "/v1/init/sessions/init_api/events?after_seq=1",
            None,
            Some(TEST_TOKEN),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let events_body = body.to_string();
        assert!(events_body.contains("second"));
        assert!(events_body.contains("result_ready"));
        assert!(!events_body.contains("acps_session_api_secret"));
        assert!(!events_body.contains("acps_admin_api_secret"));
    }

    #[tokio::test]
    async fn bootstrap_api_rejects_duplicate_authorization_headers() {
        let app = app_with_session(test_session("init_duplicate_auth"));
        let request = Request::builder()
            .method(Method::GET)
            .uri("/v1/init/sessions/init_duplicate_auth")
            .header(http::header::AUTHORIZATION, format!("Bearer {TEST_TOKEN}"))
            .header(http::header::AUTHORIZATION, "Bearer other")
            .body(Body::empty())
            .expect("request");
        let response = app.oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body bytes");
        let body: Value = serde_json::from_slice(&bytes).expect("json body");
        assert_eq!(body["ok"], false);
        assert_eq!(body["error"]["code"], "auth.malformed_header");
    }

    #[tokio::test]
    async fn bootstrap_api_malformed_json_uses_error_envelope() {
        let app = app_with_session(test_session("init_malformed"));
        let (status, body) = request_raw_json(
            app,
            Method::POST,
            "/v1/init/sessions",
            "{not-json",
            Some(TEST_TOKEN),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["ok"], false);
        assert!(body["error"]["code"].is_string());
    }
}
