use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    AgentCapabilities, CancelNotification, CloseSessionRequest, CloseSessionResponse, ContentBlock,
    ContentChunk, ForkSessionResponse, Implementation, InitializeRequest, InitializeResponse,
    ListSessionsRequest, ListSessionsResponse, LoadSessionRequest, LoadSessionResponse,
    NewSessionRequest, NewSessionResponse, PromptCapabilities, PromptRequest, PromptResponse,
    ResumeSessionRequest, ResumeSessionResponse, SessionCapabilities, SessionCloseCapabilities,
    SessionConfigOption, SessionConfigOptionCategory, SessionConfigOptionValue,
    SessionConfigSelectOption, SessionForkCapabilities, SessionId, SessionInfo,
    SessionListCapabilities, SessionNotification, SessionResumeCapabilities, SessionUpdate,
    SetSessionConfigOptionRequest, SetSessionConfigOptionResponse, StopReason, TextContent,
};
use agent_client_protocol::{
    Agent, Client, ConnectionTo, Dispatch, Error, JsonRpcMessage, JsonRpcRequest, Responder,
    UntypedMessage,
};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

const FIXTURE_SESSION_PREFIX: &str = "sess_fake_";
const LISTED_SESSION_ID: &str = "sess_listed_0";
const LISTED_PAGE_1_SESSION_ID: &str = "sess_listed_page_1";
const LISTED_PAGE_2_SESSION_ID: &str = "sess_listed_page_2";
const LIST_PAGE_2_CURSOR: &str = "page-2";
const REPEATED_CURSOR: &str = "repeat";
const FIXTURE_ORIGIN: &str = "placebo-agent";
const TESTFLIGHT_MARKER: &str = ".acp-stack-testflight.txt";
const TESTFLIGHT_CONTENT: &[u8] = b"acp-stack testflight ok\n";
const FIRST_CHUNK: &str = "chunk-1";
const SECOND_CHUNK: &str = "chunk-2";
const DEFAULT_CWD: &str = "/tmp";
const LISTED_UPDATED_AT: &str = "2026-05-25T00:00:00Z";
const LISTED_PAGE_2_UPDATED_AT: &str = "2026-05-25T00:00:01Z";
const CREATED_UPDATED_AT: &str = "2026-05-25T00:00:02Z";
const STALL_SLEEP: Duration = Duration::from_secs(3600);

#[derive(Debug, Parser)]
#[command(
    name = "placebo-agent",
    version,
    about = "Deterministic ACP test fixture agent.",
    color = clap::ColorChoice::Never
)]
struct Cli {
    #[arg(long, global = true)]
    print_logs: bool,
    #[arg(long, global = true, value_enum)]
    log_level: Option<LogLevel>,
    #[arg(long, global = true)]
    pure: bool,
    #[arg(long, global = true, default_value_t = 0)]
    port: u16,
    #[arg(long, global = true, default_value = "127.0.0.1")]
    hostname: String,
    #[arg(long, global = true, default_value_t = false)]
    mdns: bool,
    #[arg(long, global = true, default_value = "opencode.local")]
    mdns_domain: String,
    #[arg(long, global = true)]
    cors: Vec<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Start ACP (Agent Client Protocol) server.
    Acp(AcpArgs),
}

#[derive(Debug, Args, Clone)]
struct AcpArgs {
    /// Working directory.
    #[arg(long, default_value_os_t = std::env::current_dir().unwrap_or_else(|_| PathBuf::from(DEFAULT_CWD)))]
    cwd: PathBuf,
    #[arg(long, default_value = DEFAULT_CWD)]
    listed_cwd: PathBuf,
    #[arg(long)]
    assert_env_absent: Vec<String>,
    #[arg(long)]
    assert_env_present: Vec<String>,
    #[arg(long, num_args = 2)]
    assert_env_not_equals: Vec<String>,
    #[arg(long)]
    no_cap_load_session: bool,
    #[arg(long)]
    no_cap_list_session: bool,
    #[arg(long)]
    no_cap_resume_session: bool,
    #[arg(long)]
    no_cap_close_session: bool,
    #[arg(long)]
    no_cap_fork_session: bool,
    #[arg(long)]
    no_cap_fork_message_id: bool,
    #[arg(long)]
    expect_fork_message_id: Option<String>,
    #[arg(long)]
    prompt_silent: bool,
    #[arg(long)]
    initialize_error: bool,
    #[arg(long)]
    session_new_error: bool,
    #[arg(long)]
    session_new_stall: bool,
    #[arg(long)]
    prompt_error: bool,
    #[arg(long)]
    prompt_inference_error: Option<String>,
    #[arg(long)]
    prompt_inference_error_after_update: Option<String>,
    #[arg(long)]
    prompt_response_delay_ms: Option<u64>,
    #[arg(long)]
    prompt_stall_after_update: bool,
    #[arg(long)]
    session_list_paginated: bool,
    #[arg(long)]
    session_list_repeated_cursor: bool,
    #[arg(long)]
    model_config_option: Option<String>,
    #[arg(long, default_value = "model")]
    model_config_option_id: String,
    #[arg(long)]
    expect_model_config: Option<String>,
    #[arg(long)]
    write_pid: Option<PathBuf>,
}

#[derive(Debug, Clone)]
struct CreatedSession {
    id: String,
    cwd: PathBuf,
}

#[derive(Debug)]
struct PlaceboState {
    args: AcpArgs,
    title: String,
    next_session: u64,
    created_sessions: Vec<CreatedSession>,
    cancelled_sessions: HashSet<String>,
    model_configured: bool,
}

impl PlaceboState {
    fn new(args: AcpArgs) -> Self {
        let title = env_assertion_title(&args);
        Self {
            args,
            title,
            next_session: 0,
            created_sessions: Vec::new(),
            cancelled_sessions: HashSet::new(),
            model_configured: false,
        }
    }

    fn model_config_options(&self) -> Option<Vec<SessionConfigOption>> {
        let model = self.args.model_config_option.as_ref()?;
        Some(vec![
            SessionConfigOption::select(
                self.args.model_config_option_id.clone(),
                "Model",
                model.clone(),
                vec![SessionConfigSelectOption::new(model.clone(), model.clone())],
            )
            .category(SessionConfigOptionCategory::Model),
        ])
    }
}

type SharedState = Arc<Mutex<PlaceboState>>;

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Acp(args) => run_acp(args).await,
    };
    if let Err(error) = result {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

async fn run_acp(args: AcpArgs) -> agent_client_protocol::Result<()> {
    if let Some(path) = &args.write_pid {
        tokio::fs::write(path, std::process::id().to_string())
            .await
            .map_err(Error::into_internal_error)?;
    }

    let state = Arc::new(Mutex::new(PlaceboState::new(args)));
    Agent
        .builder()
        .name("placebo-agent")
        .on_receive_request(
            {
                let state = Arc::clone(&state);
                async move |request, responder, connection| {
                    handle_initialize(Arc::clone(&state), request, responder, connection).await
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = Arc::clone(&state);
                async move |request, responder, connection| {
                    handle_new_session(Arc::clone(&state), request, responder, connection).await
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = Arc::clone(&state);
                async move |request, responder, connection| {
                    handle_list_sessions(Arc::clone(&state), request, responder, connection).await
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = Arc::clone(&state);
                async move |request, responder, connection| {
                    handle_set_config_option(Arc::clone(&state), request, responder, connection)
                        .await
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = Arc::clone(&state);
                async move |request, responder, connection| {
                    handle_load_session(Arc::clone(&state), request, responder, connection).await
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = Arc::clone(&state);
                async move |request, responder, connection| {
                    handle_resume_session(Arc::clone(&state), request, responder, connection).await
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = Arc::clone(&state);
                async move |request, responder, connection| {
                    handle_close_session(Arc::clone(&state), request, responder, connection).await
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = Arc::clone(&state);
                async move |request, responder, connection| {
                    handle_fork_session(Arc::clone(&state), request, responder, connection).await
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = Arc::clone(&state);
                async move |request, responder, connection| {
                    handle_prompt(Arc::clone(&state), request, responder, connection).await
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            {
                let state = Arc::clone(&state);
                async move |notification, connection| {
                    handle_cancel(Arc::clone(&state), notification, connection).await
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_dispatch(
            async move |message: Dispatch, connection: ConnectionTo<Client>| {
                message.respond_with_error(Error::method_not_found(), connection)
            },
            agent_client_protocol::on_receive_dispatch!(),
        )
        .connect_to(agent_client_protocol::ByteStreams::new(
            tokio::io::stdout().compat_write(),
            tokio::io::stdin().compat(),
        ))
        .await
}

async fn handle_initialize(
    state: SharedState,
    request: InitializeRequest,
    responder: Responder<InitializeResponse>,
    _connection: ConnectionTo<Client>,
) -> agent_client_protocol::Result<()> {
    let state = state.lock().await;
    if state.args.initialize_error {
        return responder.respond_with_error(Error::new(-32000, "fake initialize failure"));
    }
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
    let capabilities = AgentCapabilities::new()
        .load_session(!state.args.no_cap_load_session)
        .prompt_capabilities(PromptCapabilities::new())
        .session_capabilities(session_capabilities);
    responder.respond(
        InitializeResponse::new(match request.protocol_version {
            ProtocolVersion::V1 => ProtocolVersion::V1,
            _ => ProtocolVersion::V1,
        })
        .agent_capabilities(capabilities)
        .agent_info(
            Implementation::new("placebo-agent", env!("CARGO_PKG_VERSION"))
                .title(state.title.clone()),
        ),
    )
}

async fn handle_new_session(
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
    let response = NewSessionResponse::new(session_id).config_options(state.model_config_options());
    responder.respond(response)
}

async fn handle_list_sessions(
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

async fn handle_set_config_option(
    state: SharedState,
    request: SetSessionConfigOptionRequest,
    responder: Responder<SetSessionConfigOptionResponse>,
    _connection: ConnectionTo<Client>,
) -> agent_client_protocol::Result<()> {
    let mut state = state.lock().await;
    let SessionConfigOptionValue::ValueId { value } = &request.value else {
        return responder.respond_with_error(Error::new(
            -32000,
            "placebo expects value-id session config values".to_owned(),
        ));
    };
    if state.args.expect_model_config.as_deref() == Some(value.0.as_ref())
        && request.config_id.0.as_ref() == state.args.model_config_option_id.as_str()
    {
        state.model_configured = true;
    }
    responder.respond(SetSessionConfigOptionResponse::new(Vec::new()))
}

async fn handle_load_session(
    _state: SharedState,
    _request: LoadSessionRequest,
    responder: Responder<LoadSessionResponse>,
    _connection: ConnectionTo<Client>,
) -> agent_client_protocol::Result<()> {
    responder.respond(LoadSessionResponse::new())
}

async fn handle_resume_session(
    state: SharedState,
    _request: ResumeSessionRequest,
    responder: Responder<ResumeSessionResponse>,
    _connection: ConnectionTo<Client>,
) -> agent_client_protocol::Result<()> {
    let state = state.lock().await;
    responder.respond(ResumeSessionResponse::new().config_options(state.model_config_options()))
}

async fn handle_close_session(
    _state: SharedState,
    _request: CloseSessionRequest,
    responder: Responder<CloseSessionResponse>,
    _connection: ConnectionTo<Client>,
) -> agent_client_protocol::Result<()> {
    responder.respond(CloseSessionResponse::new())
}

async fn handle_fork_session(
    state: SharedState,
    request: PlaceboForkSessionRequest,
    responder: Responder<ForkSessionResponse>,
    _connection: ConnectionTo<Client>,
) -> agent_client_protocol::Result<()> {
    let PlaceboForkSessionRequest {
        session_id: _parent_session_id,
        cwd,
        mcp_servers: _mcp_servers,
        message_id: _message_id,
    } = request;
    let mut state = state.lock().await;
    if let Some(expected) = state.args.expect_fork_message_id.as_deref()
        && _message_id.as_deref() != Some(expected)
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

async fn handle_prompt(
    state: SharedState,
    request: PromptRequest,
    responder: Responder<PromptResponse>,
    connection: ConnectionTo<Client>,
) -> agent_client_protocol::Result<()> {
    let args = {
        let state = state.lock().await;
        state.args.clone()
    };
    if args.prompt_error {
        return responder.respond_with_error(Error::new(-32000, "fake prompt failure"));
    }
    if let Some(message) = args.prompt_inference_error {
        return responder.respond_with_error(Error::new(-32000, message));
    }
    {
        let state = state.lock().await;
        if state.args.expect_model_config.is_some() && !state.model_configured {
            return responder
                .respond_with_error(Error::new(-32000, "expected model config before prompt"));
        }
    }
    if prompt_contains_testflight_marker(&request) {
        tokio::fs::write(TESTFLIGHT_MARKER, TESTFLIGHT_CONTENT)
            .await
            .map_err(Error::into_internal_error)?;
    }
    if !args.prompt_silent {
        let chunks: &[&str] = if args.prompt_stall_after_update {
            &[FIRST_CHUNK]
        } else {
            &[FIRST_CHUNK, SECOND_CHUNK]
        };
        for text in chunks {
            connection.send_notification(SessionNotification::new(
                request.session_id.clone(),
                SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                    TextContent::new(*text),
                ))),
            ))?;
        }
    }
    if args.prompt_stall_after_update {
        loop {
            tokio::time::sleep(STALL_SLEEP).await;
        }
    }
    if let Some(delay_ms) = args.prompt_response_delay_ms {
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
    }
    if let Some(message) = args.prompt_inference_error_after_update {
        return responder.respond_with_error(Error::new(-32000, message));
    }
    let stop_reason = {
        let mut state = state.lock().await;
        if state
            .cancelled_sessions
            .remove(request.session_id.0.as_ref())
        {
            StopReason::Cancelled
        } else {
            StopReason::EndTurn
        }
    };
    // Echo the local message-id extension: acp-stack stamps
    // `_meta.acpStack.messageId` on `session/prompt` and treats the same shape
    // on the response as the acknowledgment.
    let mut response = PromptResponse::new(stop_reason);
    if let Some(message_id) = request
        .meta
        .as_ref()
        .and_then(|meta| meta.get("acpStack"))
        .and_then(|stack| stack.get("messageId"))
        .and_then(serde_json::Value::as_str)
    {
        let mut stack = serde_json::Map::new();
        stack.insert(
            "messageId".to_owned(),
            serde_json::Value::String(message_id.to_owned()),
        );
        let mut meta = serde_json::Map::new();
        meta.insert("acpStack".to_owned(), serde_json::Value::Object(stack));
        response = response.meta(meta);
    }
    responder.respond(response)
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct PlaceboForkSessionRequest {
    session_id: SessionId,
    cwd: PathBuf,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    mcp_servers: Vec<agent_client_protocol::schema::v1::McpServer>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message_id: Option<String>,
}

impl JsonRpcMessage for PlaceboForkSessionRequest {
    fn matches_method(method: &str) -> bool {
        method == "session/fork"
    }

    fn method(&self) -> &str {
        "session/fork"
    }

    fn to_untyped_message(&self) -> std::result::Result<UntypedMessage, Error> {
        UntypedMessage::new("session/fork", self)
    }

    fn parse_message(method: &str, params: &impl Serialize) -> std::result::Result<Self, Error> {
        if method != "session/fork" {
            return Err(Error::method_not_found());
        }
        agent_client_protocol::util::json_cast_params(params)
    }
}

impl JsonRpcRequest for PlaceboForkSessionRequest {
    type Response = ForkSessionResponse;
}

async fn handle_cancel(
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

fn env_assertion_title(args: &AcpArgs) -> String {
    let mut failures = Vec::new();
    for name in &args.assert_env_absent {
        if std::env::var_os(name).is_some() {
            failures.push(format!("env leaked: {name}"));
        }
    }
    for name in &args.assert_env_present {
        if std::env::var_os(name).is_none() {
            failures.push(format!("env missing: {name}"));
        }
    }
    for pair in args.assert_env_not_equals.chunks_exact(2) {
        if std::env::var_os(&pair[0]).as_deref() == Some(std::ffi::OsStr::new(&pair[1])) {
            failures.push(format!("env override: {}", pair[0]));
        }
    }
    if args.assert_env_absent.is_empty()
        && args.assert_env_present.is_empty()
        && args.assert_env_not_equals.is_empty()
    {
        "ACP placebo agent".to_owned()
    } else if failures.is_empty() {
        "env assertions passed".to_owned()
    } else {
        failures.join(", ")
    }
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

fn prompt_contains_testflight_marker(request: &PromptRequest) -> bool {
    request
        .prompt
        .iter()
        .any(content_contains_testflight_marker)
}

fn content_contains_testflight_marker(content: &ContentBlock) -> bool {
    match content {
        ContentBlock::Text(text) => text.text.contains(TESTFLIGHT_MARKER),
        ContentBlock::ResourceLink(link) => {
            link.uri.contains(TESTFLIGHT_MARKER) || link.name.contains(TESTFLIGHT_MARKER)
        }
        ContentBlock::Resource(_) => false,
        ContentBlock::Image(_) | ContentBlock::Audio(_) => false,
        _ => false,
    }
}
