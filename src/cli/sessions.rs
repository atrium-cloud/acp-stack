use crate::config::Config;
use crate::error::{Result, StackError};
use crate::state::DEFAULT_SESSION_STATUS_WINDOW;
use chrono::{SecondsFormat, Utc};
use clap::{Args, Subcommand};

use super::core::{
    CliMethod, OutputFormat, SessionAccess, daemon_base_url, daemon_request, encode_path_segment,
    local_daemon_request, print_json, resolve_session_access,
};

const DEFAULT_SESSION_LIST_LIMIT: u32 = 50;
const DEFAULT_SESSION_STATUS_LIMIT: u32 = 1000;
const SESSION_LIST_DEFAULT_RANGE: &str = "month";

#[derive(Debug, Subcommand)]
pub enum SessionsCommand {
    /// List sessions newest-first.
    List(SessionsListArgs),
    /// Show compact status for active sessions.
    Status(SessionsStatusArgs),
    /// Create a new session through the running daemon.
    New(SessionsNewArgs),
    /// Fork an existing session through ACP.
    Fork(SessionsForkArgs),
    /// Send a prompt to a session. Polls until completion unless `--no-wait`.
    Prompt(SessionsPromptArgs),
    /// Cancel any in-flight prompts and notify the agent.
    Cancel(SessionsTargetArgs),
    /// Close the session on the agent side and mark it closed locally.
    Close(SessionsTargetArgs),
}

#[derive(Debug, Args)]
pub struct SessionsListArgs {
    #[arg(long, default_value_t = DEFAULT_SESSION_LIST_LIMIT)]
    limit: u32,
    /// Rolling range for sessions by updated time: day, week, month, year,
    /// all, or a duration like 60d.
    #[arg(long)]
    range: Option<String>,
    /// Explicit lower bound. Accepts RFC3339 or duration suffixes like 30d.
    #[arg(long)]
    range_start: Option<String>,
    /// Explicit upper bound. Accepts RFC3339 or duration suffixes like 30d.
    #[arg(long)]
    range_end: Option<String>,
}

#[derive(Debug, Args)]
pub struct SessionsStatusArgs {
    /// Rolling activity window. Accepts duration suffixes from 1m through 999h.
    #[arg(long, default_value = DEFAULT_SESSION_STATUS_WINDOW)]
    window: String,
    /// Recent-activity threshold. Kept for compatibility; accepts values like 15m or 1h.
    #[arg(long)]
    threshold: Option<String>,
    #[arg(long, default_value_t = DEFAULT_SESSION_STATUS_LIMIT)]
    limit: u32,
}

#[derive(Debug, Args)]
pub struct SessionsNewArgs {
    /// Optional working directory for the new session; defaults to
    /// `workspace.root` configured for the runtime.
    #[arg(long)]
    cwd: Option<String>,
    /// Session API key. Falls back to ACP_STACK_SESSION_KEY.
    #[arg(long = "session-key")]
    session_key: Option<String>,
}

#[derive(Debug, Args)]
pub struct SessionsForkArgs {
    session_id: String,
    /// Optional working directory for the forked session.
    #[arg(long)]
    cwd: Option<String>,
    /// Optional ACP prompt message id to fork from.
    #[arg(long)]
    message_id: Option<String>,
    /// Session API key. Falls back to ACP_STACK_SESSION_KEY.
    #[arg(long = "session-key")]
    session_key: Option<String>,
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
    /// Session API key. Falls back to ACP_STACK_SESSION_KEY.
    #[arg(long = "session-key")]
    session_key: Option<String>,
}

#[derive(Debug, Args)]
pub struct SessionsTargetArgs {
    session_id: String,
    /// Session API key. Falls back to ACP_STACK_SESSION_KEY.
    #[arg(long = "session-key")]
    session_key: Option<String>,
}

pub(super) fn run_sessions_command(command: SessionsCommand, output: OutputFormat) -> Result<()> {
    let config = Config::load_from_default_path()?;
    let base_url = daemon_base_url(config.api.public_url.as_deref(), &config.api.bind)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|source| StackError::ServeIo { source })?;
    runtime.block_on(async move {
        match command {
            SessionsCommand::List(args) => {
                let path = sessions_list_path(&args, args.limit.saturating_add(1))?;
                let body = local_daemon_request(&config, CliMethod::Get, &path, None).await?;
                let mut sessions = body["data"]["sessions"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                let truncated = sessions.len() > args.limit as usize;
                if truncated {
                    sessions.truncate(args.limit as usize);
                }
                if output.is_json() {
                    print_json(&serde_json::json!({
                        "sessions": sessions,
                        "truncated": truncated,
                        "limit": args.limit,
                    }))?;
                } else if sessions.is_empty() && !truncated {
                    println!("(no sessions)");
                } else {
                    for session in sessions {
                        let id = session["id"].as_str().unwrap_or("?");
                        let status = session["status"].as_str().unwrap_or("?");
                        let cwd = session["cwd"].as_str().unwrap_or("");
                        let updated = session["updated_at"].as_str().unwrap_or("?");
                        println!("{updated} {status} {id} {cwd}");
                    }
                    if truncated {
                        println!("{}", session_list_limit_hint(args.limit));
                    }
                }
                Ok(())
            }
            SessionsCommand::Status(args) => {
                let mut path = format!(
                    "/v1/sessions/-/status?window={}&limit={}",
                    encode_query_value(&args.window),
                    args.limit,
                );
                if let Some(threshold) = args.threshold.as_deref() {
                    path.push_str("&threshold=");
                    path.push_str(&encode_query_value(threshold));
                }
                let body = local_daemon_request(&config, CliMethod::Get, &path, None).await?;
                let sessions = body["data"]["sessions"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                if output.is_json() {
                    print_json(body.get("data").unwrap_or(&body))?;
                } else if sessions.is_empty() {
                    println!("No session activity in window.");
                } else {
                    for session in sessions {
                        let id = session["id"].as_str().unwrap_or("?");
                        let state = session["state"].as_str().unwrap_or("?");
                        let last = session["last_activity_at"].as_str().unwrap_or("?");
                        let actor = session["last_activity_from"].as_str().unwrap_or("?");
                        let cwd = session["cwd"].as_str().unwrap_or("");
                        let prompt = session["prompt"]["id"].as_str();
                        let permission = session["permission"]["id"].as_str();
                        let prompt_part = prompt
                            .map(|prompt_id| format!(" prompt={prompt_id}"))
                            .unwrap_or_default();
                        let permission_part = permission
                            .map(|permission_id| format!(" permission={permission_id}"))
                            .unwrap_or_default();
                        if let Some(title) = session["title"].as_str()
                            && !title.is_empty()
                        {
                            println!(
                                "{state} last_activity={last} from={actor} session={id}{prompt_part}{permission_part} title={title} cwd={cwd}"
                            );
                            continue;
                        }
                        println!(
                            "{state} last_activity={last} from={actor} session={id}{prompt_part}{permission_part} cwd={cwd}"
                        );
                    }
                }
                Ok(())
            }
            SessionsCommand::New(args) => {
                let session_access = resolve_session_access(&config, args.session_key)?;
                let body = serde_json::json!({
                    "cwd": args.cwd,
                    "mcp_servers": [],
                });
                let response = session_daemon_request(
                    &config,
                    &base_url,
                    &session_access,
                    CliMethod::Post,
                    "/v1/sessions",
                    Some(&body),
                )
                .await?;
                let id = response["data"]["id"].as_str().unwrap_or("?");
                let cwd = response["data"]["cwd"].as_str().unwrap_or("");
                if output.is_json() {
                    print_json(response.get("data").unwrap_or(&response))?;
                } else {
                    println!("session: {id}");
                    if !cwd.is_empty() {
                        println!("cwd: {cwd}");
                    }
                }
                Ok(())
            }
            SessionsCommand::Fork(args) => {
                let session_access = resolve_session_access(&config, args.session_key.clone())?;
                let body = serde_json::json!({
                    "cwd": args.cwd,
                    "message_id": args.message_id,
                });
                let encoded = encode_path_segment(&args.session_id);
                let path = format!("/v1/sessions/{encoded}/fork");
                let response = session_daemon_request(
                    &config,
                    &base_url,
                    &session_access,
                    CliMethod::Post,
                    &path,
                    Some(&body),
                )
                .await?;
                let id = response["data"]["id"].as_str().unwrap_or("?");
                let cwd = response["data"]["cwd"].as_str().unwrap_or("");
                if output.is_json() {
                    print_json(response.get("data").unwrap_or(&response))?;
                } else {
                    println!("session: {id}");
                    println!("parent: {}", args.session_id);
                    if !cwd.is_empty() {
                        println!("cwd: {cwd}");
                    }
                }
                Ok(())
            }
            SessionsCommand::Prompt(args) => {
                let session_access = resolve_session_access(&config, args.session_key.clone())?;
                run_sessions_prompt(&config, &base_url, &session_access, args, output).await
            }
            SessionsCommand::Cancel(args) => {
                let session_access = resolve_session_access(&config, args.session_key)?;
                let encoded = encode_path_segment(&args.session_id);
                let path = format!("/v1/sessions/{encoded}/cancel");
                session_daemon_request(
                    &config,
                    &base_url,
                    &session_access,
                    CliMethod::Post,
                    &path,
                    None,
                )
                .await?;
                if output.is_json() {
                    print_json(&serde_json::json!({
                        "status": "requested",
                        "session_id": args.session_id,
                    }))?;
                } else {
                    println!("session cancel: requested");
                    println!("session: {}", args.session_id);
                }
                Ok(())
            }
            SessionsCommand::Close(args) => {
                let session_access = resolve_session_access(&config, args.session_key)?;
                let encoded = encode_path_segment(&args.session_id);
                let path = format!("/v1/sessions/{encoded}");
                let response = session_daemon_request(
                    &config,
                    &base_url,
                    &session_access,
                    CliMethod::Delete,
                    &path,
                    None,
                )
                .await?;
                let status = response["data"]["status"].as_str().unwrap_or("closed");
                if output.is_json() {
                    print_json(response.get("data").unwrap_or(&response))?;
                } else {
                    println!("session close: {status}");
                    println!("session: {}", args.session_id);
                }
                Ok(())
            }
        }
    })
}

fn sessions_list_path(args: &SessionsListArgs, query_limit: u32) -> Result<String> {
    let now = Utc::now();
    let explicit_bound_mode = args.range_start.is_some() || args.range_end.is_some();
    let since = match args.range_start.as_deref() {
        Some(raw) => resolve_time_bound(Some(raw), "range-start", now)?,
        None => None,
    };
    let until = resolve_time_bound(args.range_end.as_deref(), "range-end", now)?;
    let mut query = format!("limit={query_limit}");
    let range = args
        .range
        .as_deref()
        .or_else(|| (!explicit_bound_mode).then_some(SESSION_LIST_DEFAULT_RANGE));
    if let Some(range) = range {
        validate_session_range(range)?;
        query.push_str("&range=");
        query.push_str(&encode_query_value(range));
    }
    if explicit_bound_mode && args.range.is_none() {
        query.push_str("&resolve_bounds=true");
    }
    if let Some(since) = since {
        query.push_str("&since=");
        query.push_str(&encode_query_value(&since));
    }
    if let Some(until) = until {
        query.push_str("&until=");
        query.push_str(&encode_query_value(&until));
    }
    Ok(format!("/v1/sessions?{query}"))
}

fn session_list_limit_hint(limit: u32) -> String {
    format!(
        "Showing the first {limit} results. Use --limit <number> to change session display limit."
    )
}

fn validate_session_range(raw: &str) -> Result<()> {
    match raw {
        "all" | "day" | "week" | "month" | "year" => Ok(()),
        other if crate::time_util::parse_coarse_duration_suffix(other).is_some() => Ok(()),
        other => Err(StackError::InvalidParam {
            field: "range",
            reason: format!(
                "expected day, week, month, year, all, or a duration like 30m, 60d, 6mo, or 1y; got {other}"
            ),
        }),
    }
}

fn resolve_time_bound(
    raw: Option<&str>,
    field: &'static str,
    now: chrono::DateTime<Utc>,
) -> Result<Option<String>> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(raw) {
        return Ok(Some(
            dt.with_timezone(&Utc)
                .to_rfc3339_opts(SecondsFormat::Nanos, true),
        ));
    }
    let duration = crate::time_util::parse_coarse_duration_suffix(raw).ok_or_else(|| {
        StackError::InvalidParam {
            field,
            reason: format!("not a valid RFC3339 timestamp or duration (m, h, d, w, mo, y): {raw}"),
        }
    })?;
    let resolved =
        crate::time_util::resolve_since_after_unix_epoch(duration, now).ok_or_else(|| {
            StackError::InvalidParam {
                field,
                reason: "duration range must not begin before 1970-01-01T00:00:00Z".to_owned(),
            }
        })?;
    Ok(Some(resolved.to_rfc3339_opts(SecondsFormat::Nanos, true)))
}

fn encode_query_value(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        let safe = matches!(byte,
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~'
        );
        if safe {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

async fn run_sessions_prompt(
    config: &Config,
    base_url: &str,
    session_access: &SessionAccess,
    args: SessionsPromptArgs,
    output: OutputFormat,
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
    let response = session_daemon_request(
        config,
        base_url,
        session_access,
        CliMethod::Post,
        &path,
        Some(&body),
    )
    .await?;
    let prompt_id = response["data"]["prompt_id"]
        .as_str()
        .ok_or_else(|| StackError::AgentInitializeFailed {
            reason: "daemon prompt response missing prompt_id".to_owned(),
        })?
        .to_owned();
    if args.no_wait {
        if output.is_json() {
            print_json(&serde_json::json!({
                "status": "pending",
                "prompt_id": prompt_id,
            }))?;
        } else {
            println!("prompt: pending");
            println!("prompt_id: {prompt_id}");
        }
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
        let poll = session_daemon_request(
            config,
            base_url,
            session_access,
            CliMethod::Get,
            &status_path,
            None,
        )
        .await?;
        let status = poll["data"]["status"].as_str().unwrap_or("");
        match status {
            "completed" => {
                let stop = poll["data"]["stop_reason"].as_str().unwrap_or("end_turn");
                if output.is_json() {
                    print_json(poll.get("data").unwrap_or(&poll))?;
                } else {
                    println!("prompt: completed");
                    println!("prompt_id: {prompt_id}");
                    println!("stop_reason: {stop}");
                }
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
                if output.is_json() {
                    print_json(poll.get("data").unwrap_or(&poll))?;
                } else {
                    println!("prompt: cancelled");
                    println!("prompt_id: {prompt_id}");
                }
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

async fn session_daemon_request(
    config: &Config,
    base_url: &str,
    session_access: &SessionAccess,
    method: CliMethod,
    path: &str,
    body: Option<&serde_json::Value>,
) -> Result<serde_json::Value> {
    match session_access {
        SessionAccess::Bearer(session_key) => {
            daemon_request(base_url, method, path, session_key, body).await
        }
        SessionAccess::Local => local_daemon_request(config, method, path, body).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_path_defaults_to_month_range() {
        let path = sessions_list_path(
            &SessionsListArgs {
                limit: 50,
                range: None,
                range_start: None,
                range_end: None,
            },
            51,
        )
        .expect("list path");

        assert_eq!(path, "/v1/sessions?limit=51&range=month");
    }

    #[test]
    fn range_accepts_duration_values() {
        validate_session_range("60d").expect("duration range");
    }

    #[test]
    fn range_accepts_month_and_year_duration_values() {
        validate_session_range("6mo").expect("month duration range");
        validate_session_range("1y").expect("year duration range");
    }

    #[test]
    fn range_accepts_minute_granularity() {
        validate_session_range("30m").expect("minute duration range");
    }

    #[test]
    fn range_all_is_valid() {
        validate_session_range("all").expect("all range");
    }

    #[test]
    fn explicit_bounds_override_range_defaults() {
        let path = sessions_list_path(
            &SessionsListArgs {
                limit: 50,
                range: Some("year".to_owned()),
                range_start: Some("2026-05-01T00:00:00Z".to_owned()),
                range_end: Some("7d".to_owned()),
            },
            51,
        )
        .expect("list path");

        assert!(path.starts_with("/v1/sessions?limit=51&range=year&since=2026-05-01T00%3A00%3A00"));
        assert!(path.contains("&until="));
    }

    #[test]
    fn explicit_bounds_without_range_request_bound_resolution() {
        let path = sessions_list_path(
            &SessionsListArgs {
                limit: 50,
                range: None,
                range_start: Some("2026-05-01T00:00:00Z".to_owned()),
                range_end: None,
            },
            51,
        )
        .expect("list path");

        assert!(path.contains("&since=2026-05-01T00%3A00%3A00"));
        assert!(path.contains("&resolve_bounds=true"));
        assert!(!path.contains("&range="));
    }

    #[test]
    fn list_limit_hint_names_display_limit() {
        assert_eq!(
            session_list_limit_hint(100),
            "Showing the first 100 results. Use --limit <number> to change session display limit."
        );
    }
}
