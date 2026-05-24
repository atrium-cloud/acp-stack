use crate::config::Config;
use crate::error::{Result, StackError};
use crate::fs_util::{
    create_dir_owner_only, home_dir, parent_dir, pre_create_owner_only, set_owner_only_file,
};
use crate::state::{EventFilter, StateStore, default_state_path};
use clap::{Args, Subcommand};

use super::core::{CliKey, daemon_base_url, open_cli_key};

#[derive(Debug, Subcommand)]
pub enum LogsCommand {
    Query(LogsQueryArgs),
    Tail(LogsTailArgs),
}

#[derive(Debug, Args)]
pub struct LogsQueryArgs {
    #[arg(long, default_value_t = 50)]
    limit: u32,
    #[arg(long)]
    level: Option<String>,
    /// Lower time bound. Accepts a duration suffix (`1h`/`30m`/`2d`) or an
    /// RFC3339 timestamp. When a suffix is supplied it's interpreted as
    /// "this much time ago".
    #[arg(long)]
    since: Option<String>,
    /// Upper time bound. Same format as `--since`. Defaults to "now".
    #[arg(long)]
    until: Option<String>,
    /// Exact event kind, or a dotted prefix when the value ends with `.`
    /// (e.g. `command.` matches `command.started`, `command.exited`, ...).
    #[arg(long)]
    kind: Option<String>,
    /// Filter by writer source (`api`, `acp`, `command`, `permission`, `cli`,
    /// `system`).
    #[arg(long)]
    source: Option<String>,
    /// Show events scoped to a single session id.
    #[arg(long)]
    session: Option<String>,
    /// Show events whose payload carries this command id.
    #[arg(long)]
    command: Option<String>,
    /// Show events whose payload carries this permission id.
    #[arg(long)]
    permission: Option<String>,
    /// Continuation cursor from a previous page (the last returned event id).
    #[arg(long)]
    after: Option<String>,
}

#[derive(Debug, Args)]
pub struct LogsTailArgs {
    /// WebSocket topic to subscribe to. May be passed multiple times. Defaults
    /// to `logs`. Valid: `logs`, `workspace`, `agent`, `status`,
    /// `sessions.{id}`, `commands.{id}`.
    #[arg(long = "topic")]
    topics: Vec<String>,
}

pub(super) fn run_logs_command(command: LogsCommand) -> Result<()> {
    match command {
        LogsCommand::Query(args) => {
            let home = home_dir()?;
            let state_path = default_state_path(&home);
            let state_dir = parent_dir(&state_path)?;
            create_dir_owner_only(state_dir)?;
            pre_create_owner_only(&state_path)?;
            let store = StateStore::open(&state_path)?;
            store.migrate()?;
            set_owner_only_file(&state_path)?;

            let now = chrono::Utc::now();
            let since = resolve_time_bound(args.since.as_deref(), "since", now)?;
            let until = resolve_time_bound(args.until.as_deref(), "until", now)?;
            let (kind_exact, kind_prefix) = match args.kind.as_deref() {
                Some(k) if k.ends_with('.') => (None, Some(k)),
                Some(k) => (Some(k), None),
                None => (None, None),
            };
            let events = store.query_events(EventFilter {
                limit: args.limit,
                level: args.level.as_deref(),
                kind: kind_exact,
                kind_prefix,
                source: args.source.as_deref(),
                session_id: args.session.as_deref(),
                command_id: args.command.as_deref(),
                permission_id: args.permission.as_deref(),
                since: since.as_deref(),
                until: until.as_deref(),
                after_id: args.after.as_deref(),
            })?;

            for event in &events {
                println!(
                    "{} {} {} {} {}",
                    event.created_at, event.level, event.source, event.kind, event.message
                );
            }
            if (events.len() as u32) == args.limit
                && let Some(last) = events.last()
            {
                eprintln!(
                    "-- more rows available; pass --after {} to continue",
                    last.id
                );
            }

            Ok(())
        }
        LogsCommand::Tail(args) => run_logs_tail(args),
    }
}

/// Accept either a duration suffix (`30m`, `1h`, `2d`) or an RFC3339
/// timestamp. The suffix form resolves relative to `now`; the RFC3339 form is
/// returned verbatim after a parse round-trip to confirm it's well-formed.
fn resolve_time_bound(
    raw: Option<&str>,
    field: &'static str,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Option<String>> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(raw) {
        return Ok(Some(
            dt.with_timezone(&chrono::Utc)
                .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
        ));
    }
    let duration =
        crate::time_util::parse_duration_suffix(raw).ok_or_else(|| StackError::InvalidParam {
            field,
            reason: format!("not a valid RFC3339 timestamp or duration: {raw}"),
        })?;
    let resolved = now - duration;
    Ok(Some(
        resolved.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
    ))
}

fn run_logs_tail(args: LogsTailArgs) -> Result<()> {
    let home = home_dir()?;
    let config = Config::load_from_default_path()?;
    let session_key = open_cli_key(&config, &home, CliKey::Session)?;
    let base_url = daemon_base_url(config.api.public_url.as_deref(), &config.api.bind)?;
    let topics = if args.topics.is_empty() {
        vec!["logs".to_owned()]
    } else {
        args.topics
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|source| StackError::ServeIo { source })?;
    runtime.block_on(tail_ws_loop(&base_url, &session_key, topics))
}

async fn tail_ws_loop(base_url: &str, session_key: &str, topics: Vec<String>) -> Result<()> {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::protocol::Message;

    let ws_url = http_to_ws_url(base_url)?;
    let url = format!("{ws_url}/v1/ws");
    let mut request = url
        .as_str()
        .into_client_request()
        .map_err(|source| StackError::ServeIo {
            source: std::io::Error::other(format!("invalid websocket url: {source}")),
        })?;
    request.headers_mut().insert(
        http::header::AUTHORIZATION,
        format!("Bearer {session_key}")
            .parse()
            .map_err(|_| StackError::ServeIo {
                source: std::io::Error::other("session key produced invalid header value"),
            })?,
    );

    let (stream, _response) =
        tokio_tungstenite::connect_async(request)
            .await
            .map_err(|source| StackError::ServeIo {
                source: std::io::Error::other(format!("websocket connect failed: {source}")),
            })?;
    let (mut writer, mut reader) = stream.split();

    let subscribe = serde_json::json!({"type": "subscribe", "topics": topics});
    writer
        .send(Message::Text(subscribe.to_string().into()))
        .await
        .map_err(|source| StackError::ServeIo {
            source: std::io::Error::other(format!("subscribe failed: {source}")),
        })?;

    eprintln!(
        "acps logs tail: subscribed to {} at {url}",
        topics.join(", ")
    );

    let mut ctrl_c = std::pin::pin!(tokio::signal::ctrl_c());
    loop {
        tokio::select! {
            biased;
            _ = &mut ctrl_c => {
                let _ = writer.send(Message::Close(None)).await;
                return Ok(());
            }
            frame = reader.next() => {
                let Some(frame) = frame else { return Ok(()); };
                let message = frame.map_err(|source| StackError::ServeIo {
                    source: std::io::Error::other(format!("websocket read failed: {source}")),
                })?;
                match message {
                    Message::Text(text) => println!("{text}"),
                    Message::Binary(_) | Message::Ping(_) | Message::Pong(_) => {}
                    Message::Close(_) => return Ok(()),
                    Message::Frame(_) => {}
                }
            }
        }
    }
}

fn http_to_ws_url(base: &str) -> Result<String> {
    let trimmed = base.trim_end_matches('/');
    if let Some(rest) = trimmed.strip_prefix("https://") {
        return Ok(format!("wss://{rest}"));
    }
    if let Some(rest) = trimmed.strip_prefix("http://") {
        return Ok(format!("ws://{rest}"));
    }
    Err(StackError::ServeIo {
        source: std::io::Error::other("daemon base url must start with http:// or https://"),
    })
}
