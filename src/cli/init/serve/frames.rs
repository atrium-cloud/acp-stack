use super::*;

use serde_json::value::RawValue;

/// Substitute wire error for a frame whose payload could not be encoded. Both
/// halves are plain ASCII with no JSON metacharacters, which is what lets
/// `encode_failure_frame` splice them without a serializer that could fail the
/// same way the original frame did.
pub(super) const FRAME_ENCODE_FAILED_CODE: &str = "init.frame_encode_failed";
pub(super) const FRAME_ENCODE_FAILED_MESSAGE: &str = "init frame payload could not be encoded";

#[derive(Debug, thiserror::Error)]
pub(super) enum FrameError {
    #[error("init frame payload could not be encoded: {source}")]
    Encode { source: serde_json::Error },
    #[error("stored init result is not valid JSON")]
    ResultNotJson,
}

/// Every seq-bearing session event. Each variant owns its payload so a caller
/// can hand one over while holding the session lock without borrowing across
/// the record.
pub(super) enum ServerEvent {
    Progress {
        message: String,
    },
    /// Boxed because `PublicInputRequest` is an order of magnitude larger than
    /// every other variant, and the enum is passed by value on every event.
    InputRequired {
        input: Box<PublicInputRequest>,
    },
    InputAccepted {
        request_id: String,
    },
    ResultReady,
    ResultAcked,
    Canceled {
        reason: String,
    },
    Error {
        code: String,
        message: String,
    },
    ErrorAcked,
    ErrorExpired {
        reason: String,
    },
    State(StateSnapshot),
}

impl ServerEvent {
    pub(super) fn type_str(&self) -> &'static str {
        match self {
            ServerEvent::Progress { .. } => "progress",
            ServerEvent::InputRequired { .. } => "input_required",
            ServerEvent::InputAccepted { .. } => "input_accepted",
            ServerEvent::ResultReady => "result_ready",
            ServerEvent::ResultAcked => "result_acked",
            ServerEvent::Canceled { .. } => "canceled",
            ServerEvent::Error { .. } => "error",
            ServerEvent::ErrorAcked => "error_acked",
            ServerEvent::ErrorExpired { .. } => "error_expired",
            ServerEvent::State(_) => "state",
        }
    }

    /// Scalar variants build their map by hand so no `Serialize` impl sits
    /// between the caller and the wire; `InputRequired` and `State` are the
    /// only variants carrying a struct, and therefore the only ones that can
    /// fail.
    pub(super) fn payload(self) -> std::result::Result<Map<String, Value>, FrameError> {
        let mut payload = Map::new();
        match self {
            ServerEvent::Progress { message } => {
                payload.insert("message".to_owned(), Value::String(message));
            }
            ServerEvent::InputRequired { input } => {
                let input =
                    serde_json::to_value(input).map_err(|source| FrameError::Encode { source })?;
                payload.insert("input".to_owned(), input);
            }
            ServerEvent::InputAccepted { request_id } => {
                payload.insert("request_id".to_owned(), Value::String(request_id));
            }
            ServerEvent::ResultReady => {
                payload.insert(
                    "status".to_owned(),
                    Value::String("completed_awaiting_ack".to_owned()),
                );
            }
            ServerEvent::ResultAcked => {
                payload.insert("status".to_owned(), Value::String("closed".to_owned()));
            }
            ServerEvent::Canceled { reason } => {
                payload.insert("reason".to_owned(), Value::String(reason));
            }
            ServerEvent::Error { code, message } => payload = error_payload(&code, message),
            ServerEvent::ErrorAcked => {
                payload.insert("status".to_owned(), Value::String("errored".to_owned()));
            }
            ServerEvent::ErrorExpired { reason } => {
                payload.insert("reason".to_owned(), Value::String(reason));
            }
            // The snapshot's own fields become envelope keys, so the event
            // reads `{"categories":…,"current_step":…,"seq":…}` rather than
            // nesting the snapshot one level down.
            ServerEvent::State(snapshot) => payload = snapshot.payload()?,
        }
        Ok(payload)
    }
}

/// The `error` event payload. Scalar-only and therefore infallible, which is
/// what lets a session park on a failure without a fallible step that could
/// fail the same way the frame it is reporting on did.
pub(super) fn error_payload(code: &str, message: String) -> Map<String, Value> {
    let mut payload = Map::new();
    payload.insert("code".to_owned(), Value::String(code.to_owned()));
    payload.insert("message".to_owned(), Value::String(message));
    payload
}

/// Wrap an event payload in the `type`/`seq`/`session_id` envelope. Assembly
/// runs through a `BTreeMap` so the recorded frame keeps the alphabetically
/// sorted key order clients have always seen — `serde_json::Map` is insertion
/// ordered in this build (`agent-client-protocol` turns on `preserve_order`)
/// and cannot be relied on to sort. A payload key colliding with an envelope
/// key still wins, as it did when the envelope was assembled inline.
pub(super) fn envelope(
    event_type: &str,
    seq: u64,
    session_id: &str,
    payload: Map<String, Value>,
) -> Value {
    let mut object = BTreeMap::new();
    object.insert("type".to_owned(), Value::String(event_type.to_owned()));
    object.insert("seq".to_owned(), Value::Number(seq.into()));
    object.insert(
        "session_id".to_owned(),
        Value::String(session_id.to_owned()),
    );
    for (key, value) in payload {
        object.insert(key, value);
    }
    Value::Object(object.into_iter().collect())
}

/// Frames sent outside the seq-bearing event history: connection handshakes,
/// close acknowledgements, replays, and protocol-level rejections.
pub(super) enum ServerFrame<'a> {
    Hello {
        session_id: &'a str,
        status: &'a str,
        /// Full category snapshot, so a client that connects late — or after
        /// the history cap evicted early `state` events — starts current
        /// without replaying anything.
        state: &'a StateSnapshot,
        last_seq: u64,
        pending_input: Option<&'a PublicInputRequest>,
        result_available: bool,
        error: Option<&'a PublicError>,
    },
    AckAccepted {
        session_id: &'a str,
    },
    ErrorAckedClose {
        session_id: &'a str,
    },
    ErrorReplay {
        session_id: &'a str,
        code: &'a str,
        message: &'a str,
    },
    /// Protocol-level rejection, deliberately without a `session_id`: it can
    /// fire on a frame that never named a valid session.
    ProtocolError {
        code: &'a str,
        message: &'a str,
    },
}

impl ServerFrame<'_> {
    /// Serialized through derived structs rather than a `Map`, so field order
    /// is the declaration order these frames have always been written in
    /// regardless of whether `preserve_order` is enabled downstream.
    pub(super) fn to_json(&self) -> std::result::Result<String, FrameError> {
        match self {
            ServerFrame::Hello {
                session_id,
                status,
                state,
                last_seq,
                pending_input,
                result_available,
                error,
            } => encode(&HelloBody {
                frame_type: "hello",
                session_id,
                status,
                state,
                last_seq: *last_seq,
                pending_input: *pending_input,
                result_available: *result_available,
                error: *error,
            }),
            ServerFrame::AckAccepted { session_id } => encode(&SessionBody {
                frame_type: "ack_accepted",
                session_id,
            }),
            ServerFrame::ErrorAckedClose { session_id } => encode(&SessionBody {
                frame_type: "error_acked",
                session_id,
            }),
            ServerFrame::ErrorReplay {
                session_id,
                code,
                message,
            } => encode(&ErrorReplayBody {
                frame_type: "error",
                session_id,
                code,
                message,
            }),
            ServerFrame::ProtocolError { code, message } => encode(&ProtocolErrorBody {
                frame_type: "error",
                code,
                message,
            }),
        }
    }
}

/// Serialize a seq-less frame, degrading to the encode-failure frame rather
/// than dropping the client's only notification for this transition. Only
/// `Hello` carries a struct that can fail; the rest are borrowed scalars.
pub(super) fn frame_json(frame: ServerFrame<'_>) -> String {
    frame.to_json().unwrap_or_else(|error| {
        tracing::warn!(
            error = %error,
            "hosted init frame could not be encoded; sending an encode-failure frame instead"
        );
        encode_failure_frame()
    })
}

/// The one frame with no fallible step on the way out, used when encoding a
/// real frame failed.
pub(super) fn encode_failure_frame() -> String {
    format!(
        r#"{{"type":"error","code":"{FRAME_ENCODE_FAILED_CODE}","message":"{FRAME_ENCODE_FAILED_MESSAGE}"}}"#
    )
}

/// The `result` frame. `payload` borrows the stored result JSON verbatim, so
/// the plaintext handoff is neither re-encoded nor copied into a second
/// buffer on its way to the socket.
#[derive(Serialize)]
pub(super) struct ResultFrame<'a> {
    #[serde(rename = "type")]
    frame_type: &'static str,
    session_id: &'a str,
    payload: &'a RawValue,
}

impl<'a> ResultFrame<'a> {
    pub(super) fn new(
        session_id: &'a str,
        result_json: &'a str,
    ) -> std::result::Result<Self, FrameError> {
        let payload = serde_json::from_str::<&RawValue>(result_json)
            .map_err(|_| FrameError::ResultNotJson)?;
        Ok(Self {
            frame_type: "result",
            session_id,
            payload,
        })
    }

    pub(super) fn to_json(&self) -> std::result::Result<String, FrameError> {
        encode(self)
    }
}

fn encode<T: Serialize + ?Sized>(body: &T) -> std::result::Result<String, FrameError> {
    serde_json::to_string(body).map_err(|source| FrameError::Encode { source })
}

#[derive(Serialize)]
struct HelloBody<'a> {
    #[serde(rename = "type")]
    frame_type: &'static str,
    session_id: &'a str,
    status: &'a str,
    state: &'a StateSnapshot,
    last_seq: u64,
    pending_input: Option<&'a PublicInputRequest>,
    result_available: bool,
    error: Option<&'a PublicError>,
}

#[derive(Serialize)]
struct SessionBody<'a> {
    #[serde(rename = "type")]
    frame_type: &'static str,
    session_id: &'a str,
}

#[derive(Serialize)]
struct ErrorReplayBody<'a> {
    #[serde(rename = "type")]
    frame_type: &'static str,
    session_id: &'a str,
    code: &'a str,
    message: &'a str,
}

#[derive(Serialize)]
struct ProtocolErrorBody<'a> {
    #[serde(rename = "type")]
    frame_type: &'static str,
    code: &'a str,
    message: &'a str,
}
