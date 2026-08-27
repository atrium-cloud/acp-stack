use super::*;

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct StartInitResponse {
    pub(super) session_id: String,
    /// Always `running`: the session is published only after it is created in
    /// that state, so no other value is observable here.
    #[schemars(extend("const" = "running"))]
    pub(super) status: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct SimpleSessionResponse {
    pub(super) session_id: String,
    /// Session status after the request was applied. `cancelled`, `closed`, and
    /// `errored` are terminal, as is `completed_awaiting_ack` until the result
    /// is acknowledged.
    #[schemars(extend("enum" = ["running", "waiting_for_input", "completed_awaiting_ack", "errored", "cancelled", "closed"]))]
    pub(super) status: String,
}

/// REST twin of the `input_accepted` server event: the socket carries the ack
/// as an event frame, `POST /v1/init/sessions/{id}/input` returns it as the
/// success data.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct InputAcceptedResponse {
    pub(super) request_id: String,
}

/// `POST /v1/init/credential` result: the flat secrets persisted plus the
/// managed apply outcome, mirroring the admin-tier apply response.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct DepositCredentialResponse {
    pub(super) secrets_written: usize,
    pub(super) applied_revision: i64,
    #[schemars(extend("enum" = ["applied", "cleared", "noop"]))]
    pub(super) outcome: &'static str,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct InitEventsResponse {
    pub(super) session_id: String,
    pub(super) events: Vec<Value>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct InitStatusResponse {
    pub(super) session_id: String,
    /// Current session status. `waiting_for_input` is the steady state while a
    /// prompt is pending; `cancelled`, `closed`, and `errored` are terminal, as
    /// is `completed_awaiting_ack` until the result is acknowledged.
    #[schemars(extend("enum" = ["running", "waiting_for_input", "completed_awaiting_ack", "errored", "cancelled", "closed"]))]
    pub(super) status: String,
    /// The full ordered signal stream, identical to the `signals` field of
    /// `hello`, so a REST poller and a socket client fold the same input into
    /// the same category view.
    pub(super) signals: Vec<Value>,
    pub(super) last_seq: u64,
    pub(super) pending_input: Option<PublicInputRequest>,
    pub(super) recent_events: Vec<Value>,
    pub(super) result_available: bool,
    pub(super) error: Option<PublicError>,
    pub(super) last_activity_age_secs: u64,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub(super) struct PublicError {
    pub(super) code: String,
    pub(super) message: String,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub(super) struct PublicInputRequest {
    pub(super) request_id: String,
    /// Machine-readable prompt identity, from `HostedPromptKind::as_str`. Field
    /// order here is the wire order; `kind` sits beside `request_id` so a
    /// client can route on it before parsing the rest.
    pub(super) kind: &'static str,
    /// Rendering hint. Unlike `kind`, this set is closed: a client may switch
    /// on it exhaustively.
    #[schemars(extend("enum" = ["select", "searchable_select", "confirm", "text", "password", "native_config_review"]))]
    pub(super) style: String,
    pub(super) prompt: String,
    pub(super) required: bool,
    pub(super) default: Option<bool>,
    pub(super) options: Vec<PublicInputOption>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) inspection: Option<NativeConfigInspection>,
    /// Present only for `kind: "config_option"`; this is the exact advertised
    /// option the answer's `config_id` and typed value are validated against.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) config_option:
        Option<crate::runtime::agent::config_options::SessionConfigOptionSnapshot>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub(super) struct PublicInputOption {
    pub(super) index: usize,
    /// Stable option id an answer may address by `{"value": "<id>"}`; unlike
    /// `label` it survives display rewording.
    pub(super) value: String,
    pub(super) label: String,
    pub(super) hint: String,
}
