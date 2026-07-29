//! Semantic import support for agent-native global configuration files.
//!
//! Uploaded documents are parsed into a redacted review manifest, canonical
//! `acps` candidates, and an unmanaged residual. Managed and security-owned
//! paths never survive in the residual, even when they are not selected.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value as JsonValue};
use serde_norway::{Mapping as YamlMapping, Value as YamlValue};
use sha2::{Digest, Sha256};
use toml::{Value as TomlValue, map::Map as TomlMap};

use crate::config::{Config, IMPORT_SIZE_LIMIT, McpHttpServer, McpServerConfig, McpStdioServer};
use crate::error::{Result, StackError};
use crate::fs_util::{
    atomic_write_owner_only, create_dir_owner_only, parent_dir, prepare_owner_managed_file_path,
};
use crate::runtime::agent::agent_headless_config::provision_agent_headless_config;
use crate::runtime::agent::agent_headless_config::{
    AMP_PERMISSION_ROOTS, AMP_POLICY_ROOTS, CLAUDE_CODE_AUTH_ROOTS,
    CLAUDE_CODE_CREDENTIAL_ENV_KEYS, CLAUDE_CODE_CREDENTIAL_ROOTS,
    CLAUDE_CODE_EXECUTABLE_COMMAND_ROOTS, CLAUDE_CODE_MANAGED_ENV_KEYS,
    CLAUDE_CODE_MANAGED_UNSUPPORTED_ROOTS, CLAUDE_CODE_PERMISSION_ROOTS, CLAUDE_CODE_POLICY_ROOTS,
    CODEX_AUTH_ROOTS, CODEX_MANAGED_UNSUPPORTED_ROOTS, CODEX_PERMISSION_ROOTS,
    GOOSE_MANAGED_UNSUPPORTED_ROOTS, GOOSE_PERMISSION_ROOTS, OPENCODE_MANAGED_UNSUPPORTED_ROOTS,
    OPENCODE_PERMISSION_ROOTS, OPENCODE_POLICY_ROOTS, PI_EXECUTABLE_COMMAND_ROOTS,
    PI_EXECUTABLE_PLUGIN_ROOTS, PI_PERMISSION_ROOTS,
};
use crate::runtime::agent::mcp::validate_mcp_secret_refs;
use crate::runtime::agent::provider_keys::{
    agent_provider_id_for_provider_id, apply_catalog_mapped_agent_provider,
    apply_mapped_agent_provider, canonical_provider_id_for_agent_native_id,
};
use crate::runtime::install::agent_registry::RegistryCatalog;
use crate::secrets::SecretStore;

pub const INSPECTION_TTL_SECONDS: u64 = 15 * 60;
pub const MAX_MANIFEST_PATHS: usize = 256;
pub const APPLIED_ROLLBACK_RETENTION_SECONDS: u64 = 15 * 60;
// Terminal records outlive the rollback window so a temporarily unavailable
// API consumer (the platform reconciler polls every 30s) can still observe the
// outcome long after cancel-of-applied has expired.
pub const TERMINAL_RETENTION_SECONDS: u64 = 24 * 60 * 60;
const JOURNAL_DIR_NAME: &str = "native-config-imports";
const JOURNAL_FILE_LIMIT: usize = (IMPORT_SIZE_LIMIT * 4) + (256 * 1024);
const CREDENTIAL_PATH_SEGMENT_PREFIXES: [&str; 14] = [
    "sk-",
    "pk-",
    "rk-",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "github_pat_",
    "glpat-",
    "xoxb-",
    "xoxp-",
    "xoxa-",
    "xoxs-",
];

static OPERATION_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum NativeConfigFormat {
    Json,
    Jsonc,
    Toml,
    Yaml,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ManagedFieldKind {
    Mcp,
    Model,
    Provider,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum BlockedReason {
    Credentials,
    AuthenticationState,
    Permissions,
    Sandbox,
    AcpsPolicy,
    ManagedUnsupported,
    McpUnmappable,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExecutableCategory {
    Hooks,
    Notifications,
    CommandHelpers,
    Plugins,
    Formatters,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedField {
    pub id: String,
    pub path: String,
    pub kind: ManagedFieldKind,
    pub compatible: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BlockedField {
    pub path: String,
    pub reason: BlockedReason,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NativeConfigInspection {
    pub revision: String,
    pub harness: String,
    pub format: NativeConfigFormat,
    pub size_bytes: usize,
    pub managed_fields: Vec<ManagedField>,
    pub blocked_fields: Vec<BlockedField>,
    pub unmanaged_field_paths: Vec<String>,
    pub executable_categories: Vec<ExecutableCategory>,
    pub warnings: Vec<String>,
}

#[derive(Clone)]
pub(super) enum CandidateValue {
    Provider(String),
    Model {
        value: String,
        provider_hint: Option<String>,
    },
    Mcp(McpServerConfig),
}

/// Sensitive parsed draft retained only inside the instance process. It has no
/// `Debug` or serialization implementation, so it cannot enter logs or events.
#[derive(Clone)]
pub struct InspectedNativeConfig {
    inspection: NativeConfigInspection,
    residual: Vec<u8>,
    candidates: BTreeMap<String, CandidateValue>,
    executable_candidate_ids: BTreeSet<String>,
    residual_has_executable: bool,
}

impl InspectedNativeConfig {
    pub fn inspection(&self) -> &NativeConfigInspection {
        &self.inspection
    }

    pub fn revision(&self) -> &str {
        &self.inspection.revision
    }

    pub fn harness(&self) -> &str {
        &self.inspection.harness
    }

    pub fn residual(&self) -> &[u8] {
        &self.residual
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NativeConfigSelection {
    pub revision: String,
    #[serde(default)]
    pub selected_managed_field_ids: Vec<String>,
    #[serde(default)]
    pub executable_settings_acknowledged: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct NativeConfigImportRequest {
    pub revision: String,
    #[serde(default)]
    pub selected_managed_field_ids: Vec<String>,
    #[serde(default)]
    pub executable_settings_acknowledged: bool,
}

impl NativeConfigImportRequest {
    pub fn selection(&self) -> NativeConfigSelection {
        NativeConfigSelection {
            revision: self.revision.clone(),
            selected_managed_field_ids: self.selected_managed_field_ids.clone(),
            executable_settings_acknowledged: self.executable_settings_acknowledged,
        }
    }
}

#[derive(Clone)]
pub struct PreparedNativeConfigImport {
    pub revision: String,
    pub transaction_fingerprint: String,
    pub base_config_revision: String,
    pub harness: String,
    pub canonical_config: Config,
    pub canonical_toml: String,
    pub native_path: PathBuf,
    pub native_content: Vec<u8>,
    pub imported_model: bool,
    pub selected_managed_field_ids: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum NativeConfigOperationStatus {
    Applied,
    Queued,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NativeConfigProjection {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NativeConfigRestartMetadata {
    pub required: bool,
    pub queued: bool,
    pub restarted: bool,
    pub target_id: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NativeConfigOperationError {
    pub code: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NativeConfigOperation {
    pub operation_id: String,
    pub status: NativeConfigOperationStatus,
    pub harness: String,
    pub revision: String,
    pub agent_config: NativeConfigProjection,
    pub restart: NativeConfigRestartMetadata,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<NativeConfigOperationError>,
}

#[derive(Clone)]
pub struct NativeConfigPathSnapshot {
    path: PathBuf,
    content: NativeConfigSnapshotContent,
}

#[derive(Clone)]
enum NativeConfigSnapshotContent {
    File(Option<Vec<u8>>),
    ClaudeOnboarding {
        file_existed: bool,
        value: Option<bool>,
    },
}

#[derive(Clone)]
pub struct NativeConfigFileDigest {
    pub path: PathBuf,
    pub sha256: Option<String>,
}

#[derive(Clone)]
pub struct NativeConfigOperationRecord {
    pub operation: NativeConfigOperation,
    pub transaction_fingerprint: String,
    pub prepared: Option<PreparedNativeConfigImport>,
    pub rollback_snapshots: Vec<NativeConfigPathSnapshot>,
    pub prior_config: Option<Config>,
    pub prior_was_running: bool,
    pub applied_file_digests: Vec<NativeConfigFileDigest>,
    pub applied_at: Option<chrono::DateTime<chrono::Utc>>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub cancelled: bool,
    pub phase: NativeConfigOperationPhase,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeConfigOperationPhase {
    Staged,
    Applying,
    Applied,
    CancellingQueued,
    RollingBack,
    Terminal,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DurablePreparedImport {
    revision: String,
    harness: String,
    base_config_revision: String,
    canonical_toml: String,
    native_content_base64: String,
    imported_model: bool,
    selected_managed_field_ids: Vec<String>,
}

#[derive(Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum DurableSnapshotKind {
    Canonical,
    Native,
    ClaudeState,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DurableSnapshot {
    kind: DurableSnapshotKind,
    content: DurableSnapshotContent,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum DurableSnapshotContent {
    File {
        content_base64: Option<String>,
    },
    ClaudeOnboarding {
        file_existed: bool,
        value: Option<bool>,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DurableFileDigest {
    kind: DurableSnapshotKind,
    sha256: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DurableOperationRecord {
    operation: NativeConfigOperation,
    transaction_fingerprint: String,
    prepared: Option<DurablePreparedImport>,
    rollback_snapshots: Vec<DurableSnapshot>,
    prior_config_toml: Option<String>,
    prior_was_running: bool,
    applied_file_digests: Vec<DurableFileDigest>,
    applied_at: Option<chrono::DateTime<chrono::Utc>>,
    updated_at: chrono::DateTime<chrono::Utc>,
    cancelled: bool,
    phase: NativeConfigOperationPhase,
}

struct NativeConfigDraftRecord {
    inspected: InspectedNativeConfig,
    expires_at: Instant,
}

#[derive(Default)]
pub struct NativeConfigImportState {
    drafts: HashMap<String, NativeConfigDraftRecord>,
    pub operations: HashMap<String, NativeConfigOperationRecord>,
}

mod inspect;
mod journal;
mod mcp;
mod serde_utils;
mod transaction;

// Cross-seam helpers keep `pub(super)` visibility; re-import them here so each
// sibling's `use super::*;` resolves items defined in the other siblings.
use self::inspect::*;
use self::mcp::*;
use self::serde_utils::*;

pub use self::journal::{
    load_native_config_operation_journal, next_native_config_operation_id,
    persist_native_config_operation, remove_native_config_operation_journal,
};
pub use self::transaction::{
    capture_native_config_file_digests, capture_native_config_snapshots, native_config_path,
    native_config_projection, native_config_transaction_paths, prepare_native_config_file_paths,
    restore_native_config_snapshots, validate_native_config_file_digests,
    validate_native_config_secret_refs, validate_native_config_secret_refs_read_only,
    write_native_config_files,
};

pub fn inspect_native_config(
    harness: &str,
    filename: Option<&str>,
    content: &str,
) -> Result<InspectedNativeConfig> {
    let filename = filename.ok_or_else(|| native_error("native_config_filename_required"))?;
    validate_native_config_filename(harness, filename)?;
    if content.is_empty() {
        return Err(native_error("native_config_invalid"));
    }
    if content.len() > IMPORT_SIZE_LIMIT {
        return Err(native_error("native_config_too_large"));
    }
    let revision = sha256_hex(content.as_bytes());
    match harness {
        "claude-code" => inspect_claude(content, revision),
        "codex" => inspect_codex(content, revision),
        "opencode" => inspect_opencode(content, Some(filename), revision),
        "amp" => inspect_amp(content, revision),
        "pi" => inspect_pi(content, revision),
        "goose" => inspect_goose(content, revision),
        _ => Err(native_error("native_config_harness_unsupported")),
    }
}

fn validate_native_config_filename(harness: &str, filename: &str) -> Result<()> {
    let accepted = match harness {
        "claude-code" => filename == "settings.json",
        "codex" => filename == "config.toml",
        "opencode" => matches!(filename, "opencode.json" | "opencode.jsonc"),
        "amp" => filename == "settings.json",
        // Only `settings.json` is accepted: `models.json`/`auth.json` carry
        // literal credentials and `!shell-command` exec semantics acps must
        // not import, and `trust.json`/`mcp.json` are out of scope by design.
        "pi" => filename == "settings.json",
        // Only `config.yaml` is accepted: `secrets.yaml` holds keyring-fallback
        // API keys and `permission.yaml` carries per-tool approval levels, both
        // of which acps must never import.
        "goose" => filename == "config.yaml",
        _ => return Err(native_error("native_config_harness_unsupported")),
    };
    if !accepted {
        return Err(native_error("native_config_filename_unsupported"));
    }
    Ok(())
}

pub fn prepare_native_config_import(
    inspected: &InspectedNativeConfig,
    selection: &NativeConfigSelection,
    current: &Config,
    home: &Path,
) -> Result<PreparedNativeConfigImport> {
    validate_native_config_selection(inspected, selection)?;
    if inspected.harness() != current.agent.id {
        return Err(native_error("native_config_harness_mismatch"));
    }

    let base_config_revision = sha256_hex(current.to_canonical_toml()?.as_bytes());
    let mut candidate = current.clone();
    for id in &selection.selected_managed_field_ids {
        if let Some(CandidateValue::Provider(provider)) = inspected.candidates.get(id) {
            if candidate.agent.providers.is_some() {
                let registry = RegistryCatalog::load_embedded()?;
                let entry = registry.lookup_required(&candidate.agent.id)?;
                apply_catalog_mapped_agent_provider(
                    &mut candidate.agent,
                    provider,
                    entry.multiple_active_providers,
                )
                .map_err(|_| native_error("native_config_provider_unsupported"))?;
            } else {
                apply_mapped_agent_provider(&mut candidate, provider, None)
                    .map_err(|_| native_error("native_config_provider_unsupported"))?;
            }
        }
    }
    let mut imported_model = false;
    for id in &selection.selected_managed_field_ids {
        match inspected.candidates.get(id) {
            Some(CandidateValue::Model {
                value,
                provider_hint,
            }) => {
                if let Some(provider_hint) = provider_hint {
                    let effective_provider = candidate
                        .agent
                        .provider
                        .as_ref()
                        .ok_or_else(|| native_error("native_config_model_provider_mismatch"))?;
                    let effective_native = agent_provider_id_for_provider_id(
                        &candidate.agent.id,
                        &effective_provider.id,
                    )
                    .unwrap_or(&effective_provider.id);
                    if effective_native != provider_hint {
                        return Err(native_error("native_config_model_provider_mismatch"));
                    }
                }
                apply_model(&mut candidate, value);
                imported_model = true;
            }
            Some(CandidateValue::Mcp(server)) => apply_mcp(&mut candidate, server.clone()),
            Some(CandidateValue::Provider(_)) => {}
            None => return Err(native_error("native_config_selection_invalid")),
        }
    }

    let canonical_toml = candidate.to_canonical_toml()?;
    let canonical_config = crate::config::load_config_from_str(&canonical_toml)?;
    let mut selected_managed_field_ids = selection.selected_managed_field_ids.clone();
    selected_managed_field_ids.sort();
    let transaction_fingerprint = native_config_transaction_fingerprint(
        inspected.harness(),
        &canonical_toml,
        inspected.residual(),
        &selected_managed_field_ids,
    );
    Ok(PreparedNativeConfigImport {
        revision: selection.revision.clone(),
        transaction_fingerprint,
        base_config_revision,
        harness: inspected.harness().to_owned(),
        canonical_config,
        canonical_toml,
        native_path: native_config_path(inspected.harness(), home)?,
        native_content: inspected.residual.clone(),
        imported_model,
        selected_managed_field_ids,
    })
}

pub fn rebase_prepared_native_config_import(
    prepared: &mut PreparedNativeConfigImport,
    current: &Config,
) -> Result<()> {
    if current.agent.id != prepared.harness {
        return Err(native_error("native_config_harness_mismatch"));
    }
    let imported = prepared.canonical_config.clone();
    let mut candidate = current.clone();
    if prepared
        .selected_managed_field_ids
        .iter()
        .any(|id| id == "provider")
    {
        candidate.agent.provider = imported.agent.provider.clone();
        candidate.agent.providers = imported.agent.providers.clone();
        candidate.agent.model = None;
        for entry in &imported.agent.env {
            let var_name = crate::config::env_entry_var_name(entry);
            if !crate::config::agent_env_declares(&candidate.agent.env, var_name) {
                candidate.agent.env.push(entry.clone());
            }
        }
    }
    if prepared
        .selected_managed_field_ids
        .iter()
        .any(|id| id == "model")
    {
        let model = native_config_projection(&imported)
            .model
            .ok_or_else(|| native_error("native_config_model_invalid"))?;
        apply_model(&mut candidate, &model);
    }
    for id in &prepared.selected_managed_field_ids {
        let Some(name) = id.strip_prefix("mcp:") else {
            continue;
        };
        let server = imported
            .mcp
            .servers
            .iter()
            .find(|server| server.name() == name)
            .cloned()
            .ok_or_else(|| native_error("native_config_selection_invalid"))?;
        apply_mcp(&mut candidate, server);
    }
    let canonical_toml = candidate.to_canonical_toml()?;
    prepared.base_config_revision = sha256_hex(current.to_canonical_toml()?.as_bytes());
    prepared.transaction_fingerprint = native_config_transaction_fingerprint(
        &prepared.harness,
        &canonical_toml,
        &prepared.native_content,
        &prepared.selected_managed_field_ids,
    );
    prepared.canonical_config = crate::config::load_config_from_str(&canonical_toml)?;
    prepared.canonical_toml = canonical_toml;
    Ok(())
}

fn native_config_transaction_fingerprint(
    harness: &str,
    canonical_toml: &str,
    native_content: &[u8],
    selected_managed_field_ids: &[String],
) -> String {
    sha256_hex(
        [
            harness.as_bytes(),
            b"\0",
            canonical_toml.as_bytes(),
            b"\0",
            selected_managed_field_ids.join("\0").as_bytes(),
            b"\0",
            native_content,
        ]
        .concat()
        .as_slice(),
    )
}

pub fn validate_native_config_selection(
    inspected: &InspectedNativeConfig,
    selection: &NativeConfigSelection,
) -> Result<()> {
    if selection.revision != inspected.revision() {
        return Err(native_error("native_config_revision_mismatch"));
    }
    if selection.selected_managed_field_ids.len() > MAX_MANIFEST_PATHS {
        return Err(native_error("native_config_selection_invalid"));
    }
    let selected_executable_candidate = selection
        .selected_managed_field_ids
        .iter()
        .any(|id| inspected.executable_candidate_ids.contains(id));
    if (inspected.residual_has_executable || selected_executable_candidate)
        && !selection.executable_settings_acknowledged
    {
        return Err(native_error("native_config_executable_ack_required"));
    }
    let mut selected = HashSet::new();
    for id in &selection.selected_managed_field_ids {
        if !selected.insert(id.as_str()) {
            return Err(native_error("native_config_selection_invalid"));
        }
        let Some(field) = inspected
            .inspection
            .managed_fields
            .iter()
            .find(|field| field.id == *id)
        else {
            return Err(native_error("native_config_selection_invalid"));
        };
        if !field.compatible {
            return Err(native_error("native_config_selection_invalid"));
        }
    }

    Ok(())
}

fn apply_model(config: &mut Config, model: &str) {
    if let Some(provider) = config.agent.provider.as_mut() {
        provider.model = Some(model.to_owned());
        config.agent.model = None;
    } else {
        config.agent.model = Some(model.to_owned());
    }
}

fn apply_mcp(config: &mut Config, server: McpServerConfig) {
    if let Some(existing) = config
        .mcp
        .servers
        .iter_mut()
        .find(|existing| existing.name() == server.name())
    {
        *existing = server;
    } else {
        config.mcp.servers.push(server);
    }
}

pub(crate) fn sha256_hex(content: &[u8]) -> String {
    let digest = Sha256::digest(content);
    format!("{digest:x}")
}

fn native_error(code: &'static str) -> StackError {
    StackError::NativeAgentConfig { code }
}

#[cfg(test)]
mod tests;
