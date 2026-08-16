use super::*;

#[derive(Debug, Serialize)]
pub(super) struct StartInitResponse {
    pub(super) session_id: String,
    pub(super) status: String,
}

#[derive(Debug, Serialize)]
pub(super) struct SimpleSessionResponse {
    pub(super) session_id: String,
    pub(super) status: String,
}

#[derive(Debug, Serialize)]
pub(super) struct InitEventsResponse {
    pub(super) session_id: String,
    pub(super) events: Vec<Value>,
}

#[derive(Debug, Serialize)]
pub(super) struct InitStatusResponse {
    pub(super) session_id: String,
    pub(super) status: String,
    /// Category snapshot, identical in shape to the `state` frame and to the
    /// `state` field of `hello`, so a REST poller and a socket client read the
    /// same thing.
    pub(super) state: StateSnapshot,
    pub(super) last_seq: u64,
    pub(super) pending_input: Option<PublicInputRequest>,
    pub(super) recent_events: Vec<Value>,
    pub(super) result_available: bool,
    pub(super) error: Option<PublicError>,
    pub(super) last_activity_age_secs: u64,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct PublicError {
    pub(super) code: String,
    pub(super) message: String,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct PublicInputRequest {
    pub(super) request_id: String,
    /// Machine-readable prompt identity, from `HostedPromptKind::as_str`. Field
    /// order here is the wire order; `kind` sits beside `request_id` so a
    /// client can route on it before parsing the rest.
    pub(super) kind: &'static str,
    pub(super) style: String,
    pub(super) prompt: String,
    pub(super) required: bool,
    pub(super) default: Option<bool>,
    pub(super) options: Vec<PublicInputOption>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) inspection: Option<NativeConfigInspection>,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct PublicInputOption {
    pub(super) index: usize,
    /// Stable option id an answer may address by `{"value": "<id>"}`; unlike
    /// `label` it survives display rewording.
    pub(super) value: String,
    pub(super) label: String,
    pub(super) hint: String,
}
