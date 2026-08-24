//! Hand-maintained registration point for every top-level `/v1` body/query, so
//! `acps_schema` pulls them into `#/$defs/request/*`. A NEW REQUEST TYPE MUST BE
//! ADDED HERE; `schema_covers_every_handler_wire_type` guards the omission.

/// Umbrella over every request payload and query. Never deserialized itself —
/// only its derived `JsonSchema` matters, which forces each variant's type into
/// `$defs`. The enum is never constructed, so its variant-size spread and shared
/// postfixes do not warrant the usual clippy lints.
#[derive(schemars::JsonSchema)]
#[allow(dead_code, clippy::large_enum_variant, clippy::enum_variant_names)]
pub(super) enum AcpsRequestTypes {
    AgentRestartQuery(crate::api::routes::agent::lifecycle::AgentRestartQuery),
    AgentSwitchRequest(crate::api::routes::agent::switch::AgentSwitchRequest),
    AgentUpdateRequest(crate::api::routes::agent::update::AgentUpdateRequest),
    ApplyRequest(crate::extensions::managed_state::ApplyRequest),
    CommandOutputParams(crate::api::routes::commands::CommandOutputParams),
    CommandSubmitRequest(crate::api::routes::commands::CommandSubmitRequest),
    ConfigImportQuery(crate::api::routes::config::ConfigImportQuery),
    DepsApplyBody(crate::api::routes::deps::DepsApplyBody),
    DepsApplyRunsParams(crate::api::routes::deps::DepsApplyRunsParams),
    DisconnectConnectionsRequest(crate::api::routes::ws::DisconnectConnectionsRequest),
    DisconnectSessionsRequest(crate::api::routes::ws::DisconnectSessionsRequest),
    FilesContentPutBody(crate::api::routes::workspace::FilesContentPutBody),
    FilesPathParams(crate::api::routes::workspace::FilesPathParams),
    InstallerRunsParams(crate::api::routes::installer::InstallerRunsParams),
    LocalSessionAccessRequest(crate::api::routes::auth::LocalSessionAccessRequest),
    LogsCommandsParams(crate::api::routes::logs::LogsCommandsParams),
    LogsEventsParams(crate::api::routes::logs::LogsEventsParams),
    LogsLimitParams(crate::api::routes::logs::LogsLimitParams),
    LogsPermissionsParams(crate::api::routes::logs::LogsPermissionsParams),
    LogsSecurityParams(crate::api::routes::logs::LogsSecurityParams),
    LogsSessionsParams(crate::api::routes::logs::LogsSessionsParams),
    MetricsSummaryParams(crate::api::routes::metrics::MetricsSummaryParams),
    NativeConfigImportRequest(
        crate::runtime::agent::native_config_import::NativeConfigImportRequest,
    ),
    NativeConfigInspectBody(crate::api::routes::native_config::NativeConfigInspectBody),
    PermissionApproveBody(crate::api::routes::permissions::PermissionApproveBody),
    PermissionDenyBody(crate::api::routes::permissions::PermissionDenyBody),
    SecretsSetBody(crate::api::routes::config::SecretsSetBody),
    SecurityHistoryQuery(crate::api::routes::security::SecurityHistoryQuery),
    SessionCommandRunBody(crate::api::routes::sessions::commands::SessionCommandRunBody),
    SessionConfigOptionSetBody(
        crate::api::routes::sessions::config_options::SessionConfigOptionSetBody,
    ),
    SessionsCreateBody(crate::api::routes::sessions::lifecycle::SessionsCreateBody),
    SessionsEventsParams(crate::api::routes::sessions::events::SessionsEventsParams),
    SessionsForkBody(crate::api::routes::sessions::lifecycle::SessionsForkBody),
    SessionsListParams(crate::api::routes::sessions::list::SessionsListParams),
    SessionsLoadBody(crate::api::routes::sessions::lifecycle::SessionsLoadBody),
    SessionsPromptBody(crate::api::routes::sessions::prompts::SessionsPromptBody),
    SessionsStatusParams(crate::api::routes::sessions::status::SessionsStatusParams),
    SessionsTargetParams(crate::api::routes::sessions::SessionsTargetParams),
    SkillSourceAddRequest(crate::api::routes::skills::SkillSourceAddRequest),
    SkillSourceGetQuery(crate::api::routes::skills::SkillSourceGetQuery),
    SkillSourceRemoveRequest(crate::api::routes::skills::SkillSourceRemoveRequest),
    SkillsAddRequest(crate::api::routes::skills::SkillsAddRequest),
    SkillsRemoveRequest(crate::api::routes::skills::SkillsRemoveRequest),
    WsClientMessage(crate::api::ws::WsClientMessage),
}
