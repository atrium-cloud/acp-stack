use crate::config::Config;
use crate::error::{Result, StackError};
use crate::fs_util::{
    create_dir_owner_only, home_dir, parent_dir, pre_create_owner_only, set_owner_only_file,
};
use crate::state::{
    Event, EventFilter, LogOrder, SecurityCategory, StateStore, default_state_path,
};
use clap::{Args, Subcommand};
use futures::{SinkExt, StreamExt};
use serde::Serialize;
use serde_json::Value;
use std::io::Write;
use std::str::FromStr;

use super::core::{OutputFormatChoice, daemon_base_url, resolve_session_key};

// === CONSTANTS ===

/// Default `--limit` used by `acps logs query`. Matches the historical 50-row
/// default the operator was already getting before the JSON flag landed.
const DEFAULT_QUERY_LIMIT: u32 = 50;

/// CLI sort-direction tokens. Kept narrow on purpose: an unknown value should
/// fail the parse, not silently fall back to a default.
const ORDER_TOKEN_ASC: &str = "asc";
const ORDER_TOKEN_DESC: &str = "desc";

/// WebSocket topic clients subscribe to for the unified `events` fanout. Used
/// by both `acps logs tail` and `acps logs query --follow`.
const WS_TOPIC_LOGS: &str = "logs";

#[derive(Debug, Subcommand)]
pub enum LogsCommand {
    // Boxed because `LogsQueryArgs` is materially larger than `LogsTailArgs`
    // and clippy's `large_enum_variant` would otherwise complain. The box only
    // costs an allocation per `acps logs query` invocation, which is fine.
    Query(Box<LogsQueryArgs>),
    Tail(LogsTailArgs),
}

#[derive(Debug, Args)]
pub struct LogsQueryArgs {
    #[arg(long, default_value_t = DEFAULT_QUERY_LIMIT)]
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
    /// Security-event category (`rate_limit`, `origin_cors`, `ip_block`,
    /// `oversized_request`). Constrains the query to the matching
    /// `security.*` kinds.
    #[arg(long)]
    category: Option<String>,
    /// Continuation cursor from a previous page (the last returned event id).
    /// Mutually exclusive with `--follow`; a watermark from a backfill scan
    /// drives follow-mode instead.
    #[arg(long)]
    after: Option<String>,
    /// Sort direction. `desc` (default) returns newest-first; `asc` returns
    /// oldest-first. Incompatible with `--follow`, which always uses `asc`
    /// for its backfill so the live tail extends the page naturally.
    #[arg(long, conflicts_with = "follow")]
    order: Option<String>,
    /// Emit a JSON object envelope (`{ events, next_cursor }`) to stdout
    /// instead of the text rendering. Suppresses the human "-- more rows
    /// available" hint that text mode prints to stderr.
    #[arg(long)]
    json: bool,
    /// After printing the backfill, attach to the daemon's `logs` WebSocket
    /// topic and keep printing matching events. The backfill always runs in
    /// `asc` order so the live tail picks up where it left off. Incompatible
    /// with `--after` (the watermark is derived from the backfill) and with
    /// `--order` (direction is fixed).
    #[arg(long, conflicts_with = "after")]
    follow: bool,
    /// Session API key for --follow. Falls back to ACP_STACK_SESSION_KEY.
    #[arg(long = "session-key")]
    session_key: Option<String>,
}

#[derive(Debug, Args)]
pub struct LogsTailArgs {
    /// WebSocket topic to subscribe to. May be passed multiple times. Defaults
    /// to `logs`. Valid: `logs`, `workspace`, `agent`, `status`,
    /// `sessions.{id}`, `commands.{id}`.
    #[arg(long = "topic")]
    topics: Vec<String>,
    /// Session API key. Falls back to ACP_STACK_SESSION_KEY.
    #[arg(long = "session-key")]
    session_key: Option<String>,
}

/// JSON envelope returned by `acps logs query --json`. Mirrors the HTTP
/// `LogsEventsResponse` field shape so a client that already consumes the API
/// can reuse the same deserializer when piping CLI output.
#[derive(Debug, Serialize)]
struct LogsQueryOutput {
    events: Vec<EventJson>,
    next_cursor: Option<String>,
}

#[derive(Debug, Serialize)]
struct EventJson {
    id: String,
    created_at: String,
    level: String,
    kind: String,
    message: String,
    payload_json: String,
    source: String,
}

impl From<&Event> for EventJson {
    fn from(event: &Event) -> Self {
        Self {
            id: event.id.clone(),
            created_at: event.created_at.clone(),
            level: event.level.clone(),
            kind: event.kind.clone(),
            message: event.message.clone(),
            payload_json: event.payload_json.clone(),
            source: event.source.clone(),
        }
    }
}

/// Owned watermark used by follow mode to drop live frames that the backfill
/// already printed. The keyset cursor for `events` is `(created_at, id)`.
#[derive(Debug, Clone, Default)]
struct Watermark {
    created_at: String,
    id: String,
}

impl Watermark {
    fn is_strictly_after(&self, event_created_at: &str, event_id: &str) -> bool {
        if self.created_at.is_empty() {
            return true;
        }
        match event_created_at.cmp(self.created_at.as_str()) {
            std::cmp::Ordering::Greater => true,
            std::cmp::Ordering::Less => false,
            std::cmp::Ordering::Equal => event_id > self.id.as_str(),
        }
    }
}

pub(super) fn run_logs_command(command: LogsCommand, output: OutputFormatChoice) -> Result<()> {
    match command {
        LogsCommand::Query(args) => run_logs_query(*args, output),
        LogsCommand::Tail(args) => {
            output.reject_json("logs tail")?;
            run_logs_tail(args)
        }
    }
}

fn run_logs_query(args: LogsQueryArgs, output: OutputFormatChoice) -> Result<()> {
    let format = output.resolve_json_alias(args.json, "json")?;
    let security_category = match args.category.as_deref() {
        None => None,
        Some(value) => Some(SecurityCategory::from_str(value)?),
    };
    // `--order` and `--follow` are mutually exclusive via clap; when
    // `--follow` is set we always backfill ASC so the live tail picks up
    // right after the last printed row. Without `--follow`, parse the user's
    // `--order` (defaulting to DESC for newest-first).
    let backfill_order = if args.follow {
        LogOrder::Asc
    } else {
        match args.order.as_deref() {
            Some(value) => parse_order_token(value)?,
            None => LogOrder::Desc,
        }
    };

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
    let live_filter = OwnedLogFilter {
        level: args.level.clone(),
        kind: args.kind.clone(),
        source: args.source.clone(),
        session_id: args.session.clone(),
        command_id: args.command.clone(),
        permission_id: args.permission.clone(),
        security_category,
        since: since.clone(),
        until: until.clone(),
    };

    if args.follow {
        let session_context = open_ws_session_context(args.session_key.clone())?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|source| StackError::ServeIo { source })?;
        return runtime.block_on(follow_query_loop(
            session_context,
            &store,
            live_filter,
            args.limit,
            format.is_json(),
        ));
    }

    let filter = EventFilter {
        limit: args.limit,
        level: args.level.as_deref(),
        kind: kind_exact,
        kind_prefix,
        source: args.source.as_deref(),
        session_id: args.session.as_deref(),
        command_id: args.command.as_deref(),
        permission_id: args.permission.as_deref(),
        security_category,
        since: since.as_deref(),
        until: until.as_deref(),
        after_id: args.after.as_deref(),
        order: backfill_order,
    };
    let events = store.query_events(filter)?;
    drop(store);

    let next_cursor = next_cursor_from(&events, args.limit);

    if format.is_json() {
        let output = LogsQueryOutput {
            events: events.iter().map(EventJson::from).collect(),
            next_cursor: next_cursor.clone(),
        };
        let rendered =
            serde_json::to_string_pretty(&output).map_err(|source| StackError::ServeIo {
                source: std::io::Error::other(format!("serialize logs query JSON: {source}")),
            })?;
        println!("{rendered}");
    } else {
        for event in &events {
            println!(
                "{} {} {} {} {}",
                event.created_at, event.level, event.source, event.kind, event.message
            );
        }
        if !args.follow
            && (events.len() as u32) == args.limit
            && let Some(last) = events.last()
        {
            eprintln!(
                "-- more rows available; pass --after {} to continue",
                last.id
            );
        }
    }

    Ok(())
}

fn parse_order_token(value: &str) -> Result<LogOrder> {
    match value {
        ORDER_TOKEN_DESC => Ok(LogOrder::Desc),
        ORDER_TOKEN_ASC => Ok(LogOrder::Asc),
        other => Err(StackError::InvalidParam {
            field: "order",
            reason: format!("expected `{ORDER_TOKEN_ASC}` or `{ORDER_TOKEN_DESC}`; got `{other}`"),
        }),
    }
}

/// Promote the saturated-page heuristic into a helper so JSON mode and text
/// mode pick the same cursor. Mirrors `paging_cursor` in the API layer.
fn next_cursor_from(events: &[Event], limit: u32) -> Option<String> {
    if (events.len() as u32) < limit {
        return None;
    }
    events.last().map(|event| event.id.clone())
}

fn write_event_line(writer: &mut impl Write, event: &Event, json_output: bool) -> Result<()> {
    if json_output {
        let line = serde_json::to_string(&EventJson::from(event)).map_err(|source| {
            StackError::ServeIo {
                source: std::io::Error::other(format!("serialize event JSON line: {source}")),
            }
        })?;
        writeln!(writer, "{line}").map_err(|source| StackError::ServeIo { source })?;
    } else {
        writeln!(
            writer,
            "{} {} {} {} {}",
            event.created_at, event.level, event.source, event.kind, event.message
        )
        .map_err(|source| StackError::ServeIo { source })?;
    }
    Ok(())
}

fn event_watermark(event: &Event) -> Watermark {
    Watermark {
        created_at: event.created_at.clone(),
        id: event.id.clone(),
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

/// Filter dimensions for the live-tail path. Owned strings so the long-lived
/// WS loop doesn't have to thread lifetimes through the futures it returns.
#[derive(Debug, Clone, Default)]
struct OwnedLogFilter {
    level: Option<String>,
    kind: Option<String>,
    source: Option<String>,
    session_id: Option<String>,
    command_id: Option<String>,
    permission_id: Option<String>,
    security_category: Option<SecurityCategory>,
    since: Option<String>,
    until: Option<String>,
}

impl OwnedLogFilter {
    fn as_log_filter<'a>(
        &'a self,
        limit: u32,
        after_id: Option<&'a str>,
        order: LogOrder,
    ) -> EventFilter<'a> {
        let (kind_exact, kind_prefix) = match self.kind.as_deref() {
            Some(k) if k.ends_with('.') => (None, Some(k)),
            Some(k) => (Some(k), None),
            None => (None, None),
        };
        EventFilter {
            limit,
            level: self.level.as_deref(),
            kind: kind_exact,
            kind_prefix,
            source: self.source.as_deref(),
            session_id: self.session_id.as_deref(),
            command_id: self.command_id.as_deref(),
            permission_id: self.permission_id.as_deref(),
            security_category: self.security_category,
            since: self.since.as_deref(),
            until: self.until.as_deref(),
            after_id,
            order,
        }
    }
}

/// Pre-resolved WS connection inputs shared between `tail` and follow-mode
/// `query`. Bundling them here means both modes go through the same config
/// load + session-key open path.
struct WsSessionContext {
    session_key: String,
    base_url: String,
}

fn open_ws_session_context(session_key: Option<String>) -> Result<WsSessionContext> {
    let config = Config::load_from_default_path()?;
    let session_key = resolve_session_key(session_key)?;
    let base_url = daemon_base_url(config.api.public_url.as_deref(), &config.api.bind)?;
    Ok(WsSessionContext {
        session_key,
        base_url,
    })
}

fn run_logs_tail(args: LogsTailArgs) -> Result<()> {
    let context = open_ws_session_context(args.session_key)?;
    let topics = if args.topics.is_empty() {
        vec![WS_TOPIC_LOGS.to_owned()]
    } else {
        args.topics
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|source| StackError::ServeIo { source })?;
    runtime.block_on(tail_ws_loop(
        &context.base_url,
        &context.session_key,
        topics,
    ))
}

async fn tail_ws_loop(base_url: &str, session_key: &str, topics: Vec<String>) -> Result<()> {
    use tokio_tungstenite::tungstenite::protocol::Message;

    let (mut writer, mut reader) = open_ws_stream(base_url, session_key).await?;
    let subscribe = serde_json::json!({"type": "subscribe", "topics": topics});
    writer
        .send(Message::Text(subscribe.to_string().into()))
        .await
        .map_err(|source| StackError::ServeIo {
            source: std::io::Error::other(format!("subscribe failed: {source}")),
        })?;

    let ws_url = http_to_ws_url(base_url)?;
    eprintln!(
        "acps logs tail: subscribed to {} at {ws_url}/v1/ws",
        topics.join(", ")
    );

    let mut ctrl_c = std::pin::pin!(tokio::signal::ctrl_c());
    loop {
        tokio::select! {
            biased;
            _ = &mut ctrl_c => {
                // Best-effort close; if the writer is already gone we surface
                // an error rather than mask it with `let _ =`.
                if let Err(error) = writer.send(Message::Close(None)).await {
                    eprintln!("acps logs tail: close send failed: {error}");
                }
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

async fn follow_query_loop(
    context: WsSessionContext,
    store: &StateStore,
    filter: OwnedLogFilter,
    page_limit: u32,
    json_output: bool,
) -> Result<()> {
    use tokio_tungstenite::tungstenite::protocol::Message;

    let (mut writer, mut reader) = open_ws_stream(&context.base_url, &context.session_key).await?;
    subscribe_to_logs(&mut writer).await?;
    let mut stdout = std::io::stdout().lock();
    let watermark = drain_follow_backfill(store, &filter, page_limit, json_output, &mut stdout)?;
    drop(stdout);

    let mut ctrl_c = std::pin::pin!(tokio::signal::ctrl_c());
    loop {
        tokio::select! {
            biased;
            _ = &mut ctrl_c => {
                if let Err(error) = writer.send(Message::Close(None)).await {
                    eprintln!("acps logs query --follow: close send failed: {error}");
                }
                return Ok(());
            }
            frame = reader.next() => {
                let Some(frame) = frame else { return Ok(()); };
                let message = frame.map_err(|source| StackError::ServeIo {
                    source: std::io::Error::other(format!("websocket read failed: {source}")),
                })?;
                let text = match message {
                    Message::Text(text) => text,
                    Message::Binary(_) | Message::Ping(_) | Message::Pong(_) => continue,
                    Message::Close(_) => return Ok(()),
                    Message::Frame(_) => continue,
                };
                let Some(event) = parse_logs_frame(text.as_str())? else { continue };
                if !watermark.is_strictly_after(&event.created_at, &event.id) {
                    continue;
                }
                let log_filter = filter.as_log_filter(0, None, LogOrder::Asc);
                if !log_filter.matches(&event) {
                    continue;
                }
                let mut stdout = std::io::stdout().lock();
                write_event_line(&mut stdout, &event, json_output)?;
            }
        }
    }
}

async fn subscribe_to_logs(writer: &mut WsWriter) -> Result<()> {
    use tokio_tungstenite::tungstenite::protocol::Message;

    // Subscribe only to the `logs` topic. Even when --session is supplied the
    // unified `events` row is what the matcher inspects; sessions.{id} carries
    // a different payload shape (the ACP session update envelope), not the raw
    // event row, so we'd have to special-case it. Operators who want the raw
    // session-update stream should still use `acps logs tail --topic sessions.{id}`.
    let subscribe = serde_json::json!({"type": "subscribe", "topics": [WS_TOPIC_LOGS]});
    writer
        .send(Message::Text(subscribe.to_string().into()))
        .await
        .map_err(|source| StackError::ServeIo {
            source: std::io::Error::other(format!("subscribe failed: {source}")),
        })
}

fn drain_follow_backfill(
    store: &StateStore,
    filter: &OwnedLogFilter,
    page_limit: u32,
    json_output: bool,
    writer: &mut impl Write,
) -> Result<Watermark> {
    if page_limit == 0 {
        return Err(StackError::InvalidParam {
            field: "limit",
            reason: "`--follow` requires a positive page size".to_owned(),
        });
    }

    let high_water = store
        .query_events(filter.as_log_filter(1, None, LogOrder::Desc))?
        .into_iter()
        .next();
    let Some(high_water) = high_water else {
        return Ok(Watermark::default());
    };
    let target = event_watermark(&high_water);
    let mut cursor: Option<String> = None;

    loop {
        let page = store.query_events(filter.as_log_filter(
            page_limit,
            cursor.as_deref(),
            LogOrder::Asc,
        ))?;
        if page.is_empty() {
            return Err(StackError::ServeIo {
                source: std::io::Error::other(
                    "follow backfill ended before reaching the durable high-water event",
                ),
            });
        }

        for event in &page {
            write_event_line(writer, event, json_output)?;
            if event.id == target.id {
                return Ok(target);
            }
        }
        cursor = page.last().map(|event| event.id.clone());
    }
}

type WsWriter = futures::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    tokio_tungstenite::tungstenite::protocol::Message,
>;
type WsReader = futures::stream::SplitStream<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
>;

async fn open_ws_stream(base_url: &str, session_key: &str) -> Result<(WsWriter, WsReader)> {
    use futures::StreamExt;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

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
    Ok(stream.split())
}

/// Reconstruct an `Event` from a `logs`-topic frame so it can be matched by
/// `LogFilter::matches`. The frame shape is set by
/// `EventHub::publish_log_event` in `src/events.rs`: `topic == "logs"`,
/// `payload.kind`, `payload.data.{level,kind,source,message,payload,session_id?}`.
/// Returns `Ok(None)` for non-`logs` frames so the caller can keep reading.
/// Missing required fields are treated as protocol bugs and surface as
/// `ServeIo`; only `payload` (legitimately encoded as Null) and the optional
/// `session_id` get lenient handling.
fn parse_logs_frame(text: &str) -> Result<Option<Event>> {
    let parsed: Value = serde_json::from_str(text).map_err(|source| StackError::ServeIo {
        source: std::io::Error::other(format!("invalid websocket frame JSON: {source}")),
    })?;
    let topic = parsed.get("topic").and_then(Value::as_str).unwrap_or("");
    if topic != WS_TOPIC_LOGS {
        return Ok(None);
    }
    let id = require_string_field(&parsed, "id", "logs frame missing id")?;
    let created_at = require_string_field(&parsed, "createdAt", "logs frame missing createdAt")?;
    let data = parsed
        .get("payload")
        .and_then(|payload| payload.get("data"))
        .ok_or_else(|| StackError::ServeIo {
            source: std::io::Error::other("logs frame missing payload.data"),
        })?;
    let level = require_string_field(data, "level", "logs frame missing data.level")?;
    let kind = require_string_field(data, "kind", "logs frame missing data.kind")?;
    let message = require_string_field(data, "message", "logs frame missing data.message")?;
    let source = require_string_field(data, "source", "logs frame missing data.source")?;
    // `session_id` is only present for events that came from
    // `append_session_event_with_source`; absent means a global write.
    let session_id = data
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    // `payload` is legitimately Null when the source event had an empty
    // payload, so we keep the lenient default here.
    let payload_value = data.get("payload").cloned().unwrap_or(Value::Null);
    let payload_json =
        serde_json::to_string(&payload_value).map_err(|source| StackError::ServeIo {
            source: std::io::Error::other(format!("serialize logs frame payload: {source}")),
        })?;
    Ok(Some(Event {
        id,
        created_at,
        level,
        kind,
        message,
        payload_json,
        source,
        session_id,
    }))
}

fn require_string_field(value: &Value, field: &str, error_message: &'static str) -> Result<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| StackError::ServeIo {
            source: std::io::Error::other(error_message),
        })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follow_backfill_pages_until_high_water_without_skipping_rows() {
        let tempdir = tempfile::tempdir().expect("tempdir should be created");
        let path = tempdir.path().join("state.sqlite");
        let store = StateStore::open(&path).expect("state should open");
        store.migrate().expect("migration should pass");

        for index in 0..5 {
            store
                .append_event("info", "test.follow", &format!("row-{index}"), "{}")
                .expect("seed event");
        }

        let filter = OwnedLogFilter {
            kind: Some("test.follow".to_owned()),
            ..OwnedLogFilter::default()
        };
        let mut output = Vec::new();
        let watermark =
            drain_follow_backfill(&store, &filter, 2, false, &mut output).expect("backfill");
        let rendered = String::from_utf8(output).expect("utf8 output");

        for index in 0..5 {
            assert!(
                rendered.contains(&format!("row-{index}")),
                "missing row-{index}: {rendered}"
            );
        }
        assert!(
            watermark.id.starts_with("evt_"),
            "high-water id should come from the newest durable event"
        );
    }

    #[test]
    fn follow_json_output_is_ndjson_for_backfill_events() {
        let tempdir = tempfile::tempdir().expect("tempdir should be created");
        let path = tempdir.path().join("state.sqlite");
        let store = StateStore::open(&path).expect("state should open");
        store.migrate().expect("migration should pass");

        for index in 0..2 {
            store
                .append_event("info", "test.follow_json", &format!("row-{index}"), "{}")
                .expect("seed event");
        }

        let filter = OwnedLogFilter {
            kind: Some("test.follow_json".to_owned()),
            ..OwnedLogFilter::default()
        };
        let mut output = Vec::new();
        drain_follow_backfill(&store, &filter, 1, true, &mut output).expect("json backfill");
        let rendered = String::from_utf8(output).expect("utf8 output");
        let lines = rendered.lines().collect::<Vec<_>>();

        assert_eq!(lines.len(), 2);
        for line in lines {
            let value: serde_json::Value = serde_json::from_str(line).expect("ndjson object");
            assert_eq!(value["kind"], "test.follow_json");
            assert!(
                value.get("events").is_none(),
                "follow JSON lines must not use the non-follow envelope: {value}"
            );
        }
    }

    #[test]
    fn follow_live_frame_applies_watermark_filter_and_ndjson_rendering() {
        let frame = serde_json::json!({
            "topic": WS_TOPIC_LOGS,
            "id": "evt_20260528120000000000_0002",
            "type": "event",
            "createdAt": "2026-05-28T12:00:00.000000000Z",
            "payload": {
                "kind": "security.rate_limited",
                "data": {
                    "level": "warn",
                    "kind": "security.rate_limited",
                    "source": "api",
                    "message": "limited",
                    "payload": {"session_id": "sess_live", "command_id": "cmd_live"},
                    "session_id": "sess_live"
                }
            }
        });
        let event = parse_logs_frame(&frame.to_string())
            .expect("frame should parse")
            .expect("logs frame should produce event");

        let watermark = Watermark {
            created_at: "2026-05-28T12:00:00.000000000Z".to_owned(),
            id: "evt_20260528120000000000_0001".to_owned(),
        };
        assert!(watermark.is_strictly_after(&event.created_at, &event.id));
        let duplicate_watermark = event_watermark(&event);
        assert!(!duplicate_watermark.is_strictly_after(&event.created_at, &event.id));

        let live_filter = OwnedLogFilter {
            kind: Some("security.".to_owned()),
            source: Some("api".to_owned()),
            session_id: Some("sess_live".to_owned()),
            command_id: Some("cmd_live".to_owned()),
            security_category: Some(SecurityCategory::RateLimit),
            ..OwnedLogFilter::default()
        };
        assert!(
            live_filter
                .as_log_filter(0, None, LogOrder::Asc)
                .matches(&event)
        );

        let mut output = Vec::new();
        write_event_line(&mut output, &event, true).expect("write ndjson");
        let rendered = String::from_utf8(output).expect("utf8 output");
        let lines = rendered.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 1);
        let value: serde_json::Value = serde_json::from_str(lines[0]).expect("json line");
        assert_eq!(value["id"], "evt_20260528120000000000_0002");
        assert_eq!(value["kind"], "security.rate_limited");
        assert!(value.get("events").is_none());
    }
}
