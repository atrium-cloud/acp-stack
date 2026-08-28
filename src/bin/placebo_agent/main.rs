use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    AgentCapabilities, CancelNotification, ClientCapabilities, CloseSessionRequest,
    CloseSessionResponse, ConfigOptionUpdate, ContentBlock, ContentChunk, CreateTerminalRequest,
    DeleteSessionRequest, DeleteSessionResponse, ForkSessionRequest, ForkSessionResponse,
    Implementation, InitializeRequest, InitializeResponse, KillTerminalRequest,
    ListSessionsRequest, ListSessionsResponse, LoadSessionRequest, LoadSessionResponse,
    McpCapabilities, NewSessionRequest, NewSessionResponse, PromptCapabilities, PromptRequest,
    PromptResponse, ReadTextFileRequest, ReleaseTerminalRequest, RequestPermissionRequest,
    ResumeSessionRequest, ResumeSessionResponse, SessionCapabilities, SessionCloseCapabilities,
    SessionConfigOption, SessionConfigOptionCategory, SessionConfigOptionValue,
    SessionConfigSelectOption, SessionDeleteCapabilities, SessionForkCapabilities, SessionId,
    SessionInfo, SessionListCapabilities, SessionMode, SessionModeState, SessionNotification,
    SessionResumeCapabilities, SessionUpdate, SetSessionConfigOptionRequest,
    SetSessionConfigOptionResponse, SetSessionModeRequest, SetSessionModeResponse, StopReason,
    TerminalId, TerminalOutputRequest, TextContent, ToolCallUpdate, ToolCallUpdateFields,
    WaitForTerminalExitRequest, WriteTextFileRequest,
};
use agent_client_protocol::schema::v1::{PermissionOption, PermissionOptionKind};
use agent_client_protocol::{Agent, Client, ConnectionTo, Dispatch, Error, Handled, Responder};
use clap::{Args, Parser, Subcommand, ValueEnum};
use tokio::sync::Mutex;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

mod cli;
mod probes;
mod prompt;
mod sessions;
mod state;

use cli::*;
use probes::*;
use prompt::*;
use sessions::*;
use state::*;

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
/// Cadence for the off-loop wait on a `session/cancel` notification, which
/// `handle_cancel` records under the same lock this poll reads.
const CANCEL_WAIT_POLL_INTERVAL: Duration = Duration::from_millis(20);

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
                    handle_set_mode(Arc::clone(&state), request, responder, connection).await
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
                    handle_delete_session(Arc::clone(&state), request, responder, connection).await
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
            // Only claim unhandled REQUESTS: swallowing a response here would
            // poison the placebo's own outbound requests.
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
