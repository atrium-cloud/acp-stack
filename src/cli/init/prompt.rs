//! Shared interactive prompt helpers for `acps init`, built on `cliclack`.
//!
//! Every helper checks `interactive` FIRST and returns its skip/default value
//! without touching `cliclack`, so the wizard is never driven without a
//! terminal.

use std::cell::RefCell;
use std::io;
use std::sync::Arc;

use crate::config::AgentConfigOptionValue;
use crate::error::{Result, StackError};
use crate::runtime::agent::config_options::{
    SNAPSHOT_KIND_BOOLEAN, SNAPSHOT_KIND_SELECT, SessionConfigOptionSnapshot,
    SessionConfigOptionSnapshotValue,
};
use crate::runtime::agent::native_config_import::{NativeConfigInspection, NativeConfigSelection};

#[cfg(test)]
use super::state_signal::InitCategory;
use super::state_signal::InitStateSignal;

/// One variant per prompt site in the init wizard. `as_str` is shared wire
/// surface hosted clients key rendering off, so a rename is a wire break.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HostedPromptKind {
    Agent,
    ProviderId,
    ProviderName,
    BaseUrl,
    ApiKeyRef,
    Model,
    Mode,
    Effort,
    ConfigOption,
    NativeConfigReview,
    TestflightConfirm,
    ProviderApiKeyValue,
    SecretRefValue,
    McpAdd,
    McpTransport,
    McpRowAction,
    McpStdioName,
    McpStdioCommand,
    McpStdioArgs,
    McpStdioEnvRefs,
    McpHttpName,
    McpHttpUrl,
    McpHttpHeaders,
    ConfigSource,
    ConfigSourcePath,
    ConfigSourceBase64,
    CustomAgentId,
    CustomAgentName,
    CustomAgentCommand,
    CustomAgentArgs,
    CustomAgentInstallShell,
    CustomAgentCreates,
    SkillsSource,
    SkillsGithubOwner,
    SkillsManualNames,
    SkillsSelect,
    SubagentInheritConfirm,
    StackUpdatePolicy,
    UpdateFrequency,
    UpdateFrequencyCustom,
    AgentUpdateEnabled,
    EnvironmentSetup,
    EssentialDepsConfirm,
    BrowserUseConfirm,
    EssentialSkillsConfirm,
    DataSourcesConfirm,
    CustomDepsConfirm,
    AgentSkillsConfirm,
    AgentEnvRefsConfirm,
    DataSourceType,
    DataSourceRowAction,
    DataSourceLocalPath,
    DataSourceHttpsUrl,
    DataSourceS3Bucket,
    DataSourceS3Region,
    DataSourceS3AccessKeyRef,
    DataSourceS3SecretKeyRef,
    DataSourceS3Prefix,
    DependencyName,
    DependencyInstallShell,
    DependencyScope,
    DepsApplyConfirm,
    AgentEnvRefName,
}

impl HostedPromptKind {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            HostedPromptKind::Agent => "agent",
            HostedPromptKind::ProviderId => "provider_id",
            HostedPromptKind::ProviderName => "provider_name",
            HostedPromptKind::BaseUrl => "base_url",
            HostedPromptKind::ApiKeyRef => "api_key_ref",
            HostedPromptKind::Model => "model",
            HostedPromptKind::Mode => "mode",
            HostedPromptKind::Effort => "effort",
            HostedPromptKind::ConfigOption => "config_option",
            HostedPromptKind::NativeConfigReview => "native_config_review",
            HostedPromptKind::TestflightConfirm => "testflight_confirm",
            HostedPromptKind::ProviderApiKeyValue => "provider_api_key_value",
            HostedPromptKind::SecretRefValue => "secret_ref_value",
            HostedPromptKind::McpAdd => "mcp_add",
            HostedPromptKind::McpTransport => "mcp_transport",
            HostedPromptKind::McpRowAction => "mcp_row_action",
            HostedPromptKind::McpStdioName => "mcp_stdio_name",
            HostedPromptKind::McpStdioCommand => "mcp_stdio_command",
            HostedPromptKind::McpStdioArgs => "mcp_stdio_args",
            HostedPromptKind::McpStdioEnvRefs => "mcp_stdio_env_refs",
            HostedPromptKind::McpHttpName => "mcp_http_name",
            HostedPromptKind::McpHttpUrl => "mcp_http_url",
            HostedPromptKind::McpHttpHeaders => "mcp_http_headers",
            HostedPromptKind::ConfigSource => "config_source",
            HostedPromptKind::ConfigSourcePath => "config_source_path",
            HostedPromptKind::ConfigSourceBase64 => "config_source_base64",
            HostedPromptKind::CustomAgentId => "custom_agent_id",
            HostedPromptKind::CustomAgentName => "custom_agent_name",
            HostedPromptKind::CustomAgentCommand => "custom_agent_command",
            HostedPromptKind::CustomAgentArgs => "custom_agent_args",
            HostedPromptKind::CustomAgentInstallShell => "custom_agent_install_shell",
            HostedPromptKind::CustomAgentCreates => "custom_agent_creates",
            HostedPromptKind::SkillsSource => "skills_source",
            HostedPromptKind::SkillsGithubOwner => "skills_github_owner",
            HostedPromptKind::SkillsManualNames => "skills_manual_names",
            HostedPromptKind::SkillsSelect => "skills_select",
            HostedPromptKind::SubagentInheritConfirm => "subagent_inherit_confirm",
            HostedPromptKind::StackUpdatePolicy => "stack_update_policy",
            HostedPromptKind::UpdateFrequency => "update_frequency",
            HostedPromptKind::UpdateFrequencyCustom => "update_frequency_custom",
            HostedPromptKind::AgentUpdateEnabled => "agent_update_enabled",
            HostedPromptKind::EnvironmentSetup => "environment_setup",
            HostedPromptKind::EssentialDepsConfirm => "essential_deps_confirm",
            HostedPromptKind::BrowserUseConfirm => "browser_use_confirm",
            HostedPromptKind::EssentialSkillsConfirm => "essential_skills_confirm",
            HostedPromptKind::DataSourcesConfirm => "data_sources_confirm",
            HostedPromptKind::CustomDepsConfirm => "custom_deps_confirm",
            HostedPromptKind::AgentSkillsConfirm => "agent_skills_confirm",
            HostedPromptKind::AgentEnvRefsConfirm => "agent_env_refs_confirm",
            HostedPromptKind::DataSourceType => "data_source_type",
            HostedPromptKind::DataSourceRowAction => "data_source_row_action",
            HostedPromptKind::DataSourceLocalPath => "data_source_local_path",
            HostedPromptKind::DataSourceHttpsUrl => "data_source_https_url",
            HostedPromptKind::DataSourceS3Bucket => "data_source_s3_bucket",
            HostedPromptKind::DataSourceS3Region => "data_source_s3_region",
            HostedPromptKind::DataSourceS3AccessKeyRef => "data_source_s3_access_key_ref",
            HostedPromptKind::DataSourceS3SecretKeyRef => "data_source_s3_secret_key_ref",
            HostedPromptKind::DataSourceS3Prefix => "data_source_s3_prefix",
            HostedPromptKind::DependencyName => "dependency_name",
            HostedPromptKind::DependencyInstallShell => "dependency_install_shell",
            HostedPromptKind::DependencyScope => "dependency_scope",
            HostedPromptKind::DepsApplyConfirm => "deps_apply_confirm",
            HostedPromptKind::AgentEnvRefName => "agent_env_ref_name",
        }
    }

    /// Which category is waiting on this prompt; `None` for prompts that never
    /// stream or that cut across several categories.
    #[cfg(test)]
    pub(super) fn category(self) -> Option<InitCategory> {
        match self {
            HostedPromptKind::Agent => Some(InitCategory::Agent),
            HostedPromptKind::ProviderId
            | HostedPromptKind::ProviderName
            | HostedPromptKind::BaseUrl
            | HostedPromptKind::ApiKeyRef
            | HostedPromptKind::ProviderApiKeyValue => Some(InitCategory::Provider),
            HostedPromptKind::Model => Some(InitCategory::Model),
            HostedPromptKind::Mode => Some(InitCategory::Mode),
            HostedPromptKind::Effort => Some(InitCategory::Effort),
            HostedPromptKind::NativeConfigReview => Some(InitCategory::NativeConfig),
            HostedPromptKind::McpAdd
            | HostedPromptKind::McpTransport
            | HostedPromptKind::McpRowAction
            | HostedPromptKind::McpStdioName
            | HostedPromptKind::McpStdioCommand
            | HostedPromptKind::McpStdioArgs
            | HostedPromptKind::McpStdioEnvRefs
            | HostedPromptKind::McpHttpName
            | HostedPromptKind::McpHttpUrl
            | HostedPromptKind::McpHttpHeaders => Some(InitCategory::Mcp),
            HostedPromptKind::ConfigOption
            | HostedPromptKind::SecretRefValue
            | HostedPromptKind::TestflightConfirm
            | HostedPromptKind::ConfigSource
            | HostedPromptKind::ConfigSourcePath
            | HostedPromptKind::ConfigSourceBase64
            | HostedPromptKind::CustomAgentId
            | HostedPromptKind::CustomAgentName
            | HostedPromptKind::CustomAgentCommand
            | HostedPromptKind::CustomAgentArgs
            | HostedPromptKind::CustomAgentInstallShell
            | HostedPromptKind::CustomAgentCreates
            | HostedPromptKind::SkillsSource
            | HostedPromptKind::SkillsGithubOwner
            | HostedPromptKind::SkillsManualNames
            | HostedPromptKind::SkillsSelect
            | HostedPromptKind::SubagentInheritConfirm
            | HostedPromptKind::StackUpdatePolicy
            | HostedPromptKind::UpdateFrequency
            | HostedPromptKind::UpdateFrequencyCustom
            | HostedPromptKind::AgentUpdateEnabled
            | HostedPromptKind::EnvironmentSetup
            | HostedPromptKind::EssentialDepsConfirm
            | HostedPromptKind::BrowserUseConfirm
            | HostedPromptKind::EssentialSkillsConfirm
            | HostedPromptKind::DataSourcesConfirm
            | HostedPromptKind::CustomDepsConfirm
            | HostedPromptKind::AgentSkillsConfirm
            | HostedPromptKind::AgentEnvRefsConfirm
            | HostedPromptKind::DataSourceType
            | HostedPromptKind::DataSourceRowAction
            | HostedPromptKind::DataSourceLocalPath
            | HostedPromptKind::DataSourceHttpsUrl
            | HostedPromptKind::DataSourceS3Bucket
            | HostedPromptKind::DataSourceS3Region
            | HostedPromptKind::DataSourceS3AccessKeyRef
            | HostedPromptKind::DataSourceS3SecretKeyRef
            | HostedPromptKind::DataSourceS3Prefix
            | HostedPromptKind::DependencyName
            | HostedPromptKind::DependencyInstallShell
            | HostedPromptKind::DependencyScope
            | HostedPromptKind::DepsApplyConfirm
            | HostedPromptKind::AgentEnvRefName => None,
        }
    }
}

/// Hand-maintained roster for the wire-string uniqueness test.
#[cfg(test)]
pub(super) const ALL_HOSTED_PROMPT_KINDS: &[HostedPromptKind] = &[
    HostedPromptKind::Agent,
    HostedPromptKind::ProviderId,
    HostedPromptKind::ProviderName,
    HostedPromptKind::BaseUrl,
    HostedPromptKind::ApiKeyRef,
    HostedPromptKind::Model,
    HostedPromptKind::Mode,
    HostedPromptKind::Effort,
    HostedPromptKind::ConfigOption,
    HostedPromptKind::NativeConfigReview,
    HostedPromptKind::TestflightConfirm,
    HostedPromptKind::ProviderApiKeyValue,
    HostedPromptKind::SecretRefValue,
    HostedPromptKind::McpAdd,
    HostedPromptKind::McpTransport,
    HostedPromptKind::McpRowAction,
    HostedPromptKind::McpStdioName,
    HostedPromptKind::McpStdioCommand,
    HostedPromptKind::McpStdioArgs,
    HostedPromptKind::McpStdioEnvRefs,
    HostedPromptKind::McpHttpName,
    HostedPromptKind::McpHttpUrl,
    HostedPromptKind::McpHttpHeaders,
    HostedPromptKind::ConfigSource,
    HostedPromptKind::ConfigSourcePath,
    HostedPromptKind::ConfigSourceBase64,
    HostedPromptKind::CustomAgentId,
    HostedPromptKind::CustomAgentName,
    HostedPromptKind::CustomAgentCommand,
    HostedPromptKind::CustomAgentArgs,
    HostedPromptKind::CustomAgentInstallShell,
    HostedPromptKind::CustomAgentCreates,
    HostedPromptKind::SkillsSource,
    HostedPromptKind::SkillsGithubOwner,
    HostedPromptKind::SkillsManualNames,
    HostedPromptKind::SkillsSelect,
    HostedPromptKind::SubagentInheritConfirm,
    HostedPromptKind::StackUpdatePolicy,
    HostedPromptKind::UpdateFrequency,
    HostedPromptKind::UpdateFrequencyCustom,
    HostedPromptKind::AgentUpdateEnabled,
    HostedPromptKind::EnvironmentSetup,
    HostedPromptKind::EssentialDepsConfirm,
    HostedPromptKind::BrowserUseConfirm,
    HostedPromptKind::EssentialSkillsConfirm,
    HostedPromptKind::DataSourcesConfirm,
    HostedPromptKind::CustomDepsConfirm,
    HostedPromptKind::AgentSkillsConfirm,
    HostedPromptKind::AgentEnvRefsConfirm,
    HostedPromptKind::DataSourceType,
    HostedPromptKind::DataSourceRowAction,
    HostedPromptKind::DataSourceLocalPath,
    HostedPromptKind::DataSourceHttpsUrl,
    HostedPromptKind::DataSourceS3Bucket,
    HostedPromptKind::DataSourceS3Region,
    HostedPromptKind::DataSourceS3AccessKeyRef,
    HostedPromptKind::DataSourceS3SecretKeyRef,
    HostedPromptKind::DataSourceS3Prefix,
    HostedPromptKind::DependencyName,
    HostedPromptKind::DependencyInstallShell,
    HostedPromptKind::DependencyScope,
    HostedPromptKind::DepsApplyConfirm,
    HostedPromptKind::AgentEnvRefName,
];

/// A pickable item: `id` is the stable wire identity a hosted client answers
/// with, `value` the in-process choice, `label` freely reworded display text.
#[derive(Debug, Clone)]
pub(super) struct PromptItem<T> {
    pub(super) value: T,
    pub(super) id: String,
    pub(super) label: String,
    pub(super) hint: String,
}

pub(super) fn item<T>(
    value: T,
    id: impl Into<String>,
    label: impl Into<String>,
    hint: impl Into<String>,
) -> PromptItem<T> {
    PromptItem {
        value,
        id: id.into(),
        label: label.into(),
        hint: hint.into(),
    }
}

#[derive(Debug, Clone)]
pub(super) struct HostedPromptItem {
    pub(super) value: String,
    pub(super) label: String,
    pub(super) hint: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HostedPromptStyle {
    Select,
    SearchableSelect,
    Confirm,
    Text,
    Password,
    NativeConfigReview,
}

#[derive(Debug, Clone)]
pub(super) struct HostedPromptRequest {
    pub(super) kind: HostedPromptKind,
    pub(super) style: HostedPromptStyle,
    pub(super) prompt: String,
    pub(super) required: bool,
    pub(super) default: Option<bool>,
    pub(super) items: Vec<HostedPromptItem>,
    pub(super) inspection: Option<NativeConfigInspection>,
    pub(super) config_option: Option<SessionConfigOptionSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum HostedPromptOutcome<T> {
    Handled(T),
    Unhandled,
}

/// A confirm answer plus the hosted-only `deferred` flag, which is what tells a
/// backend deferring the work apart from an operator declining it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ConfirmAnswer {
    pub(super) value: bool,
    pub(super) deferred: bool,
}

impl ConfirmAnswer {
    /// The terminal path and every driver that cannot receive the flag.
    pub(super) fn plain(value: bool) -> Self {
        Self {
            value,
            deferred: false,
        }
    }
}

pub(super) trait HostedPromptDriver: Send + Sync {
    fn select(&self, request: HostedPromptRequest) -> Result<HostedPromptOutcome<Option<usize>>>;
    fn confirm(&self, request: HostedPromptRequest) -> Result<HostedPromptOutcome<bool>>;
    /// Confirm answer with the frame's `deferred` sibling preserved.
    fn confirm_with_deferral(
        &self,
        request: HostedPromptRequest,
    ) -> Result<HostedPromptOutcome<ConfirmAnswer>> {
        Ok(match self.confirm(request)? {
            HostedPromptOutcome::Handled(value) => {
                HostedPromptOutcome::Handled(ConfirmAnswer::plain(value))
            }
            HostedPromptOutcome::Unhandled => HostedPromptOutcome::Unhandled,
        })
    }
    fn text(&self, request: HostedPromptRequest) -> Result<HostedPromptOutcome<Option<String>>>;
    fn password(&self, request: HostedPromptRequest)
    -> Result<HostedPromptOutcome<Option<String>>>;
    fn native_config_review(
        &self,
        _request: HostedPromptRequest,
    ) -> Result<HostedPromptOutcome<NativeConfigSelection>> {
        Ok(HostedPromptOutcome::Unhandled)
    }
    fn config_option(
        &self,
        _request: HostedPromptRequest,
    ) -> Result<HostedPromptOutcome<Option<AgentConfigOptionValue>>> {
        Ok(HostedPromptOutcome::Unhandled)
    }
    fn progress(&self, message: String);
    fn result(&self, payload: serde_json::Value);
    /// Machine-readable counterpart to `progress`.
    fn state_signal(&self, _signal: InitStateSignal) {}
    /// Whether this driver declared it will supply a custom provider's
    /// credential out-of-band after init.
    fn defer_provider_credentials(&self) -> bool {
        false
    }
}

/// Shared test double: captures state signals and leaves every prompt
/// `Unhandled`, as if no client answered.
#[cfg(test)]
#[derive(Default)]
pub(super) struct RecordingPromptDriver {
    signals: std::sync::Mutex<Vec<InitStateSignal>>,
    password_prompts: std::sync::Mutex<Vec<String>>,
    defer_provider_credentials: bool,
}

#[cfg(test)]
impl RecordingPromptDriver {
    /// A driver that declared it will push provider credentials out-of-band.
    pub(super) fn deferring_provider_credentials() -> Self {
        Self {
            defer_provider_credentials: true,
            ..Self::default()
        }
    }

    pub(super) fn recorded(&self) -> Vec<InitStateSignal> {
        self.signals
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// The password prompts this driver was asked to stream.
    pub(super) fn recorded_password_prompts(&self) -> Vec<String> {
        self.password_prompts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

#[cfg(test)]
impl HostedPromptDriver for RecordingPromptDriver {
    fn select(&self, _request: HostedPromptRequest) -> Result<HostedPromptOutcome<Option<usize>>> {
        Ok(HostedPromptOutcome::Unhandled)
    }

    fn confirm(&self, _request: HostedPromptRequest) -> Result<HostedPromptOutcome<bool>> {
        Ok(HostedPromptOutcome::Unhandled)
    }

    fn text(&self, _request: HostedPromptRequest) -> Result<HostedPromptOutcome<Option<String>>> {
        Ok(HostedPromptOutcome::Unhandled)
    }

    fn password(
        &self,
        request: HostedPromptRequest,
    ) -> Result<HostedPromptOutcome<Option<String>>> {
        self.password_prompts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(request.prompt);
        Ok(HostedPromptOutcome::Unhandled)
    }

    fn progress(&self, _message: String) {}

    fn result(&self, _payload: serde_json::Value) {}

    fn state_signal(&self, signal: InitStateSignal) {
        self.signals
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(signal);
    }

    fn defer_provider_credentials(&self) -> bool {
        self.defer_provider_credentials
    }
}

thread_local! {
    static HOSTED_DRIVER: RefCell<Option<Arc<dyn HostedPromptDriver>>> = RefCell::new(None);
}

pub(super) fn with_hosted_driver<T>(
    driver: Arc<dyn HostedPromptDriver>,
    work: impl FnOnce() -> T,
) -> T {
    struct DriverReset(Option<Arc<dyn HostedPromptDriver>>);

    impl Drop for DriverReset {
        fn drop(&mut self) {
            HOSTED_DRIVER.with(|slot| {
                *slot.borrow_mut() = self.0.take();
            });
        }
    }

    let previous = HOSTED_DRIVER.with(|slot| slot.borrow_mut().replace(driver));
    let _reset = DriverReset(previous);
    work()
}

pub(super) fn hosted_driver_active() -> bool {
    HOSTED_DRIVER.with(|slot| slot.borrow().is_some())
}

/// Whether the active hosted driver declared that provider credentials arrive
/// out-of-band; false everywhere else, so a missing ref stays a hard failure.
pub(super) fn defer_provider_credentials() -> bool {
    HOSTED_DRIVER.with(|slot| {
        slot.borrow()
            .as_ref()
            .is_some_and(|driver| driver.defer_provider_credentials())
    })
}

pub(super) fn emit_progress(message: impl Into<String>) {
    if let Some(driver) = HOSTED_DRIVER.with(|slot| slot.borrow().clone()) {
        driver.progress(message.into());
    }
}

/// Emit one state signal, built lazily so terminal runs skip the derivation.
pub(super) fn emit_state_signal(build: impl FnOnce() -> InitStateSignal) {
    if let Some(driver) = HOSTED_DRIVER.with(|slot| slot.borrow().clone()) {
        driver.state_signal(build());
    }
}

/// Batch form for a derivation that settles several categories at one instant.
pub(super) fn emit_state_signals(build: impl FnOnce() -> Vec<InitStateSignal>) {
    if let Some(driver) = HOSTED_DRIVER.with(|slot| slot.borrow().clone()) {
        for signal in build() {
            driver.state_signal(signal);
        }
    }
}

pub(super) fn emit_result(payload: serde_json::Value) {
    if let Some(driver) = HOSTED_DRIVER.with(|slot| slot.borrow().clone()) {
        driver.result(payload);
    }
}

pub(super) fn native_config_review(
    inspection: NativeConfigInspection,
) -> Result<NativeConfigSelection> {
    let Some(driver) = HOSTED_DRIVER.with(|slot| slot.borrow().clone()) else {
        return Err(StackError::InvalidParam {
            field: "native_config",
            reason: "native config upload is supported only by hosted init".to_owned(),
        });
    };
    let request = HostedPromptRequest {
        kind: HostedPromptKind::NativeConfigReview,
        style: HostedPromptStyle::NativeConfigReview,
        prompt: "Review native Agent config".to_owned(),
        required: true,
        default: None,
        items: Vec::new(),
        inspection: Some(inspection),
        config_option: None,
    };
    match driver.native_config_review(request)? {
        HostedPromptOutcome::Handled(selection) => Ok(selection),
        HostedPromptOutcome::Unhandled => Err(StackError::InvalidParam {
            field: "native_config",
            reason: "hosted init client did not handle native config review".to_owned(),
        }),
    }
}

/// One generic ACP config option discovered from the provisional init session.
/// A hosted client answers with `{ "config_id": "...", "value": ... }`; a
/// null value keeps the agent's advertised default and writes no override.
pub(super) fn config_option(
    option: SessionConfigOptionSnapshot,
    interactive: bool,
) -> Result<Option<AgentConfigOptionValue>> {
    if let Some(driver) = HOSTED_DRIVER.with(|slot| slot.borrow().clone()) {
        let style = match option.kind.as_str() {
            SNAPSHOT_KIND_SELECT => HostedPromptStyle::SearchableSelect,
            SNAPSHOT_KIND_BOOLEAN => HostedPromptStyle::Confirm,
            _ => {
                return Err(StackError::InvalidParam {
                    field: "agent.config_options",
                    reason: format!("option `{}` has an unsupported type", option.id),
                });
            }
        };
        let default = match &option.current_value {
            SessionConfigOptionSnapshotValue::Bool(value) => Some(*value),
            SessionConfigOptionSnapshotValue::Text(_) => None,
        };
        let items = option
            .options
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|choice| HostedPromptItem {
                value: choice.value.clone(),
                label: choice.name.clone(),
                hint: choice.description.clone().unwrap_or_default(),
            })
            .collect();
        let request = HostedPromptRequest {
            kind: HostedPromptKind::ConfigOption,
            style,
            prompt: option.name.clone(),
            required: false,
            default,
            items,
            inspection: None,
            config_option: Some(option),
        };
        return match driver.config_option(request)? {
            HostedPromptOutcome::Handled(value) => Ok(value),
            HostedPromptOutcome::Unhandled => Ok(None),
        };
    }
    if !interactive {
        return Ok(None);
    }

    let items = config_option_items(&option)?;
    match select(
        HostedPromptKind::ConfigOption,
        interactive,
        &option.name,
        &items,
    )? {
        Some(ConfigOptionChoice::Bool(value)) => Ok(Some(AgentConfigOptionValue::Bool(value))),
        Some(ConfigOptionChoice::Text(value)) => Ok(Some(AgentConfigOptionValue::Text(value))),
        Some(ConfigOptionChoice::KeepCurrent) | None => Ok(None),
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
enum ConfigOptionChoice {
    Bool(bool),
    Text(String),
    KeepCurrent,
}

/// Interactive items for one generic config option. The advertised current
/// value is marked so an operator can see what "Keep current" preserves; the
/// hosted path communicates it through the snapshot on the request instead.
fn config_option_items(
    option: &SessionConfigOptionSnapshot,
) -> Result<Vec<PromptItem<ConfigOptionChoice>>> {
    let current = match &option.current_value {
        SessionConfigOptionSnapshotValue::Bool(value) => ConfigOptionChoice::Bool(*value),
        SessionConfigOptionSnapshotValue::Text(value) => ConfigOptionChoice::Text(value.clone()),
    };
    let label = |choice: &ConfigOptionChoice, name: &str| {
        if *choice == current {
            format!("{name} (current)")
        } else {
            name.to_owned()
        }
    };
    let mut items: Vec<PromptItem<ConfigOptionChoice>> = match option.kind.as_str() {
        SNAPSHOT_KIND_SELECT => option
            .options
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|choice| {
                let value = ConfigOptionChoice::Text(choice.value.clone());
                item(
                    value.clone(),
                    choice.value.clone(),
                    label(&value, &choice.name),
                    choice.description.clone().unwrap_or_default(),
                )
            })
            .collect(),
        SNAPSHOT_KIND_BOOLEAN => [(true, "Enabled"), (false, "Disabled")]
            .into_iter()
            .map(|(enabled, name)| {
                let value = ConfigOptionChoice::Bool(enabled);
                item(
                    value.clone(),
                    if enabled { "true" } else { "false" },
                    label(&value, name),
                    "",
                )
            })
            .collect(),
        _ => {
            return Err(StackError::InvalidParam {
                field: "agent.config_options",
                reason: format!("option `{}` has an unsupported type", option.id),
            });
        }
    };
    items.push(item(
        ConfigOptionChoice::KeepCurrent,
        "__keep_current",
        "Keep current",
        "",
    ));
    Ok(items)
}

fn map_interact_error(source: io::Error) -> StackError {
    StackError::StdinRead { source }
}

fn cancelled() -> StackError {
    StackError::InvalidParam {
        field: "init",
        reason: "cancelled by operator".to_owned(),
    }
}

/// Single-choice picker; `None` when not interactive or nothing to choose.
pub(super) fn select<T: Clone + Eq>(
    kind: HostedPromptKind,
    interactive: bool,
    prompt: &str,
    items: &[PromptItem<T>],
) -> Result<Option<T>> {
    select_inner(kind, interactive, prompt, items, false)
}

pub(super) fn searchable_select<T: Clone + Eq>(
    kind: HostedPromptKind,
    interactive: bool,
    prompt: &str,
    items: &[PromptItem<T>],
) -> Result<Option<T>> {
    select_inner(kind, interactive, prompt, items, true)
}

fn select_inner<T: Clone + Eq>(
    kind: HostedPromptKind,
    interactive: bool,
    prompt: &str,
    items: &[PromptItem<T>],
    searchable: bool,
) -> Result<Option<T>> {
    // Answers address options by id, so a collision makes one of them
    // unreachable over the wire while the terminal path looks fine.
    debug_assert!(
        items
            .iter()
            .map(|item| item.id.as_str())
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            == items.len(),
        "prompt `{prompt}` has duplicate option ids"
    );
    if let Some(driver) = HOSTED_DRIVER.with(|slot| slot.borrow().clone()) {
        let request = hosted_request(
            kind,
            if searchable {
                HostedPromptStyle::SearchableSelect
            } else {
                HostedPromptStyle::Select
            },
            prompt,
            false,
            None,
            items,
        );
        return match driver.select(request)? {
            HostedPromptOutcome::Handled(Some(index)) => items
                .get(index)
                .map(|item| Some(item.value.clone()))
                .ok_or(StackError::InvalidParam {
                    field: "init",
                    reason: format!("hosted init selected invalid item index {index}"),
                }),
            HostedPromptOutcome::Handled(None) | HostedPromptOutcome::Unhandled => Ok(None),
        };
    }
    if !interactive || items.is_empty() {
        return Ok(None);
    }
    let mut builder = cliclack::select::<T>(prompt);
    if searchable {
        builder = builder.filter_mode().max_rows(12);
    }
    for entry in items {
        builder = builder.item(entry.value.clone(), &entry.label, &entry.hint);
    }
    match builder.interact() {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == io::ErrorKind::Interrupted => Err(cancelled()),
        Err(error) => Err(map_interact_error(error)),
    }
}

/// Yes/no confirm; returns `default` when not interactive.
pub(super) fn confirm(
    kind: HostedPromptKind,
    interactive: bool,
    prompt: &str,
    default: bool,
) -> Result<bool> {
    confirm_with_deferral(kind, interactive, prompt, default).map(|answer| answer.value)
}

/// `confirm` that preserves the hosted `deferred` flag; terminal answers are
/// never deferred.
pub(super) fn confirm_with_deferral(
    kind: HostedPromptKind,
    interactive: bool,
    prompt: &str,
    default: bool,
) -> Result<ConfirmAnswer> {
    if let Some(driver) = HOSTED_DRIVER.with(|slot| slot.borrow().clone()) {
        let request = HostedPromptRequest {
            kind,
            style: HostedPromptStyle::Confirm,
            prompt: prompt.to_owned(),
            required: true,
            default: Some(default),
            items: Vec::new(),
            inspection: None,
            config_option: None,
        };
        return match driver.confirm_with_deferral(request)? {
            HostedPromptOutcome::Handled(answer) => Ok(answer),
            HostedPromptOutcome::Unhandled => Ok(ConfirmAnswer::plain(default)),
        };
    }
    if !interactive {
        return Ok(ConfirmAnswer::plain(default));
    }
    match cliclack::confirm(prompt).initial_value(default).interact() {
        Ok(value) => Ok(ConfirmAnswer::plain(value)),
        Err(error) if error.kind() == io::ErrorKind::Interrupted => Err(cancelled()),
        Err(error) => Err(map_interact_error(error)),
    }
}

/// Free-text line; `required` re-prompts on empty input, `None` when not
/// interactive.
pub(super) fn text(
    kind: HostedPromptKind,
    interactive: bool,
    prompt: &str,
    required: bool,
) -> Result<Option<String>> {
    if let Some(driver) = HOSTED_DRIVER.with(|slot| slot.borrow().clone()) {
        let request = HostedPromptRequest {
            kind,
            style: HostedPromptStyle::Text,
            prompt: prompt.to_owned(),
            required,
            default: None,
            items: Vec::new(),
            inspection: None,
            config_option: None,
        };
        return match driver.text(request)? {
            HostedPromptOutcome::Handled(value) => Ok(value),
            HostedPromptOutcome::Unhandled => Ok(None),
        };
    }
    if !interactive {
        return Ok(None);
    }
    let result: io::Result<String> = cliclack::input(prompt).required(required).interact();
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == io::ErrorKind::Interrupted => Err(cancelled()),
        Err(error) => Err(map_interact_error(error)),
    }
}

/// Run `work` under a spinner; interactive path only, since cliclack writes the
/// spinner straight to the terminal.
pub(super) fn with_spinner<T>(message: &str, work: impl FnOnce() -> Result<T>) -> Result<T> {
    if hosted_driver_active() {
        emit_progress(message.to_owned());
        return work();
    }
    let spinner = cliclack::spinner();
    spinner.start(message);
    match work() {
        Ok(value) => {
            spinner.stop(message);
            Ok(value)
        }
        Err(error) => {
            spinner.error(message);
            Err(error)
        }
    }
}

/// Masked secret entry; `None` when not interactive. `required` is hosted-wire
/// metadata only — the server accepts a `null` answer either way.
pub(super) fn password(
    kind: HostedPromptKind,
    interactive: bool,
    prompt: &str,
    required: bool,
) -> Result<Option<String>> {
    if let Some(driver) = HOSTED_DRIVER.with(|slot| slot.borrow().clone()) {
        let request = HostedPromptRequest {
            kind,
            style: HostedPromptStyle::Password,
            prompt: prompt.to_owned(),
            required,
            default: None,
            items: Vec::new(),
            inspection: None,
            config_option: None,
        };
        return match driver.password(request)? {
            HostedPromptOutcome::Handled(value) => Ok(value),
            HostedPromptOutcome::Unhandled => Ok(None),
        };
    }
    if !interactive {
        return Ok(None);
    }
    match cliclack::password(prompt).mask('•').interact() {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == io::ErrorKind::Interrupted => Err(cancelled()),
        Err(error) => Err(map_interact_error(error)),
    }
}

fn hosted_request<T>(
    kind: HostedPromptKind,
    style: HostedPromptStyle,
    prompt: &str,
    required: bool,
    default: Option<bool>,
    items: &[PromptItem<T>],
) -> HostedPromptRequest {
    HostedPromptRequest {
        kind,
        style,
        prompt: prompt.to_owned(),
        required,
        default,
        items: items
            .iter()
            .map(|entry| HostedPromptItem {
                value: entry.id.clone(),
                label: entry.label.clone(),
                hint: entry.hint.clone(),
            })
            .collect(),
        inspection: None,
        config_option: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_returns_none_when_not_interactive() {
        let items = [item(1u8, "one", "one", "")];
        assert_eq!(
            select(HostedPromptKind::Model, false, "pick", &items).expect("select"),
            None
        );
    }

    #[test]
    fn confirm_returns_default_when_not_interactive() {
        assert!(confirm(HostedPromptKind::TestflightConfirm, false, "ok?", true).expect("true"));
        assert!(!confirm(HostedPromptKind::TestflightConfirm, false, "ok?", false).expect("false"));
    }

    #[test]
    fn text_returns_none_when_not_interactive() {
        assert_eq!(
            text(HostedPromptKind::ProviderName, false, "name", true).expect("text"),
            None
        );
    }

    #[test]
    fn password_returns_none_when_not_interactive() {
        assert_eq!(
            password(HostedPromptKind::SecretRefValue, false, "secret", true).expect("password"),
            None
        );
    }

    #[test]
    fn hosted_prompt_kind_wire_strings_are_unique() {
        let mut seen = std::collections::BTreeSet::new();
        for kind in ALL_HOSTED_PROMPT_KINDS {
            assert!(
                seen.insert(kind.as_str()),
                "duplicate hosted prompt kind wire string `{}`",
                kind.as_str()
            );
        }
        assert_eq!(seen.len(), ALL_HOSTED_PROMPT_KINDS.len());
    }

    #[test]
    fn hosted_prompt_kind_categories_match_the_lane_they_block() {
        let mapped = [
            (HostedPromptKind::Agent, Some(InitCategory::Agent)),
            (HostedPromptKind::ProviderId, Some(InitCategory::Provider)),
            (HostedPromptKind::ProviderName, Some(InitCategory::Provider)),
            (HostedPromptKind::BaseUrl, Some(InitCategory::Provider)),
            (HostedPromptKind::ApiKeyRef, Some(InitCategory::Provider)),
            (
                HostedPromptKind::ProviderApiKeyValue,
                Some(InitCategory::Provider),
            ),
            (HostedPromptKind::Model, Some(InitCategory::Model)),
            (HostedPromptKind::Mode, Some(InitCategory::Mode)),
            (HostedPromptKind::Effort, Some(InitCategory::Effort)),
            (
                HostedPromptKind::NativeConfigReview,
                Some(InitCategory::NativeConfig),
            ),
            (HostedPromptKind::McpAdd, Some(InitCategory::Mcp)),
            (HostedPromptKind::McpHttpHeaders, Some(InitCategory::Mcp)),
            (HostedPromptKind::SecretRefValue, None),
            (HostedPromptKind::TestflightConfirm, None),
            (HostedPromptKind::SkillsSelect, None),
            (HostedPromptKind::DataSourceS3Bucket, None),
        ];
        for (kind, category) in mapped {
            assert_eq!(kind.category(), category, "kind {}", kind.as_str());
        }
    }

    #[test]
    fn non_hostable_kinds_have_no_category() {
        for kind in ALL_HOSTED_PROMPT_KINDS {
            let hostable = matches!(
                kind,
                HostedPromptKind::Agent
                    | HostedPromptKind::ProviderId
                    | HostedPromptKind::ProviderName
                    | HostedPromptKind::BaseUrl
                    | HostedPromptKind::ApiKeyRef
                    | HostedPromptKind::Model
                    | HostedPromptKind::Mode
                    | HostedPromptKind::Effort
                    | HostedPromptKind::NativeConfigReview
                    | HostedPromptKind::TestflightConfirm
                    | HostedPromptKind::ProviderApiKeyValue
                    | HostedPromptKind::SecretRefValue
                    | HostedPromptKind::McpAdd
                    | HostedPromptKind::McpTransport
                    | HostedPromptKind::McpRowAction
                    | HostedPromptKind::McpStdioName
                    | HostedPromptKind::McpStdioCommand
                    | HostedPromptKind::McpStdioArgs
                    | HostedPromptKind::McpStdioEnvRefs
                    | HostedPromptKind::McpHttpName
                    | HostedPromptKind::McpHttpUrl
                    | HostedPromptKind::McpHttpHeaders
            );
            if !hostable {
                assert_eq!(
                    kind.category(),
                    None,
                    "non-hostable kind {} must not claim a category",
                    kind.as_str()
                );
            }
        }
    }

    #[test]
    fn config_option_items_mark_the_advertised_current_value() {
        let select: SessionConfigOptionSnapshot = serde_json::from_value(serde_json::json!({
            "id": "agent.persona",
            "name": "Persona",
            "type": "select",
            "current_value": "balanced",
            "options": [
                { "value": "balanced", "name": "Balanced" },
                { "value": "research", "name": "Research" }
            ]
        }))
        .expect("select option");
        let items = config_option_items(&select).expect("select items");
        let labels: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();
        assert_eq!(labels, ["Balanced (current)", "Research", "Keep current"]);

        let boolean: SessionConfigOptionSnapshot = serde_json::from_value(serde_json::json!({
            "id": "fast",
            "name": "Fast mode",
            "type": "boolean",
            "current_value": false
        }))
        .expect("boolean option");
        let items = config_option_items(&boolean).expect("boolean items");
        let labels: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();
        assert_eq!(labels, ["Enabled", "Disabled (current)", "Keep current"]);
    }

    #[test]
    fn config_option_items_reject_an_unsupported_kind() {
        let option: SessionConfigOptionSnapshot = serde_json::from_value(serde_json::json!({
            "id": "future",
            "name": "Future",
            "type": "slider",
            "current_value": "1"
        }))
        .expect("option");
        let error = config_option_items(&option).expect_err("unsupported kind");
        assert!(
            error.to_string().contains("unsupported type"),
            "error: {error}"
        );
    }
}
