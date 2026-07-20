use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    AgentCapabilities, CancelNotification, ClientCapabilities, CloseSessionRequest,
    CloseSessionResponse, ContentBlock, ContentChunk, CreateTerminalRequest, ForkSessionRequest,
    ForkSessionResponse, Implementation, InitializeRequest, InitializeResponse,
    KillTerminalRequest, ListSessionsRequest, ListSessionsResponse, LoadSessionRequest,
    LoadSessionResponse, NewSessionRequest, NewSessionResponse, PromptCapabilities, PromptRequest,
    PromptResponse, ReadTextFileRequest, ReleaseTerminalRequest, RequestPermissionRequest,
    ResumeSessionRequest, ResumeSessionResponse, SessionCapabilities, SessionCloseCapabilities,
    SessionConfigOption, SessionConfigOptionCategory, SessionConfigOptionValue,
    SessionConfigSelectOption, SessionForkCapabilities, SessionId, SessionInfo,
    SessionListCapabilities, SessionNotification, SessionResumeCapabilities, SessionUpdate,
    SetSessionConfigOptionRequest, SetSessionConfigOptionResponse, StopReason, TerminalId,
    TerminalOutputRequest, TextContent, ToolCallUpdate, ToolCallUpdateFields,
    WaitForTerminalExitRequest, WriteTextFileRequest,
};
use agent_client_protocol::schema::v1::{PermissionOption, PermissionOptionKind};
use agent_client_protocol::{Agent, Client, ConnectionTo, Dispatch, Error, Handled, Responder};
use clap::{Args, Parser, Subcommand, ValueEnum};
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
    initialize_protocol_v0: bool,
    #[arg(long)]
    require_client_info: bool,
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
    request_permission_then_cancel: bool,
    #[arg(long)]
    session_list_paginated: bool,
    #[arg(long)]
    session_list_repeated_cursor: bool,
    #[arg(long)]
    model_config_option: Option<String>,
    #[arg(long, default_value = "model")]
    model_config_option_id: String,
    /// Strict-agent mode: return session config options only when the client
    /// advertised `session.configOptions` support at initialize.
    #[arg(long)]
    require_client_config_options: bool,
    /// Strict-agent mode: drive `terminal/*` only when the client advertised
    /// `terminal: true` at initialize.
    #[arg(long)]
    require_terminal: bool,
    /// During prompt handling, run this program through a client terminal and
    /// report the round-trip as a `terminal-report:` message chunk.
    #[arg(long)]
    terminal_command: Option<String>,
    #[arg(long)]
    terminal_arg: Vec<String>,
    #[arg(long)]
    terminal_byte_limit: Option<u64>,
    #[arg(long)]
    terminal_cwd: Option<PathBuf>,
    /// Kill the terminal right after creation instead of waiting for natural
    /// exit.
    #[arg(long)]
    terminal_kill: bool,
    /// Cancel the first wait request, verify the terminal remains usable,
    /// then kill and complete the normal wait/release lifecycle.
    #[arg(long)]
    terminal_cancel_wait: bool,
    /// Create the terminal and leave it running: no wait, kill, or release.
    /// Exercises the client's shutdown kill-and-release path.
    #[arg(long)]
    terminal_orphan: bool,
    /// Call `terminal/release` with an unknown id and report the error code.
    #[arg(long)]
    terminal_release_unknown: bool,
    /// Strict-agent mode: drive `fs/*` only when the client advertised both
    /// `fs.readTextFile` and `fs.writeTextFile` at initialize.
    #[arg(long)]
    require_fs: bool,
    /// During prompt handling, write this file via `fs/write_text_file` and
    /// report the round-trip.
    #[arg(long)]
    fs_write_path: Option<PathBuf>,
    #[arg(long, default_value = "fs-probe-content")]
    fs_write_content: String,
    /// During prompt handling, read this file via `fs/read_text_file`.
    #[arg(long)]
    fs_read_path: Option<PathBuf>,
    #[arg(long)]
    fs_read_line: Option<u32>,
    #[arg(long)]
    fs_read_limit: Option<u32>,
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
    client_capabilities: Option<ClientCapabilities>,
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
            client_capabilities: None,
        }
    }

    fn client_advertised_config_options(&self) -> bool {
        self.client_capabilities
            .as_ref()
            .and_then(|caps| caps.session.as_ref())
            .and_then(|session| session.config_options.as_ref())
            .is_some()
    }

    fn model_config_options(&self) -> Option<Vec<SessionConfigOption>> {
        let model = self.args.model_config_option.as_ref()?;
        if self.args.require_client_config_options && !self.client_advertised_config_options() {
            return None;
        }
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
            // Only claim unhandled REQUESTS. Responses and notifications must
            // fall through to the SDK's internal routing — swallowing a
            // response here would poison the placebo's own outbound requests
            // (e.g. the terminal probe's terminal/create).
            async move |message: Dispatch, connection: ConnectionTo<Client>| match message {
                Dispatch::Request(..) => {
                    message.respond_with_error(Error::method_not_found(), connection)?;
                    Ok(Handled::Yes)
                }
                other => Ok(Handled::No {
                    message: other,
                    retry: false,
                }),
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
    if args.request_permission_then_cancel {
        let permission_request = RequestPermissionRequest::new(
            request.session_id.clone(),
            ToolCallUpdate::new("tool_permission_cancel", ToolCallUpdateFields::new()),
            vec![PermissionOption::new(
                "allow",
                "Allow",
                PermissionOptionKind::AllowOnce,
            )],
        );
        let permission = connection.send_request(permission_request);
        permission.cancel()?;
        let state_for_task = Arc::clone(&state);
        return connection.spawn(async move {
            let Some(error) = permission.block_task().await.err() else {
                return responder.respond_with_error(Error::new(
                    -32000,
                    "cancelled permission returned a successful response",
                ));
            };
            if error.code != agent_client_protocol::ErrorCode::RequestCancelled {
                return responder.respond_with_error(Error::new(
                    -32000,
                    format!("cancelled permission returned error {}", error.code),
                ));
            }
            finish_prompt(state_for_task, request, responder).await
        });
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
    // The client probes send requests back to the client, so they must run
    // off the event loop (block_task inside a handler deadlocks). The
    // responder moves into the spawned task and answers from there.
    let probe_requested = args.terminal_command.is_some()
        || args.terminal_release_unknown
        || args.fs_write_path.is_some()
        || args.fs_read_path.is_some();
    if probe_requested {
        let (terminal_advertised, fs_advertised) = {
            let state = state.lock().await;
            let caps = state.client_capabilities.as_ref();
            (
                caps.is_some_and(|caps| caps.terminal),
                caps.is_some_and(|caps| caps.fs.read_text_file && caps.fs.write_text_file),
            )
        };
        let state_for_task = Arc::clone(&state);
        let probe_connection = connection.clone();
        return connection.spawn(async move {
            let terminal_report = run_terminal_probe(
                &args,
                &request.session_id,
                &probe_connection,
                terminal_advertised,
            )
            .await;
            let report = match terminal_report {
                Ok(mut report) => {
                    match run_fs_probe(&args, &request.session_id, &probe_connection, fs_advertised)
                        .await
                    {
                        Ok(fs_report) => {
                            report.extend(fs_report);
                            report
                        }
                        Err(error) => return responder.respond_with_error(error),
                    }
                }
                Err(error) => return responder.respond_with_error(error),
            };
            let report = serde_json::Value::Object(report);
            probe_connection.send_notification(SessionNotification::new(
                request.session_id.clone(),
                SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                    TextContent::new(format!("terminal-report:{report}")),
                ))),
            ))?;
            finish_prompt(state_for_task, request, responder).await
        });
    }
    finish_prompt(state, request, responder).await
}

/// `fs/*` round-trip driven against the client. Runs in a spawned task.
async fn run_fs_probe(
    args: &AcpArgs,
    session_id: &SessionId,
    connection: &ConnectionTo<Client>,
    fs_advertised: bool,
) -> agent_client_protocol::Result<serde_json::Map<String, serde_json::Value>> {
    let mut report = serde_json::Map::new();
    if args.fs_write_path.is_none() && args.fs_read_path.is_none() {
        return Ok(report);
    }
    if args.require_fs && !fs_advertised {
        report.insert(
            "fs_skipped".to_owned(),
            serde_json::json!("fs-not-advertised"),
        );
        return Ok(report);
    }
    if let Some(path) = &args.fs_write_path {
        let write = WriteTextFileRequest::new(
            session_id.clone(),
            path.clone(),
            args.fs_write_content.clone(),
        );
        match connection.send_request(write).block_task().await {
            Ok(_) => {
                report.insert("fs_write_ok".to_owned(), serde_json::json!(true));
            }
            Err(error) => {
                report.insert(
                    "fs_write_error_code".to_owned(),
                    serde_json::json!(error.code),
                );
            }
        }
    }
    if let Some(path) = &args.fs_read_path {
        let mut read = ReadTextFileRequest::new(session_id.clone(), path.clone());
        if let Some(line) = args.fs_read_line {
            read = read.line(line);
        }
        if let Some(limit) = args.fs_read_limit {
            read = read.limit(limit);
        }
        match connection.send_request(read).block_task().await {
            Ok(response) => {
                report.insert(
                    "fs_read_content".to_owned(),
                    serde_json::json!(response.content),
                );
            }
            Err(error) => {
                report.insert(
                    "fs_read_error_code".to_owned(),
                    serde_json::json!(error.code),
                );
            }
        }
    }
    Ok(report)
}

/// Terminal round-trip driven against the client, reported as a JSON object.
/// Runs in a spawned task; `block_task` is safe here.
async fn run_terminal_probe(
    args: &AcpArgs,
    session_id: &SessionId,
    connection: &ConnectionTo<Client>,
    terminal_advertised: bool,
) -> agent_client_protocol::Result<serde_json::Map<String, serde_json::Value>> {
    let mut report = serde_json::Map::new();
    if args.terminal_release_unknown {
        let error = connection
            .send_request(ReleaseTerminalRequest::new(
                session_id.clone(),
                TerminalId::new("term_unknown"),
            ))
            .block_task()
            .await
            .err();
        report.insert(
            "release_unknown_error_code".to_owned(),
            serde_json::json!(error.map(|error| error.code)),
        );
    }
    let Some(command) = &args.terminal_command else {
        return Ok(report);
    };
    if args.require_terminal && !terminal_advertised {
        report.insert(
            "skipped".to_owned(),
            serde_json::json!("terminal-not-advertised"),
        );
        return Ok(report);
    }
    let mut create = CreateTerminalRequest::new(session_id.clone(), command.clone())
        .args(args.terminal_arg.clone());
    if let Some(limit) = args.terminal_byte_limit {
        create = create.output_byte_limit(limit);
    }
    if let Some(cwd) = &args.terminal_cwd {
        create = create.cwd(cwd.clone());
    }
    let created = match connection.send_request(create).block_task().await {
        Ok(created) => created,
        Err(error) => {
            report.insert(
                "create_error_code".to_owned(),
                serde_json::json!(error.code),
            );
            return Ok(report);
        }
    };
    let terminal_id = created.terminal_id;
    if args.terminal_orphan {
        report.insert("orphaned".to_owned(), serde_json::json!(true));
        return Ok(report);
    }
    if args.terminal_cancel_wait {
        let wait = connection.send_request(WaitForTerminalExitRequest::new(
            session_id.clone(),
            terminal_id.clone(),
        ));
        wait.cancel()?;
        let error = wait.block_task().await.err();
        report.insert(
            "cancelled_wait_error_code".to_owned(),
            serde_json::json!(error.map(|error| error.code)),
        );
        let output = connection
            .send_request(TerminalOutputRequest::new(
                session_id.clone(),
                terminal_id.clone(),
            ))
            .block_task()
            .await?;
        report.insert(
            "output_after_cancel_ok".to_owned(),
            serde_json::json!(!output.truncated),
        );
    }
    if args.terminal_kill || args.terminal_cancel_wait {
        // Poll until the child has produced output before killing, so the
        // output-stays-readable-after-kill assertion is deterministic.
        for _ in 0..100 {
            let output = connection
                .send_request(TerminalOutputRequest::new(
                    session_id.clone(),
                    terminal_id.clone(),
                ))
                .block_task()
                .await?;
            if !output.output.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        connection
            .send_request(KillTerminalRequest::new(
                session_id.clone(),
                terminal_id.clone(),
            ))
            .block_task()
            .await?;
    }
    let exit = connection
        .send_request(WaitForTerminalExitRequest::new(
            session_id.clone(),
            terminal_id.clone(),
        ))
        .block_task()
        .await?;
    report.insert(
        "exit_code".to_owned(),
        serde_json::json!(exit.exit_status.exit_code),
    );
    report.insert(
        "signal".to_owned(),
        serde_json::json!(exit.exit_status.signal),
    );
    let output = connection
        .send_request(TerminalOutputRequest::new(
            session_id.clone(),
            terminal_id.clone(),
        ))
        .block_task()
        .await?;
    report.insert("output".to_owned(), serde_json::json!(output.output));
    report.insert("truncated".to_owned(), serde_json::json!(output.truncated));
    connection
        .send_request(ReleaseTerminalRequest::new(
            session_id.clone(),
            terminal_id.clone(),
        ))
        .block_task()
        .await?;
    let post_release_error = connection
        .send_request(TerminalOutputRequest::new(
            session_id.clone(),
            terminal_id.clone(),
        ))
        .block_task()
        .await
        .err();
    report.insert(
        "post_release_error_code".to_owned(),
        serde_json::json!(post_release_error.map(|error| error.code)),
    );
    Ok(report)
}

async fn finish_prompt(
    state: SharedState,
    request: PromptRequest,
    responder: Responder<PromptResponse>,
) -> agent_client_protocol::Result<()> {
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
