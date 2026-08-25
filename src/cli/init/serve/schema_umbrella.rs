//! Feeds the bootstrap init API's module-private wire DTOs into the published JSON
//! Schema without widening their visibility. Dev-tools only.

use schemars::generate::Contract;
use serde_json::Value;

use super::*;

/// Every top-level body/query the bootstrap init API accepts (deserialize
/// contract).
#[derive(schemars::JsonSchema)]
#[allow(dead_code, clippy::large_enum_variant)]
enum InitRequestTypes {
    StartInitRequest(StartInitRequest),
    EventsQuery(EventsQuery),
    NativeConfigCancelRequest(NativeConfigCancelRequest),
    ClientFrame(ClientFrame),
    SessionInputRequest(SessionInputRequest),
}

/// Every top-level body the bootstrap init API emits (serialize contract).
#[derive(schemars::JsonSchema)]
#[allow(dead_code, clippy::large_enum_variant, clippy::enum_variant_names)]
enum InitResponseTypes {
    StartInitResponse(StartInitResponse),
    SimpleSessionResponse(SimpleSessionResponse),
    InitEventsResponse(InitEventsResponse),
    InitStatusResponse(InitStatusResponse),
    InputAcceptedResponse(InputAcceptedResponse),
}

/// The init request DTOs' definitions, re-keyed under the `request` namespace.
pub(crate) fn init_request_defs() -> Value {
    crate::schema_export::pass_defs::<InitRequestTypes>(Contract::Deserialize, "request")
}

/// The init response DTOs' definitions, re-keyed under the `response` namespace.
pub(crate) fn init_response_defs() -> Value {
    crate::schema_export::pass_defs::<InitResponseTypes>(Contract::Serialize, "response")
}
