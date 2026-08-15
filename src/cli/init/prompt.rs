//! Shared interactive prompt helpers for `acps init`, built on `cliclack`.
//!
//! Every helper takes an `interactive: bool` and checks it FIRST. When it is
//! false (a `--non-interactive` run or a non-TTY stdin) the helper returns its
//! skip/default value WITHOUT touching `cliclack`, so the documented
//! non-interactive contract holds and the wizard is never driven without a
//! terminal. The caller computes `interactive` once via `prompts_enabled` in
//! `init.rs` (`is_terminal() && !args.non_interactive`).
//!
//! Esc/cancel surfaces from `cliclack::interact()` as
//! `io::ErrorKind::Interrupted`; this is a deliberate init abort. Optional
//! prompts must expose an explicit Skip item instead of treating cancellation as
//! a hidden skip.
//!
//! `--format json` is rejected for `init`, and `--handoff-json` disables prompts
//! before these helpers run, so terminal UI never collides with structured
//! output.

use std::cell::RefCell;
use std::io;
use std::sync::Arc;

use crate::error::{Result, StackError};
use crate::runtime::agent::native_config_import::{NativeConfigInspection, NativeConfigSelection};

use super::state_signal::{InitCategory, InitStateSignal};

/// One variant per prompt site in the init wizard. There is deliberately no
/// catch-all: `should_handle_hosted_prompt` matches exhaustively, so a new
/// prompt cannot be added without deciding whether it streams to hosted
/// clients. `as_str` is shared wire surface — hosted clients key rendering off
/// it — so a rename is a wire break, not a refactor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HostedPromptKind {
    Agent,
    ProviderId,
    CustomProviderConfirm,
    ProviderName,
    BaseUrl,
    ApiKeyRef,
    Model,
    Mode,
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
            HostedPromptKind::CustomProviderConfirm => "custom_provider_confirm",
            HostedPromptKind::ProviderName => "provider_name",
            HostedPromptKind::BaseUrl => "base_url",
            HostedPromptKind::ApiKeyRef => "api_key_ref",
            HostedPromptKind::Model => "model",
            HostedPromptKind::Mode => "mode",
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

    /// Which category is waiting on this prompt. `None` means the prompt does
    /// not move any category's frontier: either it never streams (the whole
    /// non-hostable set) or it is a cross-cutting ask — a secret value can be
    /// requested for an MCP server, a data source, or a provider, and the
    /// testflight confirm settles nothing.
    pub(super) fn category(self) -> Option<InitCategory> {
        match self {
            HostedPromptKind::Agent => Some(InitCategory::Agent),
            HostedPromptKind::ProviderId
            | HostedPromptKind::CustomProviderConfirm
            | HostedPromptKind::ProviderName
            | HostedPromptKind::BaseUrl
            | HostedPromptKind::ApiKeyRef
            | HostedPromptKind::ProviderApiKeyValue => Some(InitCategory::Provider),
            HostedPromptKind::Model => Some(InitCategory::Model),
            HostedPromptKind::Mode => Some(InitCategory::Mode),
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
            HostedPromptKind::SecretRefValue
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

/// Hand-maintained roster for the wire-string uniqueness test. Drift here only
/// narrows that test; the exhaustive matches on `HostedPromptKind` are what
/// force a decision for every new variant.
#[cfg(test)]
pub(super) const ALL_HOSTED_PROMPT_KINDS: &[HostedPromptKind] = &[
    HostedPromptKind::Agent,
    HostedPromptKind::ProviderId,
    HostedPromptKind::CustomProviderConfirm,
    HostedPromptKind::ProviderName,
    HostedPromptKind::BaseUrl,
    HostedPromptKind::ApiKeyRef,
    HostedPromptKind::Model,
    HostedPromptKind::Mode,
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

/// A pickable item. `id` is the stable wire identity a hosted client answers
/// with; `value` is the in-process choice the terminal path resolves to. They
/// are separate because `label` is display text that may be reworded freely.
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum HostedPromptOutcome<T> {
    Handled(T),
    Unhandled,
}

pub(super) trait HostedPromptDriver: Send + Sync {
    fn select(&self, request: HostedPromptRequest) -> Result<HostedPromptOutcome<Option<usize>>>;
    fn confirm(&self, request: HostedPromptRequest) -> Result<HostedPromptOutcome<bool>>;
    fn text(&self, request: HostedPromptRequest) -> Result<HostedPromptOutcome<Option<String>>>;
    fn password(&self, request: HostedPromptRequest)
    -> Result<HostedPromptOutcome<Option<String>>>;
    fn native_config_review(
        &self,
        _request: HostedPromptRequest,
    ) -> Result<HostedPromptOutcome<NativeConfigSelection>> {
        Ok(HostedPromptOutcome::Unhandled)
    }
    fn progress(&self, message: String);
    fn result(&self, payload: serde_json::Value);
    /// Machine-readable counterpart to `progress`. Defaulted to a no-op so a
    /// driver that only renders prompts and text needs no state map.
    fn state_signal(&self, _signal: InitStateSignal) {}
}

/// Shared test double: captures state signals the way the hosted session will,
/// and leaves every prompt `Unhandled` so a driven prompt behaves as if no
/// client answered. Lives here rather than in one module's test mod because
/// several init modules assert on the signals they emit.
#[cfg(test)]
#[derive(Default)]
pub(super) struct RecordingPromptDriver {
    signals: std::sync::Mutex<Vec<InitStateSignal>>,
}

#[cfg(test)]
impl RecordingPromptDriver {
    pub(super) fn recorded(&self) -> Vec<InitStateSignal> {
        self.signals
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
        _request: HostedPromptRequest,
    ) -> Result<HostedPromptOutcome<Option<String>>> {
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

pub(super) fn emit_progress(message: impl Into<String>) {
    if let Some(driver) = HOSTED_DRIVER.with(|slot| slot.borrow().clone()) {
        driver.progress(message.into());
    }
}

/// Signals are built lazily: a terminal run has no driver, and the derivations
/// behind these (registry lookups, joined name lists) are pure overhead there.
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
    };
    match driver.native_config_review(request)? {
        HostedPromptOutcome::Handled(selection) => Ok(selection),
        HostedPromptOutcome::Unhandled => Err(StackError::InvalidParam {
            field: "native_config",
            reason: "hosted init client did not handle native config review".to_owned(),
        }),
    }
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

/// Single-choice picker. Returns the chosen value, or `None` when not
/// interactive or when there is nothing to choose.
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
    // Answers may address an option by id, so a collision would make one of
    // the colliding options unreachable over the wire — invisible in the
    // terminal path, hence the assertion at the shared entry point.
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

/// Yes/no confirm. Returns `default` when not interactive, so the caller picks
/// the right polarity (`false` for opt-in prompts, `true` for default-yes ones).
pub(super) fn confirm(
    kind: HostedPromptKind,
    interactive: bool,
    prompt: &str,
    default: bool,
) -> Result<bool> {
    if let Some(driver) = HOSTED_DRIVER.with(|slot| slot.borrow().clone()) {
        let request = HostedPromptRequest {
            kind,
            style: HostedPromptStyle::Confirm,
            prompt: prompt.to_owned(),
            required: true,
            default: Some(default),
            items: Vec::new(),
            inspection: None,
        };
        return match driver.confirm(request)? {
            HostedPromptOutcome::Handled(value) => Ok(value),
            HostedPromptOutcome::Unhandled => Ok(default),
        };
    }
    if !interactive {
        return Ok(default);
    }
    match cliclack::confirm(prompt).initial_value(default).interact() {
        Ok(value) => Ok(value),
        Err(error) if error.kind() == io::ErrorKind::Interrupted => Err(cancelled()),
        Err(error) => Err(map_interact_error(error)),
    }
}

/// Free-text line. `required` re-prompts on empty input. Returns `None` when
/// not interactive; the caller decides whether `None` is a skip or a hard error
/// for its field.
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

/// Run `work` while showing an animated spinner with `message`. The spinner
/// stops with a success line on `Ok` and an error line on `Err`. Only call this
/// on the interactive path — cliclack writes the spinner to the terminal.
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

/// Masked secret entry. Returns `None` when not interactive. `required` is
/// hosted-wire metadata only: it tells the client whether a `null` answer
/// skips cleanly (declared-ref collection) or leads to a hard failure
/// (provider key refs); the server accepts `null` either way.
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The load-bearing invariant: non-interactive helpers return the skip /
    // default value WITHOUT touching stdin or cliclack.

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

    // The wire strings are shared API surface with hosted clients, so two kinds
    // resolving to the same string would make them indistinguishable on the
    // wire while still compiling.
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
            (
                HostedPromptKind::CustomProviderConfirm,
                Some(InitCategory::Provider),
            ),
            (HostedPromptKind::ProviderName, Some(InitCategory::Provider)),
            (HostedPromptKind::BaseUrl, Some(InitCategory::Provider)),
            (HostedPromptKind::ApiKeyRef, Some(InitCategory::Provider)),
            (
                HostedPromptKind::ProviderApiKeyValue,
                Some(InitCategory::Provider),
            ),
            (HostedPromptKind::Model, Some(InitCategory::Model)),
            (HostedPromptKind::Mode, Some(InitCategory::Mode)),
            (
                HostedPromptKind::NativeConfigReview,
                Some(InitCategory::NativeConfig),
            ),
            (HostedPromptKind::McpAdd, Some(InitCategory::Mcp)),
            (HostedPromptKind::McpHttpHeaders, Some(InitCategory::Mcp)),
            // Cross-cutting: the same masked prompt collects refs for MCP
            // servers, S3 data sources, and providers alike.
            (HostedPromptKind::SecretRefValue, None),
            (HostedPromptKind::TestflightConfirm, None),
            (HostedPromptKind::SkillsSelect, None),
            (HostedPromptKind::DataSourceS3Bucket, None),
        ];
        for (kind, category) in mapped {
            assert_eq!(kind.category(), category, "kind {}", kind.as_str());
        }
    }

    // Every non-hostable prompt is invisible to a hosted client, so claiming a
    // category for one would park that category on `awaiting_input` forever.
    #[test]
    fn non_hostable_kinds_have_no_category() {
        for kind in ALL_HOSTED_PROMPT_KINDS {
            let hostable = matches!(
                kind,
                HostedPromptKind::Agent
                    | HostedPromptKind::ProviderId
                    | HostedPromptKind::CustomProviderConfirm
                    | HostedPromptKind::ProviderName
                    | HostedPromptKind::BaseUrl
                    | HostedPromptKind::ApiKeyRef
                    | HostedPromptKind::Model
                    | HostedPromptKind::Mode
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
}
