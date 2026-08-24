use super::*;

use serde_json::value::RawValue;

/// Substitute wire error for a frame whose payload could not be encoded. Both
/// halves MUST stay plain ASCII with no JSON metacharacters, so
/// `encode_failure_frame` can splice them without a fallible serializer.
pub(super) const FRAME_ENCODE_FAILED_CODE: &str = "init.frame_encode_failed";
pub(super) const FRAME_ENCODE_FAILED_MESSAGE: &str = "init frame payload could not be encoded";

#[derive(Debug, thiserror::Error)]
pub(super) enum FrameError {
    #[error("init frame payload could not be encoded: {source}")]
    Encode { source: serde_json::Error },
    #[error("stored init result is not valid JSON")]
    ResultNotJson,
}

/// Every seq-bearing session event.
pub(super) enum ServerEvent {
    Progress {
        message: String,
    },
    /// Boxed: `PublicInputRequest` dwarfs every other variant.
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
    /// One raw init state signal, prebuilt into its wire payload.
    Signal(Map<String, Value>),
}

impl ServerEvent {
    pub(super) fn type_str(&self) -> &'static str {
        match self {
            ServerEvent::Progress { .. } => "progress",
            ServerEvent::InputRequired { .. } => "input_required",
            ServerEvent::InputAccepted { .. } => "input_accepted",
            ServerEvent::ResultReady => "result_ready",
            ServerEvent::ResultAcked => "result_acked",
            ServerEvent::Canceled { .. } => "cancelled",
            ServerEvent::Error { .. } => "error",
            ServerEvent::ErrorAcked => "error_acked",
            ServerEvent::ErrorExpired { .. } => "error_expired",
            ServerEvent::Signal(_) => "signal",
        }
    }

    /// `InputRequired` is the only variant carrying a struct, and therefore
    /// the only one whose encode can fail.
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
            // The signal's own fields become envelope keys rather than nesting
            // the payload one level down.
            ServerEvent::Signal(map) => payload = map,
        }
        Ok(payload)
    }
}

/// The `error` event payload. Scalar-only, so reporting a failure can never
/// fail the same way the frame it reports on did.
pub(super) fn error_payload(code: &str, message: String) -> Map<String, Value> {
    let mut payload = Map::new();
    payload.insert("code".to_owned(), Value::String(code.to_owned()));
    payload.insert("message".to_owned(), Value::String(message));
    payload
}

/// Wrap an event payload in the `type`/`seq`/`session_id` envelope. Assembled
/// through a `BTreeMap` because `serde_json::Map` is insertion-ordered in this
/// build (`agent-client-protocol` enables `preserve_order`) and clients expect
/// sorted keys.
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
        /// The whole signal stream so far, so a late client can fold it to the
        /// current view even after the history cap evicted early events.
        signals: &'a [Value],
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
    /// Serialized through derived structs rather than a `Map` so field order
    /// stays declaration order regardless of `preserve_order` downstream.
    pub(super) fn to_json(&self) -> std::result::Result<String, FrameError> {
        match self {
            ServerFrame::Hello {
                session_id,
                status,
                signals,
                last_seq,
                pending_input,
                result_available,
                error,
            } => encode(&HelloBody {
                frame_type: "hello",
                session_id,
                status,
                signals,
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
/// than dropping the client's only notification for this transition.
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

/// The `result` frame; `payload` borrows the stored result JSON verbatim.
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
    signals: &'a [Value],
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
