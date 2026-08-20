//! The Serialize-contract umbrella: every top-level body the `/v1` API emits,
//! plus the shared response envelope. Registered here so `acps_schema` pulls
//! them (and their transitive field types) into `#/$defs/response/*`. The init
//! API's responses are contributed separately (see `crate::cli::init_response_defs`).
//!
//! The success envelope `{ok, data}` is registered once as
//! [`ApiSuccess<serde_json::Value>`]; per-endpoint responses are the bare `data`
//! payload types. See `docs/specs/api/api.md` for the enveloping rule.
//!
//! This is a hand-maintained registration point: a new response body must be
//! added here. The `schema_covers_every_handler_wire_type` coverage test
//! (see `super::coverage`) guards against a handler payload with no schema entry.

/// Umbrella over every response payload. Never serialized itself — only its
/// derived `JsonSchema` matters, which forces each variant's type into `$defs`.
/// The variants have wildly different sizes and a shared `Response` postfix,
/// but the enum is never constructed, so those clippy lints do not apply.
#[derive(schemars::JsonSchema)]
#[allow(dead_code, clippy::large_enum_variant, clippy::enum_variant_names)]
pub(super) enum AcpsResponseTypes {
    // Shared envelope.
    ApiSuccess(crate::envelope::ApiSuccess<serde_json::Value>),
    ApiErrorEnvelope(crate::envelope::ApiErrorEnvelope),
    ApiError(crate::envelope::ApiError),
    // Per-endpoint response payloads.
    AgentCapabilitiesResponseBody(crate::api::routes::agent::AgentCapabilitiesResponseBody),
    AgentInstallResponse(crate::api::routes::agent::AgentInstallResponse),
    AgentRestartBlockersResponse(
        crate::api::routes::agent::lifecycle::AgentRestartBlockersResponse,
    ),
    AgentRestartResultResponse(crate::api::routes::agent::lifecycle::AgentRestartResultResponse),
    AgentStartResponse(crate::api::routes::agent::lifecycle::AgentStartResponse),
    AgentStopResponse(crate::api::routes::agent::lifecycle::AgentStopResponse),
    AgentSwitchResponse(crate::api::routes::agent::switch::AgentSwitchResponse),
    AgentUpdateReport(crate::runtime::install::agent_updater::AgentUpdateReport),
    AgentUpdateStatusResponse(crate::api::routes::agent::update::AgentUpdateStatusResponse),
    ApplyResponse(crate::extensions::managed_state::ApplyResponse),
    ArrayStatusResponse(crate::api::routes::agent::ArrayStatusResponse),
    CommandOutputResponse(crate::api::routes::commands::CommandOutputResponse),
    CommandResponse(crate::api::routes::commands::CommandResponse),
    CommandsListResponse(crate::api::routes::commands::CommandsListResponse),
    ConfigExportResponse(crate::api::routes::config::ConfigExportResponse),
    ConfigValidateResponse(crate::api::routes::config::ConfigValidateResponse),
    DepsApplyResponse(crate::api::routes::deps::DepsApplyResponse),
    DepsReport(crate::runtime::dependencies::deps::DepsReport),
    DisconnectResponse(crate::api::routes::ws::DisconnectResponse),
    FileDeleteResponse(crate::api::routes::workspace::FileDeleteResponse),
    FileMutationResponse(crate::api::routes::workspace::FileMutationResponse),
    FileUploadResponse(crate::api::routes::workspace::FileUploadResponse),
    FilesContentResponse(crate::api::routes::workspace::FilesContentResponse),
    FilesListResponse(crate::api::routes::workspace::FilesListResponse),
    HealthLiveResponse(crate::api::routes::status::HealthLiveResponse),
    InstallerRunsResponse(crate::api::routes::installer::InstallerRunsResponse),
    LocalSessionAccessResponse(crate::api::routes::auth::LocalSessionAccessResponse),
    LogsCommandsResponse(crate::api::routes::logs::LogsCommandsResponse),
    LogsEventsResponse(crate::api::routes::logs::LogsEventsResponse),
    LogsSecurityResponse(crate::api::routes::logs::LogsSecurityResponse),
    LogsSessionsResponse(crate::api::routes::logs::LogsSessionsResponse),
    MetricsSummaryResponse(crate::api::routes::metrics::MetricsSummaryResponse),
    ModelsResponse(crate::api::routes::providers::ModelsResponse),
    NativeConfigInspection(crate::runtime::agent::native_config_import::NativeConfigInspection),
    NativeConfigOperation(crate::runtime::agent::native_config_import::NativeConfigOperation),
    PermissionDecisionView(crate::runtime::mediation::permissions::PermissionDecisionView),
    PermissionRequestView(crate::runtime::mediation::permissions::PermissionRequestView),
    PermissionsListResponse(crate::api::routes::permissions::PermissionsListResponse),
    PromptStatusResponse(crate::api::routes::sessions::PromptStatusResponse),
    PromptSubmitResponse(crate::api::routes::sessions::prompts::PromptSubmitResponse),
    ProvidersResponse(crate::api::routes::providers::ProvidersResponse),
    RegenerateSessionKeyResponse(crate::api::routes::auth::RegenerateSessionKeyResponse),
    SecretsDeleteResponse(crate::api::routes::config::SecretsDeleteResponse),
    SecretsListResponse(crate::api::routes::config::SecretsListResponse),
    SecretsSetResponse(crate::api::routes::config::SecretsSetResponse),
    SecurityCheckResponse(crate::api::routes::security::SecurityCheckResponse),
    SecurityHistoryResponse(crate::api::routes::security::SecurityHistoryResponse),
    SecurityHistoryShowResponse(crate::api::routes::security::SecurityHistoryShowResponse),
    SessionChangesSnapshot(crate::runtime::agent::session_changes::SessionChangesSnapshot),
    SessionResponse(crate::api::routes::sessions::SessionResponse),
    SessionSnapshotResponse(crate::api::routes::sessions::events::SessionSnapshotResponse),
    SessionsCancelResponse(crate::api::routes::sessions::teardown::SessionsCancelResponse),
    SessionsDeleteResponse(crate::api::routes::sessions::teardown::SessionsDeleteResponse),
    SessionsEventsResponse(crate::api::routes::sessions::events::SessionsEventsResponse),
    SessionsListResponse(crate::api::routes::sessions::list::SessionsListResponse),
    SessionsStatusResponse(crate::api::routes::sessions::status::SessionsStatusResponse),
    SkillSourceAddResponse(crate::api::routes::skills::SkillSourceAddResponse),
    SkillSourceGetResponse(crate::api::routes::skills::SkillSourceGetResponse),
    SkillSourceRemoveResponse(crate::api::routes::skills::SkillSourceRemoveResponse),
    SkillsAddResponse(crate::api::routes::skills::SkillsAddResponse),
    SkillsCatalogResponse(crate::api::routes::skills::SkillsCatalogResponse),
    SkillsListResponse(crate::api::routes::skills::SkillsListResponse),
    SkillsRemoveResponse(crate::api::routes::skills::SkillsRemoveResponse),
    StatusAgentResponse(crate::api::routes::status::StatusAgentResponse),
    StatusConnectionsResponse(crate::api::routes::status::StatusConnectionsResponse),
    StatusResponse(crate::api::routes::status::StatusResponse),
    WorkspaceMetadataResponse(crate::api::routes::workspace::WorkspaceMetadataResponse),
    WsConnectionsResponse(crate::api::routes::ws::WsConnectionsResponse),
    WsSessionsResponse(crate::api::routes::ws::WsSessionsResponse),
}
