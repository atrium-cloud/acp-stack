//! Opt-in trace of ACP JSON-RPC traffic: when `[logging].acp_trace` is on, every frame on an
//! agent connection logs one redacted, bounded line under the `acp_trace` target.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use futures::{AsyncBufReadExt, AsyncWriteExt, StreamExt};
use serde_json::Value;
use tokio::process::{ChildStdin, ChildStdout};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::redaction::{bounded, redact_json, redact_text};

// CONSTANTS

/// Log target of the trace lines; the daemon log filter admits it at INFO.
pub const ACP_TRACE_TARGET: &str = "acp_trace";

/// Cap on a traced frame's rendered result, error, or unparsed body.
const ACP_TRACE_FRAME_MAX_BYTES: usize = 4096;

/// Cap on a traced frame's id, method, and session id, which the peer chooses freely.
const ACP_TRACE_LABEL_MAX_BYTES: usize = 256;

static ACP_TRACE_ENABLED: AtomicBool = AtomicBool::new(false);

/// Turn the trace on or off for every agent connection in the process.
pub fn set_enabled(enabled: bool) {
    ACP_TRACE_ENABLED.store(enabled, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ACP_TRACE_ENABLED.load(Ordering::Relaxed)
}

/// Ids stamped on every trace line of one agent connection.
#[derive(Debug, Clone)]
pub struct TraceLabels {
    pub agent_id: String,
    pub target_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Direction {
    ClientToAgent,
    AgentToClient,
}

impl Direction {
    fn as_str(self) -> &'static str {
        match self {
            Self::ClientToAgent => "client->agent",
            Self::AgentToClient => "agent->client",
        }
    }

    fn counterpart(self) -> Self {
        match self {
            Self::ClientToAgent => Self::AgentToClient,
            Self::AgentToClient => Self::ClientToAgent,
        }
    }
}

/// One traced frame, as logged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TraceFrame {
    pub(crate) direction: Direction,
    /// `request`, `notification`, `response`, `unrecognized` (JSON outside JSON-RPC's
    /// shapes), or `unparsed` (not JSON).
    pub(crate) kind: &'static str,
    pub(crate) method: Option<String>,
    pub(crate) id: Option<String>,
    pub(crate) session_id: Option<String>,
    /// `ok` or `error` on a response.
    pub(crate) outcome: Option<&'static str>,
    pub(crate) elapsed_ms: Option<u64>,
    /// An error from either side, the agent's result to a client request, or an unparsed line,
    /// redacted and bounded.
    pub(crate) payload: Option<String>,
}

struct InFlight {
    method: String,
    session_id: Option<String>,
    started: Instant,
}

/// Per-connection trace state: the requests each side has sent and not yet seen answered.
pub(crate) struct TraceTap {
    labels: TraceLabels,
    was_enabled: AtomicBool,
    sent_by_client: Mutex<HashMap<String, InFlight>>,
    sent_by_agent: Mutex<HashMap<String, InFlight>>,
}

impl TraceTap {
    pub(crate) fn new(labels: TraceLabels) -> Self {
        Self {
            labels,
            was_enabled: AtomicBool::new(false),
            sent_by_client: Mutex::new(HashMap::new()),
            sent_by_agent: Mutex::new(HashMap::new()),
        }
    }

    /// Classify `line` if the trace is on. Costs two atomic loads when it is off; turning it
    /// off drops the in-flight requests, whose answers would otherwise match stale entries later.
    fn trace(&self, direction: Direction, line: &str) -> Option<TraceFrame> {
        if !enabled() {
            if self.was_enabled.load(Ordering::Relaxed) {
                self.was_enabled.store(false, Ordering::Relaxed);
                self.clear();
            }
            return None;
        }
        self.was_enabled.store(true, Ordering::Relaxed);
        Some(self.observe(direction, line))
    }

    fn emit(&self, frame: &TraceFrame) {
        tracing::info!(
            target: ACP_TRACE_TARGET,
            agent_id = %self.labels.agent_id,
            target_id = self.labels.target_id.as_deref().unwrap_or_default(),
            direction = frame.direction.as_str(),
            kind = frame.kind,
            method = frame.method.as_deref().unwrap_or_default(),
            id = frame.id.as_deref().unwrap_or_default(),
            session_id = frame.session_id.as_deref().unwrap_or_default(),
            outcome = frame.outcome.unwrap_or_default(),
            elapsed_ms = frame.elapsed_ms,
            payload = %frame.payload.as_deref().unwrap_or_default(),
            "acp frame"
        );
    }

    /// Drop the in-flight entry of a request that never reached the peer.
    pub(crate) fn forget(&self, frame: &TraceFrame) {
        if let ("request", Some(id)) = (frame.kind, &frame.id) {
            self.sent_by(frame.direction).remove(id);
        }
    }

    fn clear(&self) {
        self.sent_by_client
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        self.sent_by_agent
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }

    /// Classify one JSON-RPC line and match a response to the request it answers.
    pub(crate) fn observe(&self, direction: Direction, line: &str) -> TraceFrame {
        let mut frame = TraceFrame {
            direction,
            kind: "unparsed",
            method: None,
            id: None,
            session_id: None,
            outcome: None,
            elapsed_ms: None,
            payload: None,
        };
        let mut message = match serde_json::from_str::<Value>(line) {
            Ok(Value::Object(message)) => message,
            Ok(_) => {
                frame.kind = "unrecognized";
                return frame;
            }
            Err(_) => {
                frame.payload = Some(render_text(line));
                return frame;
            }
        };
        // A null id marks a notification, or a response to a request whose id was unreadable.
        frame.id = message
            .get("id")
            .filter(|id| !id.is_null())
            .map(|id| match id {
                Value::String(id) => bounded_label(id),
                id => bounded_label(&id.to_string()),
            });
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .map(bounded_label);
        let is_response = message.contains_key("result") || message.contains_key("error");
        match (method, &frame.id) {
            (Some(method), Some(id)) => {
                frame.kind = "request";
                frame.session_id = params_session_id(&message);
                self.sent_by(direction).insert(
                    id.clone(),
                    InFlight {
                        method: method.clone(),
                        session_id: frame.session_id.clone(),
                        started: Instant::now(),
                    },
                );
                frame.method = Some(method);
            }
            (Some(method), None) => {
                frame.kind = "notification";
                frame.session_id = params_session_id(&message);
                frame.method = Some(method);
            }
            (None, id) if is_response => {
                frame.kind = "response";
                let answered = id
                    .as_ref()
                    .and_then(|id| self.sent_by(direction.counterpart()).remove(id));
                let (outcome, body) = match message.remove("error") {
                    Some(error) => ("error", error),
                    None => ("ok", message.remove("result").unwrap_or(Value::Null)),
                };
                frame.outcome = Some(outcome);
                frame.session_id = answered
                    .as_ref()
                    .and_then(|request| request.session_id.clone())
                    .or_else(|| {
                        body.get("sessionId")
                            .and_then(Value::as_str)
                            .map(bounded_label)
                    });
                if let Some(request) = answered {
                    frame.method = Some(request.method);
                    frame.elapsed_ms = Some(
                        u64::try_from(request.started.elapsed().as_millis()).unwrap_or(u64::MAX),
                    );
                }
                // The client's results carry workspace content (file reads, terminal output),
                // so only its errors are rendered.
                if outcome == "error" || direction == Direction::AgentToClient {
                    frame.payload = Some(render_json(body));
                }
            }
            (None, _) => frame.kind = "unrecognized",
        }
        frame
    }

    fn sent_by(
        &self,
        direction: Direction,
    ) -> std::sync::MutexGuard<'_, HashMap<String, InFlight>> {
        match direction {
            Direction::ClientToAgent => &self.sent_by_client,
            Direction::AgentToClient => &self.sent_by_agent,
        }
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
    }
}

fn params_session_id(message: &serde_json::Map<String, Value>) -> Option<String> {
    message
        .get("params")
        .and_then(|params| params.get("sessionId"))
        .and_then(Value::as_str)
        .map(bounded_label)
}

fn bounded_label(label: &str) -> String {
    bounded(label, ACP_TRACE_LABEL_MAX_BYTES).into_owned()
}

fn render_json(mut value: Value) -> String {
    redact_json(&mut value);
    bounded(&value.to_string(), ACP_TRACE_FRAME_MAX_BYTES).into_owned()
}

fn render_text(text: &str) -> String {
    bounded(&redact_text(text), ACP_TRACE_FRAME_MAX_BYTES).into_owned()
}

/// The agent's stdio as an ACP line transport with the trace tap on both directions. Line
/// splitting and writing match `agent_client_protocol::ByteStreams`.
pub fn traced_lines(
    stdin: ChildStdin,
    stdout: ChildStdout,
    labels: TraceLabels,
) -> agent_client_protocol::Lines<
    impl futures::Sink<String, Error = std::io::Error> + Send + 'static,
    impl futures::Stream<Item = std::io::Result<String>> + Send + 'static,
> {
    let tap = Arc::new(TraceTap::new(labels));
    let incoming_tap = Arc::clone(&tap);
    let incoming = Box::pin(
        futures::io::BufReader::new(stdout.compat())
            .lines()
            .inspect(move |line| {
                if let Ok(line) = line
                    && let Some(frame) = incoming_tap.trace(Direction::AgentToClient, line)
                {
                    incoming_tap.emit(&frame);
                }
            }),
    );
    let outgoing = futures::sink::unfold(
        (Box::pin(stdin.compat_write()), tap),
        async move |(mut writer, tap), line: String| {
            // Classified before the write so a fast answer finds its request, but logged only
            // once the line has reached the agent.
            let frame = tap.trace(Direction::ClientToAgent, &line);
            let mut bytes = line.into_bytes();
            bytes.push(b'\n');
            let written = write_line(&mut writer, &bytes).await;
            if let Some(frame) = frame {
                match &written {
                    Ok(()) => tap.emit(&frame),
                    Err(_) => tap.forget(&frame),
                }
            }
            written?;
            Ok::<_, std::io::Error>((writer, tap))
        },
    );
    agent_client_protocol::Lines::new(outgoing, incoming)
}

async fn write_line(
    writer: &mut (impl futures::AsyncWrite + Unpin),
    bytes: &[u8],
) -> std::io::Result<()> {
    writer.write_all(bytes).await?;
    writer.flush().await
}

#[cfg(test)]
mod tests;
