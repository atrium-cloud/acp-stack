use crate::config::Config;
use crate::error::{Result, StackError};
use crate::fs_util::home_dir;
use clap::{Args, Subcommand};

use super::core::{
    CliKey, CliMethod, daemon_base_url, daemon_request, encode_path_segment, open_cli_key,
};

#[derive(Debug, Subcommand)]
pub enum SessionsCommand {
    /// List sessions newest-first.
    List(SessionsListArgs),
    /// Create a new session through the running daemon.
    New(SessionsNewArgs),
    /// Send a prompt to a session. Polls until completion unless `--no-wait`.
    Prompt(SessionsPromptArgs),
    /// Cancel any in-flight prompts and notify the agent.
    Cancel(SessionsTargetArgs),
    /// Close the session on the agent side and mark it closed locally.
    Close(SessionsTargetArgs),
}

#[derive(Debug, Args)]
pub struct SessionsListArgs {
    #[arg(long, default_value_t = 50)]
    limit: u32,
}

#[derive(Debug, Args)]
pub struct SessionsNewArgs {
    /// Optional working directory for the new session; defaults to
    /// `workspace.root` configured for the runtime.
    #[arg(long)]
    cwd: Option<String>,
}

#[derive(Debug, Args)]
pub struct SessionsPromptArgs {
    session_id: String,
    /// Prompt text. If omitted, the CLI reads stdin until EOF.
    text: Option<String>,
    /// Return immediately with the prompt id without polling completion.
    #[arg(long)]
    no_wait: bool,
    /// Maximum seconds to wait before giving up on the prompt (ignored when
    /// `--no-wait` is set). The daemon keeps the task running regardless.
    #[arg(long, default_value_t = 300)]
    timeout_secs: u64,
}

#[derive(Debug, Args)]
pub struct SessionsTargetArgs {
    session_id: String,
}

pub(super) fn run_sessions_command(command: SessionsCommand) -> Result<()> {
    let home = home_dir()?;
    let config = Config::load_from_default_path()?;
    let session_key = open_cli_key(&config, &home, CliKey::Session)?;
    let base_url = daemon_base_url(config.api.public_url.as_deref(), &config.api.bind)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|source| StackError::ServeIo { source })?;
    runtime.block_on(async move {
        match command {
            SessionsCommand::List(args) => {
                let path = format!("/v1/sessions?limit={}", args.limit);
                let body =
                    daemon_request(&base_url, CliMethod::Get, &path, &session_key, None).await?;
                let sessions = body["data"]["sessions"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                if sessions.is_empty() {
                    println!("(no sessions)");
                } else {
                    for session in sessions {
                        let id = session["id"].as_str().unwrap_or("?");
                        let status = session["status"].as_str().unwrap_or("?");
                        let cwd = session["cwd"].as_str().unwrap_or("");
                        let updated = session["updated_at"].as_str().unwrap_or("?");
                        println!("{updated} {status} {id} {cwd}");
                    }
                }
                Ok(())
            }
            SessionsCommand::New(args) => {
                let body = serde_json::json!({
                    "cwd": args.cwd,
                    "mcp_servers": [],
                });
                let response = daemon_request(
                    &base_url,
                    CliMethod::Post,
                    "/v1/sessions",
                    &session_key,
                    Some(&body),
                )
                .await?;
                let id = response["data"]["id"].as_str().unwrap_or("?");
                let cwd = response["data"]["cwd"].as_str().unwrap_or("");
                println!("session: {id}");
                if !cwd.is_empty() {
                    println!("cwd: {cwd}");
                }
                Ok(())
            }
            SessionsCommand::Prompt(args) => {
                run_sessions_prompt(&base_url, &session_key, args).await
            }
            SessionsCommand::Cancel(args) => {
                let encoded = encode_path_segment(&args.session_id);
                let path = format!("/v1/sessions/{encoded}/cancel");
                daemon_request(&base_url, CliMethod::Post, &path, &session_key, None).await?;
                println!("session cancel: requested");
                println!("session: {}", args.session_id);
                Ok(())
            }
            SessionsCommand::Close(args) => {
                let encoded = encode_path_segment(&args.session_id);
                let path = format!("/v1/sessions/{encoded}");
                let response =
                    daemon_request(&base_url, CliMethod::Delete, &path, &session_key, None).await?;
                let status = response["data"]["status"].as_str().unwrap_or("closed");
                println!("session close: {status}");
                println!("session: {}", args.session_id);
                Ok(())
            }
        }
    })
}

async fn run_sessions_prompt(
    base_url: &str,
    session_key: &str,
    args: SessionsPromptArgs,
) -> Result<()> {
    let prompt_text = match args.text {
        Some(text) => text,
        None => {
            let mut buffer = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin().lock(), &mut buffer)
                .map_err(|source| StackError::StdinRead { source })?;
            buffer
        }
    };
    let body = serde_json::json!({ "prompt": prompt_text });
    let encoded_session = encode_path_segment(&args.session_id);
    let path = format!("/v1/sessions/{encoded_session}/prompt");
    let response =
        daemon_request(base_url, CliMethod::Post, &path, session_key, Some(&body)).await?;
    let prompt_id = response["data"]["prompt_id"]
        .as_str()
        .ok_or_else(|| StackError::AgentInitializeFailed {
            reason: "daemon prompt response missing prompt_id".to_owned(),
        })?
        .to_owned();
    if args.no_wait {
        println!("prompt: pending");
        println!("prompt_id: {prompt_id}");
        return Ok(());
    }

    let encoded_prompt = encode_path_segment(&prompt_id);
    let status_path = format!("/v1/sessions/{encoded_session}/prompts/{encoded_prompt}");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(args.timeout_secs);
    let mut delay_ms: u64 = 250;
    loop {
        if std::time::Instant::now() > deadline {
            return Err(StackError::AgentInitializeFailed {
                reason: format!(
                    "prompt did not settle within {}s (prompt_id={})",
                    args.timeout_secs, prompt_id
                ),
            });
        }
        let poll =
            daemon_request(base_url, CliMethod::Get, &status_path, session_key, None).await?;
        let status = poll["data"]["status"].as_str().unwrap_or("");
        match status {
            "completed" => {
                let stop = poll["data"]["stop_reason"].as_str().unwrap_or("end_turn");
                println!("prompt: completed");
                println!("prompt_id: {prompt_id}");
                println!("stop_reason: {stop}");
                return Ok(());
            }
            "errored" => {
                let code = poll["data"]["error_code"].as_str().unwrap_or("agent.error");
                let message = poll["data"]["error_message"].as_str().unwrap_or("");
                return Err(StackError::AgentRequestFailed {
                    method: "session/prompt",
                    message: format!("{code}: {message}"),
                });
            }
            "cancelled" => {
                println!("prompt: cancelled");
                println!("prompt_id: {prompt_id}");
                return Ok(());
            }
            _ => {
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                // Linear back-off capped at 2s so a long-running prompt does
                // not spam the daemon every 250ms for minutes on end.
                delay_ms = (delay_ms + 250).min(2000);
            }
        }
    }
}
