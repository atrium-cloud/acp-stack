//! Feeds the bootstrap init API's wire DTOs into the published JSON Schema
//! without widening the visibility of the (deliberately module-private) init
//! DTOs. The umbrella enums stay private here; only the two `pub(crate)`
//! functions escape (through a `#[cfg]`-gated re-export chain to `crate::cli`),
//! and they return plain `serde_json::Value` so no private type leaks. Each
//! umbrella is a schema-generation root that is discarded, so its members land
//! in `$defs` while the umbrella itself does not. Dev-tools only.

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
}

/// Every top-level body the bootstrap init API emits (serialize contract).
#[derive(schemars::JsonSchema)]
#[allow(dead_code, clippy::large_enum_variant, clippy::enum_variant_names)]
enum InitResponseTypes {
    StartInitResponse(StartInitResponse),
    SimpleSessionResponse(SimpleSessionResponse),
    InitEventsResponse(InitEventsResponse),
    InitStatusResponse(InitStatusResponse),
}

/// The init request DTOs' definitions, re-keyed under the `request` namespace.
pub(crate) fn init_request_defs() -> Value {
    crate::schema_export::pass_defs::<InitRequestTypes>(Contract::Deserialize, "request")
}

/// The init response DTOs' definitions, re-keyed under the `response` namespace.
pub(crate) fn init_response_defs() -> Value {
    crate::schema_export::pass_defs::<InitResponseTypes>(Contract::Serialize, "response")
}
