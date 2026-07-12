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
use crate::runtime::agent::mcp::resolve_mcp_servers;
use crate::runtime::agent::provider_keys::{
    agent_provider_id_for_provider_id, apply_mapped_agent_provider,
    canonical_provider_id_for_agent_native_id,
};
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
enum CandidateValue {
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

impl NativeConfigImportState {
    pub fn inspect(
        &mut self,
        harness: &str,
        filename: Option<&str>,
        content: &str,
    ) -> Result<NativeConfigInspection> {
        self.prune();
        let inspected = inspect_native_config(harness, filename, content)?;
        let inspection = inspected.inspection().clone();
        self.drafts.insert(
            inspection.revision.clone(),
            NativeConfigDraftRecord {
                inspected,
                expires_at: Instant::now() + Duration::from_secs(INSPECTION_TTL_SECONDS),
            },
        );
        Ok(inspection)
    }

    pub fn prepare(
        &mut self,
        selection: &NativeConfigSelection,
        current: &Config,
        home: &Path,
    ) -> Result<PreparedNativeConfigImport> {
        self.prune();
        let draft = self
            .drafts
            .get(&selection.revision)
            .ok_or_else(|| native_error("native_config_inspection_expired"))?;
        prepare_native_config_import(&draft.inspected, selection, current, home)
    }

    pub fn insert_operation(&mut self, record: NativeConfigOperationRecord) {
        self.prune();
        self.operations
            .insert(record.operation.operation_id.clone(), record);
    }

    pub fn operation_for_fingerprint(
        &mut self,
        transaction_fingerprint: &str,
    ) -> Option<NativeConfigOperationRecord> {
        self.prune();
        let latest_applied = self
            .operations
            .values()
            .filter(|record| record.phase == NativeConfigOperationPhase::Applied)
            .filter_map(|record| {
                record
                    .applied_at
                    .map(|applied_at| (applied_at, record.operation.operation_id.as_str()))
            })
            .max()
            .map(|(_, operation_id)| operation_id.to_owned());
        self.operations
            .values()
            .find(|record| {
                record.transaction_fingerprint == transaction_fingerprint
                    && (record.operation.status == NativeConfigOperationStatus::Queued
                        || (record.operation.status == NativeConfigOperationStatus::Applied
                            && latest_applied.as_deref()
                                == Some(record.operation.operation_id.as_str())))
            })
            .cloned()
    }

    pub fn operation(&mut self, operation_id: &str) -> Option<NativeConfigOperation> {
        self.prune();
        self.operations
            .get(operation_id)
            .map(|record| record.operation.clone())
    }

    fn prune(&mut self) {
        let instant_now = Instant::now();
        let utc_now = chrono::Utc::now();
        self.drafts
            .retain(|_, draft| draft.expires_at > instant_now);
        self.operations.retain(|_, record| {
            matches!(
                record.phase,
                NativeConfigOperationPhase::Staged
                    | NativeConfigOperationPhase::Applying
                    | NativeConfigOperationPhase::CancellingQueued
                    | NativeConfigOperationPhase::RollingBack
            ) || utc_now
                .signed_duration_since(record.updated_at)
                .num_seconds()
                < TERMINAL_RETENTION_SECONDS as i64
        });
    }
}

pub fn persist_native_config_operation(
    state_path: &Path,
    config_path: &Path,
    home: &Path,
    record: &NativeConfigOperationRecord,
) -> Result<()> {
    let journal_dir = native_config_journal_dir(state_path)?;
    create_dir_owner_only(&journal_dir)?;
    let path = native_config_journal_path(&journal_dir, &record.operation.operation_id)?;
    prepare_owner_managed_file_path(&journal_dir, &path)?;
    let native_path = native_config_path(&record.operation.harness, home)?;
    let prepared = record
        .prepared
        .as_ref()
        .map(|prepared| DurablePreparedImport {
            revision: prepared.revision.clone(),
            harness: prepared.harness.clone(),
            base_config_revision: prepared.base_config_revision.clone(),
            canonical_toml: prepared.canonical_toml.clone(),
            native_content_base64: base64::engine::general_purpose::STANDARD
                .encode(&prepared.native_content),
            imported_model: prepared.imported_model,
            selected_managed_field_ids: prepared.selected_managed_field_ids.clone(),
        });
    let rollback_snapshots = record
        .rollback_snapshots
        .iter()
        .map(|snapshot| {
            let kind = snapshot_kind_for_path(
                &snapshot.path,
                config_path,
                &native_path,
                &record.operation.harness,
                home,
            )?;
            let content = match (&snapshot.content, kind) {
                (NativeConfigSnapshotContent::File(content), kind)
                    if kind != DurableSnapshotKind::ClaudeState =>
                {
                    DurableSnapshotContent::File {
                        content_base64: content.as_ref().map(|content| {
                            base64::engine::general_purpose::STANDARD.encode(content)
                        }),
                    }
                }
                (
                    NativeConfigSnapshotContent::ClaudeOnboarding {
                        file_existed,
                        value,
                    },
                    DurableSnapshotKind::ClaudeState,
                ) => DurableSnapshotContent::ClaudeOnboarding {
                    file_existed: *file_existed,
                    value: *value,
                },
                _ => return Err(native_error("native_config_journal_invalid")),
            };
            Ok(DurableSnapshot { kind, content })
        })
        .collect::<Result<Vec<_>>>()?;
    let applied_file_digests = record
        .applied_file_digests
        .iter()
        .map(|digest| {
            Ok(DurableFileDigest {
                kind: snapshot_kind_for_path(
                    &digest.path,
                    config_path,
                    &native_path,
                    &record.operation.harness,
                    home,
                )?,
                sha256: digest.sha256.clone(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let prior_config_toml = record
        .prior_config
        .as_ref()
        .map(Config::to_canonical_toml)
        .transpose()?;
    let durable = DurableOperationRecord {
        operation: record.operation.clone(),
        transaction_fingerprint: record.transaction_fingerprint.clone(),
        prepared,
        rollback_snapshots,
        prior_config_toml,
        prior_was_running: record.prior_was_running,
        applied_file_digests,
        applied_at: record.applied_at,
        updated_at: record.updated_at,
        cancelled: record.cancelled,
        phase: record.phase,
    };
    let content =
        serde_json::to_vec(&durable).map_err(|_| native_error("native_config_journal_invalid"))?;
    if content.len() > JOURNAL_FILE_LIMIT {
        return Err(native_error("native_config_journal_too_large"));
    }
    atomic_write_owner_only(&path, &content)
}

pub fn remove_native_config_operation_journal(state_path: &Path, operation_id: &str) -> Result<()> {
    let journal_dir = native_config_journal_dir(state_path)?;
    if !journal_dir.exists() {
        return Ok(());
    }
    let path = native_config_journal_path(&journal_dir, operation_id)?;
    prepare_owner_managed_file_path(&journal_dir, &path)?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(StackError::FileRemove { path, source }),
    }
}

pub fn load_native_config_operation_journal(
    state_path: &Path,
    config_path: &Path,
    home: &Path,
) -> Result<Vec<NativeConfigOperationRecord>> {
    let journal_dir = native_config_journal_dir(state_path)?;
    create_dir_owner_only(&journal_dir)?;
    let mut records = Vec::new();
    for entry in std::fs::read_dir(&journal_dir).map_err(|source| StackError::DirectoryCreate {
        path: journal_dir.clone(),
        source,
    })? {
        let entry = entry.map_err(|source| StackError::DirectoryCreate {
            path: journal_dir.clone(),
            source,
        })?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        if records.len() >= MAX_MANIFEST_PATHS {
            return Err(native_error("native_config_journal_too_many"));
        }
        prepare_owner_managed_file_path(&journal_dir, &path)?;
        let metadata = std::fs::metadata(&path).map_err(|source| StackError::ConfigRead {
            path: path.clone(),
            source,
        })?;
        if metadata.len() > JOURNAL_FILE_LIMIT as u64 {
            return Err(native_error("native_config_journal_too_large"));
        }
        let content = std::fs::read(&path).map_err(|source| StackError::ConfigRead {
            path: path.clone(),
            source,
        })?;
        let durable: DurableOperationRecord = serde_json::from_slice(&content)
            .map_err(|_| native_error("native_config_journal_invalid"))?;
        let expected_id = path.file_stem().and_then(|value| value.to_str());
        if expected_id != Some(durable.operation.operation_id.as_str()) {
            return Err(native_error("native_config_journal_invalid"));
        }
        let pending_recovery = matches!(
            durable.phase,
            NativeConfigOperationPhase::Staged
                | NativeConfigOperationPhase::Applying
                | NativeConfigOperationPhase::CancellingQueued
                | NativeConfigOperationPhase::RollingBack
        );
        let expired = chrono::Utc::now()
            .signed_duration_since(durable.updated_at)
            .num_seconds()
            >= TERMINAL_RETENTION_SECONDS as i64;
        if !pending_recovery && expired {
            std::fs::remove_file(&path).map_err(|source| StackError::FileRemove {
                path: path.clone(),
                source,
            })?;
            continue;
        }
        records.push(inflate_durable_record(durable, config_path, home)?);
    }
    Ok(records)
}

fn inflate_durable_record(
    durable: DurableOperationRecord,
    config_path: &Path,
    home: &Path,
) -> Result<NativeConfigOperationRecord> {
    let native_path = native_config_path(&durable.operation.harness, home)?;
    let prepared = durable
        .prepared
        .map(|prepared| {
            if prepared.revision != durable.operation.revision
                || prepared.harness != durable.operation.harness
            {
                return Err(native_error("native_config_journal_invalid"));
            }
            let native_content = base64::engine::general_purpose::STANDARD
                .decode(prepared.native_content_base64)
                .map_err(|_| native_error("native_config_journal_invalid"))?;
            if native_content.len() > IMPORT_SIZE_LIMIT {
                return Err(native_error("native_config_journal_too_large"));
            }
            let canonical_config = crate::config::load_config_from_str(&prepared.canonical_toml)?;
            if canonical_config.agent.id != prepared.harness {
                return Err(native_error("native_config_journal_invalid"));
            }
            let transaction_fingerprint = native_config_transaction_fingerprint(
                &prepared.harness,
                &prepared.canonical_toml,
                &native_content,
                &prepared.selected_managed_field_ids,
            );
            if transaction_fingerprint != durable.transaction_fingerprint {
                return Err(native_error("native_config_journal_invalid"));
            }
            Ok(PreparedNativeConfigImport {
                revision: prepared.revision,
                transaction_fingerprint,
                base_config_revision: prepared.base_config_revision,
                harness: prepared.harness,
                canonical_config,
                canonical_toml: prepared.canonical_toml,
                native_path: native_path.clone(),
                native_content,
                imported_model: prepared.imported_model,
                selected_managed_field_ids: prepared.selected_managed_field_ids,
            })
        })
        .transpose()?;
    if matches!(
        durable.operation.status,
        NativeConfigOperationStatus::Queued
    ) && prepared.is_none()
    {
        return Err(native_error("native_config_journal_invalid"));
    }
    let rollback_snapshots = durable
        .rollback_snapshots
        .into_iter()
        .map(|snapshot| {
            let content = match (snapshot.content, snapshot.kind) {
                (DurableSnapshotContent::File { content_base64 }, kind)
                    if kind != DurableSnapshotKind::ClaudeState =>
                {
                    NativeConfigSnapshotContent::File(
                        content_base64
                            .map(|content| {
                                base64::engine::general_purpose::STANDARD
                                    .decode(content)
                                    .map_err(|_| native_error("native_config_journal_invalid"))
                            })
                            .transpose()?,
                    )
                }
                (
                    DurableSnapshotContent::ClaudeOnboarding {
                        file_existed,
                        value,
                    },
                    DurableSnapshotKind::ClaudeState,
                ) => NativeConfigSnapshotContent::ClaudeOnboarding {
                    file_existed,
                    value,
                },
                _ => return Err(native_error("native_config_journal_invalid")),
            };
            Ok(NativeConfigPathSnapshot {
                path: path_for_snapshot_kind(
                    snapshot.kind,
                    config_path,
                    &native_path,
                    &durable.operation.harness,
                    home,
                )?,
                content,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let applied_file_digests = durable
        .applied_file_digests
        .into_iter()
        .map(|digest| {
            Ok(NativeConfigFileDigest {
                path: path_for_snapshot_kind(
                    digest.kind,
                    config_path,
                    &native_path,
                    &durable.operation.harness,
                    home,
                )?,
                sha256: digest.sha256,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let prior_config = durable
        .prior_config_toml
        .as_deref()
        .map(crate::config::load_config_from_str)
        .transpose()?;
    let phase_valid = match durable.phase {
        NativeConfigOperationPhase::Staged => {
            durable.operation.status == NativeConfigOperationStatus::Queued && prepared.is_some()
        }
        NativeConfigOperationPhase::Applying => {
            durable.operation.status == NativeConfigOperationStatus::Queued
                && prepared.is_some()
                && !rollback_snapshots.is_empty()
                && prior_config.is_some()
        }
        NativeConfigOperationPhase::Applied => {
            durable.operation.status == NativeConfigOperationStatus::Applied
                && prepared.is_none()
                && !rollback_snapshots.is_empty()
                && prior_config.is_some()
                && !applied_file_digests.is_empty()
        }
        NativeConfigOperationPhase::CancellingQueued => {
            durable.operation.status == NativeConfigOperationStatus::Queued
                && durable.cancelled
                && prepared.is_some()
        }
        NativeConfigOperationPhase::RollingBack => {
            matches!(
                durable.operation.status,
                NativeConfigOperationStatus::Queued
                    | NativeConfigOperationStatus::Applied
                    | NativeConfigOperationStatus::Failed
            ) && !rollback_snapshots.is_empty()
                && prior_config.is_some()
        }
        NativeConfigOperationPhase::Terminal => {
            matches!(
                durable.operation.status,
                NativeConfigOperationStatus::Applied
                    | NativeConfigOperationStatus::Failed
                    | NativeConfigOperationStatus::Cancelled
            ) && prepared.is_none()
                && rollback_snapshots.is_empty()
                && prior_config.is_none()
                && applied_file_digests.is_empty()
        }
    };
    if !phase_valid {
        return Err(native_error("native_config_journal_invalid"));
    }
    Ok(NativeConfigOperationRecord {
        operation: durable.operation,
        transaction_fingerprint: durable.transaction_fingerprint,
        prepared,
        rollback_snapshots,
        prior_config,
        prior_was_running: durable.prior_was_running,
        applied_file_digests,
        applied_at: durable.applied_at,
        updated_at: durable.updated_at,
        cancelled: durable.cancelled,
        phase: durable.phase,
    })
}

fn native_config_journal_dir(state_path: &Path) -> Result<PathBuf> {
    Ok(parent_dir(state_path)?.join(JOURNAL_DIR_NAME))
}

fn native_config_journal_path(journal_dir: &Path, operation_id: &str) -> Result<PathBuf> {
    if operation_id.is_empty()
        || operation_id.len() > 128
        || !operation_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return Err(native_error("native_config_operation_invalid"));
    }
    Ok(journal_dir.join(format!("{operation_id}.json")))
}

fn snapshot_kind_for_path(
    path: &Path,
    config_path: &Path,
    native_path: &Path,
    harness: &str,
    home: &Path,
) -> Result<DurableSnapshotKind> {
    if path == config_path {
        Ok(DurableSnapshotKind::Canonical)
    } else if path == native_path {
        Ok(DurableSnapshotKind::Native)
    } else if harness == "claude-code" && path == home.join(".claude.json") {
        Ok(DurableSnapshotKind::ClaudeState)
    } else {
        Err(native_error("native_config_journal_invalid"))
    }
}

fn path_for_snapshot_kind(
    kind: DurableSnapshotKind,
    config_path: &Path,
    native_path: &Path,
    harness: &str,
    home: &Path,
) -> Result<PathBuf> {
    match kind {
        DurableSnapshotKind::Canonical => Ok(config_path.to_path_buf()),
        DurableSnapshotKind::Native => Ok(native_path.to_path_buf()),
        DurableSnapshotKind::ClaudeState if harness == "claude-code" => {
            Ok(home.join(".claude.json"))
        }
        DurableSnapshotKind::ClaudeState => Err(native_error("native_config_journal_invalid")),
    }
}

pub fn next_native_config_operation_id() -> String {
    let sequence = OPERATION_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0).max(0) as u128;
    format!("nci_{nanos:020}_{sequence:010}_{:010}", std::process::id())
}

pub fn native_config_projection(config: &Config) -> NativeConfigProjection {
    NativeConfigProjection {
        id: config.agent.id.clone(),
        provider: config
            .agent
            .provider
            .as_ref()
            .map(|provider| provider.id.clone()),
        model: config
            .agent
            .provider
            .as_ref()
            .and_then(|provider| provider.model.clone())
            .or_else(|| config.agent.model.clone()),
    }
}

pub fn native_config_path(harness: &str, home: &Path) -> Result<PathBuf> {
    match harness {
        "claude-code" => Ok(home.join(".claude").join("settings.json")),
        "codex" => Ok(home.join(".codex").join("config.toml")),
        "opencode" => Ok(home.join(".config").join("opencode").join("opencode.json")),
        "amp" => Ok(home.join(".config").join("amp").join("settings.json")),
        "pi" => Ok(home.join(".pi").join("agent").join("settings.json")),
        "goose" => Ok(home.join(".config").join("goose").join("config.yaml")),
        _ => Err(native_error("native_config_harness_unsupported")),
    }
}

pub fn validate_native_config_secret_refs_read_only(
    prepared: &PreparedNativeConfigImport,
    home: &Path,
) -> Result<()> {
    let secrets = SecretStore::open_read_only(home)?;
    validate_native_config_secret_refs_with_store(prepared, &secrets)
}

pub fn validate_native_config_secret_refs(
    prepared: &PreparedNativeConfigImport,
    home: &Path,
) -> Result<()> {
    let secrets = SecretStore::open(home)?;
    validate_native_config_secret_refs_with_store(prepared, &secrets)
}

fn validate_native_config_secret_refs_with_store(
    prepared: &PreparedNativeConfigImport,
    secrets: &SecretStore,
) -> Result<()> {
    for name in &prepared.canonical_config.agent.env {
        secrets.get(name)?;
    }
    resolve_mcp_servers(&prepared.canonical_config.mcp, secrets)?;
    Ok(())
}

pub fn native_config_transaction_paths(
    config_path: &Path,
    native_path: &Path,
    harness: &str,
    home: &Path,
) -> Vec<PathBuf> {
    let mut paths = vec![config_path.to_path_buf(), native_path.to_path_buf()];
    if harness == "claude-code" {
        paths.push(home.join(".claude.json"));
    }
    paths.sort();
    paths.dedup();
    paths
}

pub fn prepare_native_config_file_paths(
    prepared: &PreparedNativeConfigImport,
    config_path: &Path,
    home: &Path,
) -> Result<Vec<PathBuf>> {
    let paths = native_config_transaction_paths(
        config_path,
        &prepared.native_path,
        &prepared.harness,
        home,
    );
    for path in &paths {
        prepare_owner_managed_file_path(home, path)?;
    }
    Ok(paths)
}

pub fn capture_native_config_snapshots(
    paths: &[PathBuf],
    home: &Path,
) -> Result<Vec<NativeConfigPathSnapshot>> {
    let mut snapshots = Vec::with_capacity(paths.len());
    for path in paths {
        prepare_owner_managed_file_path(home, path)?;
        let content = if path == &home.join(".claude.json") {
            match std::fs::read(path) {
                Ok(content) => {
                    let root = match serde_json::from_slice::<JsonValue>(&content) {
                        Ok(JsonValue::Object(root)) => root,
                        _ => return Err(native_error("native_config_claude_state_invalid")),
                    };
                    let value = match root.get("hasCompletedOnboarding") {
                        Some(JsonValue::Bool(value)) => Some(*value),
                        None => None,
                        Some(_) => {
                            return Err(native_error("native_config_claude_state_invalid"));
                        }
                    };
                    NativeConfigSnapshotContent::ClaudeOnboarding {
                        file_existed: true,
                        value,
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    NativeConfigSnapshotContent::ClaudeOnboarding {
                        file_existed: false,
                        value: None,
                    }
                }
                Err(source) => {
                    return Err(StackError::ConfigRead {
                        path: path.clone(),
                        source,
                    });
                }
            }
        } else {
            let content = match std::fs::read(path) {
                Ok(content) => Some(content),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(source) => {
                    return Err(StackError::ConfigRead {
                        path: path.clone(),
                        source,
                    });
                }
            };
            NativeConfigSnapshotContent::File(content)
        };
        snapshots.push(NativeConfigPathSnapshot {
            path: path.clone(),
            content,
        });
    }
    Ok(snapshots)
}

pub fn restore_native_config_snapshots(
    snapshots: &[NativeConfigPathSnapshot],
    home: &Path,
) -> Result<()> {
    for snapshot in snapshots {
        prepare_owner_managed_file_path(home, &snapshot.path)?;
        match &snapshot.content {
            NativeConfigSnapshotContent::File(Some(content)) => {
                atomic_write_owner_only(&snapshot.path, content)?;
            }
            NativeConfigSnapshotContent::File(None)
            | NativeConfigSnapshotContent::ClaudeOnboarding {
                file_existed: false,
                ..
            } => match std::fs::remove_file(&snapshot.path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(StackError::FileRemove {
                        path: snapshot.path.clone(),
                        source,
                    });
                }
            },
            NativeConfigSnapshotContent::ClaudeOnboarding {
                file_existed: true,
                value,
            } => {
                let content =
                    std::fs::read(&snapshot.path).map_err(|source| StackError::ConfigRead {
                        path: snapshot.path.clone(),
                        source,
                    })?;
                let mut root = match serde_json::from_slice::<JsonValue>(&content) {
                    Ok(JsonValue::Object(root)) => root,
                    _ => return Err(native_error("native_config_claude_state_invalid")),
                };
                match value {
                    Some(value) => {
                        root.insert("hasCompletedOnboarding".to_owned(), JsonValue::Bool(*value));
                    }
                    None => {
                        root.remove("hasCompletedOnboarding");
                    }
                }
                atomic_write_owner_only(&snapshot.path, &json_bytes(root)?)?;
            }
        }
    }
    Ok(())
}

pub fn write_native_config_files(
    prepared: &PreparedNativeConfigImport,
    config_path: &Path,
    home: &Path,
) -> Result<()> {
    atomic_write_owner_only(config_path, prepared.canonical_toml.as_bytes())?;
    atomic_write_owner_only(&prepared.native_path, &prepared.native_content)?;
    provision_agent_headless_config(&prepared.canonical_config, home)?;
    Ok(())
}

pub fn capture_native_config_file_digests(
    paths: &[PathBuf],
    home: &Path,
) -> Result<Vec<NativeConfigFileDigest>> {
    paths
        .iter()
        .map(|path| {
            prepare_owner_managed_file_path(home, path)?;
            let sha256 = native_config_file_digest(path, home)?;
            Ok(NativeConfigFileDigest {
                path: path.clone(),
                sha256,
            })
        })
        .collect()
}

pub fn validate_native_config_file_digests(
    digests: &[NativeConfigFileDigest],
    home: &Path,
) -> Result<()> {
    if digests.is_empty() {
        return Err(native_error("native_config_rollback_conflict"));
    }
    for expected in digests {
        prepare_owner_managed_file_path(home, &expected.path)?;
        let actual = native_config_file_digest(&expected.path, home)?;
        if actual != expected.sha256 {
            return Err(native_error("native_config_rollback_conflict"));
        }
    }
    Ok(())
}

fn native_config_file_digest(path: &Path, home: &Path) -> Result<Option<String>> {
    let content = match std::fs::read(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(StackError::ConfigRead {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    if path != home.join(".claude.json") {
        return Ok(Some(sha256_hex(&content)));
    }
    let root = match serde_json::from_slice::<JsonValue>(&content) {
        Ok(JsonValue::Object(root)) => root,
        _ => return Err(native_error("native_config_claude_state_invalid")),
    };
    let owned_value = match root.get("hasCompletedOnboarding") {
        Some(JsonValue::Bool(true)) => b"true".as_slice(),
        Some(JsonValue::Bool(false)) => b"false".as_slice(),
        None => b"missing".as_slice(),
        Some(_) => return Err(native_error("native_config_claude_state_invalid")),
    };
    Ok(Some(sha256_hex(owned_value)))
}

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
            apply_mapped_agent_provider(&mut candidate, provider, None)
                .map_err(|_| native_error("native_config_provider_unsupported"))?;
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
        candidate.agent.model = None;
        for name in &imported.agent.env {
            if !candidate.agent.env.iter().any(|existing| existing == name) {
                candidate.agent.env.push(name.clone());
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

fn inspect_claude(content: &str, revision: String) -> Result<InspectedNativeConfig> {
    let mut root = parse_json_object(content)?;
    let mut builder = InspectionBuilder::new(
        "claude-code",
        NativeConfigFormat::Json,
        revision,
        content.len(),
    );

    if let Some(value) = root.remove("model") {
        builder.add_string_candidate("model", "model", ManagedFieldKind::Model, value, |value| {
            Some(CandidateValue::Model {
                value: value.to_owned(),
                provider_hint: None,
            })
        });
    }
    for key in CLAUDE_CODE_CREDENTIAL_ROOTS {
        if root.remove(*key).is_some() {
            builder.block(*key, BlockedReason::Credentials);
        }
    }
    for key in CLAUDE_CODE_AUTH_ROOTS {
        if root.remove(*key).is_some() {
            builder.block(*key, BlockedReason::AuthenticationState);
        }
    }
    for key in CLAUDE_CODE_PERMISSION_ROOTS {
        if root.remove(*key).is_some() {
            let reason = if *key == "sandbox" {
                BlockedReason::Sandbox
            } else {
                BlockedReason::Permissions
            };
            builder.block(*key, reason);
        }
    }
    for key in CLAUDE_CODE_POLICY_ROOTS {
        if root.remove(*key).is_some() {
            builder.block(*key, BlockedReason::AcpsPolicy);
        }
    }
    for key in CLAUDE_CODE_MANAGED_UNSUPPORTED_ROOTS {
        if root.remove(*key).is_some() {
            builder.block(*key, BlockedReason::ManagedUnsupported);
        }
    }

    if let Some(env) = root.get_mut("env").and_then(JsonValue::as_object_mut) {
        let mut remove = Vec::new();
        for key in CLAUDE_CODE_MANAGED_ENV_KEYS {
            if let Some(value) = env.get(*key) {
                if *key == "ANTHROPIC_MODEL" && !builder.has_candidate("model") {
                    builder.add_string_candidate(
                        "model",
                        "env.ANTHROPIC_MODEL",
                        ManagedFieldKind::Model,
                        value.clone(),
                        |value| {
                            Some(CandidateValue::Model {
                                value: value.to_owned(),
                                provider_hint: None,
                            })
                        },
                    );
                } else {
                    let reason = if key.contains("TOKEN") || key.contains("API_KEY") {
                        BlockedReason::Credentials
                    } else {
                        BlockedReason::ManagedUnsupported
                    };
                    builder.block(format!("env.{key}"), reason);
                }
                remove.push(*key);
            }
        }
        for key in remove {
            env.remove(key);
        }
        for key in CLAUDE_CODE_CREDENTIAL_ENV_KEYS {
            if env.remove(*key).is_some() {
                builder.block(format!("env.{key}"), BlockedReason::Credentials);
            }
        }
        for key in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"] {
            let contains_credentials = env
                .get(key)
                .and_then(JsonValue::as_str)
                .is_some_and(url_contains_userinfo);
            if contains_credentials {
                env.remove(key);
                builder.block(format!("env.{key}"), BlockedReason::Credentials);
            }
        }
        let sensitive = env
            .keys()
            .filter_map(|key| sensitive_field_reason(key).map(|reason| (key.clone(), reason)))
            .collect::<Vec<_>>();
        for (key, reason) in sensitive {
            env.remove(&key);
            builder.block(format!("env.{key}"), reason);
        }
        if env.keys().any(|key| executable_environment_key(key)) {
            builder.executable(ExecutableCategory::CommandHelpers);
        }
        if env.is_empty() {
            root.remove("env");
        }
    }

    if let Some(mcp) = root.remove("mcpServers") {
        classify_json_mcp(&mut builder, "mcpServers", mcp, JsonMcpDialect::Claude);
    }
    if root.contains_key("hooks") {
        builder.executable(ExecutableCategory::Hooks);
    }
    for key in CLAUDE_CODE_EXECUTABLE_COMMAND_ROOTS {
        if root.contains_key(*key) {
            builder.executable(ExecutableCategory::CommandHelpers);
        }
    }
    if root.contains_key("enabledPlugins") || root.contains_key("extraKnownMarketplaces") {
        builder.executable(ExecutableCategory::Plugins);
    }

    sanitize_sensitive_json_object(&mut root, "", &mut builder);
    let residual = json_bytes(root)?;
    builder.finish_json(residual)
}

fn inspect_opencode(
    content: &str,
    filename: Option<&str>,
    revision: String,
) -> Result<InspectedNativeConfig> {
    let filename_jsonc = filename.is_some_and(|name| name.to_ascii_lowercase().ends_with(".jsonc"));
    let (mut root, normalized_jsonc) = match parse_json_object(content) {
        Ok(root) => (root, filename_jsonc),
        Err(_) => (parse_jsonc_object(content)?, true),
    };
    let format = if normalized_jsonc {
        NativeConfigFormat::Jsonc
    } else {
        NativeConfigFormat::Json
    };
    let mut builder = InspectionBuilder::new("opencode", format, revision, content.len());
    if normalized_jsonc {
        builder.warn("jsonc-normalized");
    }

    if let Some(value) = root.remove("model") {
        if let Some(model) = value.as_str() {
            let (provider, _) = split_opencode_model(model);
            let canonical_provider = provider.and_then(|provider| {
                canonical_provider_id_for_agent_native_id("opencode", provider)
            });
            if let Some(provider) = provider {
                builder.add_candidate(
                    "provider",
                    "model",
                    ManagedFieldKind::Provider,
                    canonical_provider.is_some(),
                    CandidateValue::Provider(canonical_provider.unwrap_or(provider).to_owned()),
                );
            }
            builder.add_candidate(
                "model",
                "model",
                ManagedFieldKind::Model,
                !model.trim().is_empty() && (provider.is_none() || canonical_provider.is_some()),
                CandidateValue::Model {
                    value: model.to_owned(),
                    provider_hint: provider.map(str::to_owned),
                },
            );
        } else {
            builder.incompatible("model", "model", ManagedFieldKind::Model);
        }
    }
    for key in OPENCODE_MANAGED_UNSUPPORTED_ROOTS {
        if root.remove(*key).is_some() {
            builder.block(*key, BlockedReason::ManagedUnsupported);
        }
    }
    for key in OPENCODE_PERMISSION_ROOTS {
        if root.remove(*key).is_some() {
            let reason = if *key == "sandbox" {
                BlockedReason::Sandbox
            } else {
                BlockedReason::Permissions
            };
            builder.block(*key, reason);
        }
    }
    for key in OPENCODE_POLICY_ROOTS {
        if root.remove(*key).is_some() {
            builder.block(*key, BlockedReason::AcpsPolicy);
        }
    }
    if let Some(mcp) = root.remove("mcp") {
        classify_json_mcp(&mut builder, "mcp", mcp, JsonMcpDialect::OpenCode);
    }
    if root.contains_key("plugin") {
        builder.executable(ExecutableCategory::Plugins);
    }
    if root.contains_key("command") {
        builder.executable(ExecutableCategory::CommandHelpers);
    }
    if root.contains_key("formatter") {
        builder.executable(ExecutableCategory::Formatters);
    }
    if root.contains_key("lsp") {
        builder.executable(ExecutableCategory::CommandHelpers);
    }
    sanitize_sensitive_json_object(&mut root, "", &mut builder);
    let residual = json_bytes(root)?;
    builder.finish_json(residual)
}

fn inspect_codex(content: &str, revision: String) -> Result<InspectedNativeConfig> {
    let mut root = parse_toml_table(content)?;
    let mut builder =
        InspectionBuilder::new("codex", NativeConfigFormat::Toml, revision, content.len());
    if let Some(value) = root.remove("model") {
        builder.add_toml_string_candidate(
            "model",
            "model",
            ManagedFieldKind::Model,
            value,
            |value| {
                Some(CandidateValue::Model {
                    value: value.to_owned(),
                    provider_hint: None,
                })
            },
        );
    }
    if let Some(value) = root.remove("model_provider") {
        builder.add_toml_string_candidate(
            "provider",
            "model_provider",
            ManagedFieldKind::Provider,
            value,
            |value| {
                canonical_provider_id_for_agent_native_id("codex", value)
                    .map(|provider| CandidateValue::Provider(provider.to_owned()))
            },
        );
    }
    if root.remove("model_providers").is_some() {
        builder.block("model_providers", BlockedReason::ManagedUnsupported);
    }
    if let Some(mcp) = root.remove("mcp_servers") {
        classify_toml_mcp(&mut builder, mcp);
    }
    for key in CODEX_PERMISSION_ROOTS {
        if root.remove(*key).is_some() {
            let reason = if *key == "sandbox_mode" || *key == "sandbox_workspace_write" {
                BlockedReason::Sandbox
            } else {
                BlockedReason::Permissions
            };
            builder.block(*key, reason);
        }
    }
    for key in CODEX_AUTH_ROOTS {
        if root.remove(*key).is_some() {
            builder.block(*key, BlockedReason::AuthenticationState);
        }
    }
    for key in CODEX_MANAGED_UNSUPPORTED_ROOTS {
        if root.remove(*key).is_some() {
            builder.block(*key, BlockedReason::ManagedUnsupported);
        }
    }
    if root.contains_key("hooks") {
        builder.executable(ExecutableCategory::Hooks);
    }
    if root.contains_key("notify") {
        builder.executable(ExecutableCategory::Notifications);
    }
    sanitize_sensitive_toml_table(&mut root, "", &mut builder);
    let residual = toml_bytes(root)?;
    builder.finish_toml(residual)
}

fn inspect_amp(content: &str, revision: String) -> Result<InspectedNativeConfig> {
    let mut root = parse_json_object(content)?;
    // Amp is provider/model-opaque (set_provider=false, set_model=false), so
    // `settings.json` yields only MCP-server candidates. Its keys are flat
    // dotted strings (`"amp.mcpServers"`), matched as literal object keys.
    let mut builder =
        InspectionBuilder::new("amp", NativeConfigFormat::Json, revision, content.len());
    if let Some(mcp) = root.remove("amp.mcpServers") {
        classify_json_mcp(&mut builder, "amp.mcpServers", mcp, JsonMcpDialect::Amp);
    }
    for key in AMP_PERMISSION_ROOTS {
        if root.remove(*key).is_some() {
            builder.block(*key, BlockedReason::Permissions);
        }
    }
    for key in AMP_POLICY_ROOTS {
        if root.remove(*key).is_some() {
            builder.block(*key, BlockedReason::AcpsPolicy);
        }
    }
    sanitize_sensitive_json_object(&mut root, "", &mut builder);
    let residual = json_bytes(root)?;
    builder.finish_json(residual)
}

fn inspect_pi(content: &str, revision: String) -> Result<InspectedNativeConfig> {
    let mut root = parse_json_object(content)?;
    // Pi documents `settings.json` as strict JSON (no JSONC), so parse it the
    // same way as amp. Pi is provider-selecting (`defaultProvider`) with
    // a bare `defaultModel` id, so both a provider and a model candidate can be
    // extracted. Pi has no first-class MCP in its settings file (adapter-only),
    // so there are no MCP candidates.
    let mut builder =
        InspectionBuilder::new("pi", NativeConfigFormat::Json, revision, content.len());

    if let Some(value) = root.remove("defaultProvider") {
        builder.add_string_candidate(
            "provider",
            "defaultProvider",
            ManagedFieldKind::Provider,
            value,
            |value| {
                canonical_provider_id_for_agent_native_id("pi", value)
                    .map(|provider| CandidateValue::Provider(provider.to_owned()))
            },
        );
    }
    if let Some(value) = root.remove("defaultModel") {
        builder.add_string_candidate(
            "model",
            "defaultModel",
            ManagedFieldKind::Model,
            value,
            |value| {
                Some(CandidateValue::Model {
                    value: value.to_owned(),
                    provider_hint: None,
                })
            },
        );
    }
    for key in PI_PERMISSION_ROOTS {
        if root.remove(*key).is_some() {
            builder.block(*key, BlockedReason::Permissions);
        }
    }
    // `httpProxy` is a bare host:port for benign routing, but a proxy URL can
    // embed `user:pass@` credentials. Block it as credentials only when it
    // carries userinfo, mirroring how the Claude env-proxy keys are handled;
    // otherwise it survives into the residual.
    if root
        .get("httpProxy")
        .and_then(JsonValue::as_str)
        .is_some_and(url_contains_userinfo)
    {
        root.remove("httpProxy");
        builder.block("httpProxy", BlockedReason::Credentials);
    }
    for key in PI_EXECUTABLE_COMMAND_ROOTS {
        if root.contains_key(*key) {
            builder.executable(ExecutableCategory::CommandHelpers);
        }
    }
    for key in PI_EXECUTABLE_PLUGIN_ROOTS {
        if root.contains_key(*key) {
            builder.executable(ExecutableCategory::Plugins);
        }
    }

    sanitize_sensitive_json_object(&mut root, "", &mut builder);
    let residual = json_bytes(root)?;
    builder.finish_json(residual)
}

fn inspect_goose(content: &str, revision: String) -> Result<InspectedNativeConfig> {
    // Goose `config.yaml` root is a mapping of UPPERCASE `GOOSE_*` env-style
    // keys (provider/model/mode/tuning) plus a lowercase `extensions:` map. It
    // is parsed as a JSON `Map` after a YAML→JSON conversion that rejects
    // non-string keys, so the whole sanitize/paths pipeline shared with the
    // JSON harnesses applies unchanged; the residual is re-serialized as YAML.
    let mut root = parse_goose_root(content)?;
    let mut builder =
        InspectionBuilder::new("goose", NativeConfigFormat::Yaml, revision, content.len());

    if let Some(value) = root.remove("GOOSE_PROVIDER") {
        builder.add_string_candidate(
            "provider",
            "GOOSE_PROVIDER",
            ManagedFieldKind::Provider,
            value,
            |value| {
                canonical_provider_id_for_agent_native_id("goose", value)
                    .map(|provider| CandidateValue::Provider(provider.to_owned()))
            },
        );
    }
    // Goose `GOOSE_MODEL` is a bare model id; pair it with the provider named by
    // `GOOSE_PROVIDER` (mirroring how Codex pairs `model` with `model_provider`)
    // so the apply step can reject a model that does not belong to the selected
    // provider lane.
    if let Some(value) = root.remove("GOOSE_MODEL") {
        let provider_hint = goose_provider_hint(&builder);
        builder.add_string_candidate(
            "model",
            "GOOSE_MODEL",
            ManagedFieldKind::Model,
            value,
            move |value| {
                Some(CandidateValue::Model {
                    value: value.to_owned(),
                    provider_hint,
                })
            },
        );
    }
    if let Some(extensions) = root.remove("extensions") {
        classify_goose_extensions(&mut builder, extensions);
    }
    for key in GOOSE_PERMISSION_ROOTS {
        if root.remove(*key).is_some() {
            builder.block(*key, BlockedReason::Permissions);
        }
    }
    for key in GOOSE_MANAGED_UNSUPPORTED_ROOTS {
        if root.remove(*key).is_some() {
            builder.block(*key, BlockedReason::ManagedUnsupported);
        }
    }

    sanitize_sensitive_json_object(&mut root, "", &mut builder);
    let residual = goose_yaml_bytes(root)?;
    builder.finish_yaml(residual)
}

/// Resolve the provider hint for a `GOOSE_MODEL` candidate from a
/// `GOOSE_PROVIDER` candidate already recorded on the builder. The hint is the
/// agent-native provider id (what a `GOOSE_PROVIDER` value would read as), so
/// the apply step can compare it against the effective provider's native id.
fn goose_provider_hint(builder: &InspectionBuilder) -> Option<String> {
    match builder.candidate("provider")? {
        CandidateValue::Provider(provider) => agent_provider_id_for_provider_id("goose", provider)
            .map(str::to_owned)
            .or_else(|| Some(provider.clone())),
        _ => None,
    }
}

fn classify_goose_extensions(builder: &mut InspectionBuilder, value: JsonValue) {
    let JsonValue::Object(servers) = value else {
        builder.block("extensions", BlockedReason::McpUnmappable);
        return;
    };
    for (name, value) in &servers {
        let path = format!("extensions.{name}");
        match goose_extension_server(name, value) {
            Ok(server) => {
                let candidate_id = format!("mcp:{name}");
                if matches!(server, McpServerConfig::Stdio(_)) {
                    builder.executable_candidate(
                        candidate_id.clone(),
                        ExecutableCategory::CommandHelpers,
                    );
                }
                builder.add_candidate(
                    candidate_id,
                    path,
                    ManagedFieldKind::Mcp,
                    true,
                    CandidateValue::Mcp(server),
                );
            }
            Err(reason) => builder.block(path, reason),
        }
    }
}

fn goose_extension_server(
    name: &str,
    value: &JsonValue,
) -> std::result::Result<McpServerConfig, BlockedReason> {
    let object = value.as_object().ok_or(BlockedReason::McpUnmappable)?;
    if object.get("enabled").and_then(JsonValue::as_bool) == Some(false) {
        return Err(BlockedReason::McpUnmappable);
    }
    // Goose tags each extension with a `type`. Only `stdio` and the remote
    // `streamable_http`/`sse` transports have anything acps can launch; the
    // rest (`builtin`, `platform`, `frontend`, `inline_python`) run inside the
    // Goose process, so there is no external server to import.
    let extension_type = object
        .get("type")
        .and_then(JsonValue::as_str)
        .ok_or(BlockedReason::McpUnmappable)?;
    match extension_type {
        "stdio" => goose_stdio_extension(name, object),
        "streamable_http" | "sse" => goose_remote_extension(name, object),
        _ => Err(BlockedReason::McpUnmappable),
    }
}

fn goose_stdio_extension(
    name: &str,
    object: &JsonMap<String, JsonValue>,
) -> std::result::Result<McpServerConfig, BlockedReason> {
    let command = object
        .get("cmd")
        .and_then(JsonValue::as_str)
        .ok_or(BlockedReason::McpUnmappable)?
        .to_owned();
    let args = match object.get("args") {
        Some(value) => json_string_array(value).ok_or(BlockedReason::McpUnmappable)?,
        None => Vec::new(),
    };
    // `envs` is a literal `KEY: value` map. acps stdio `env` entries are
    // secret-store reference NAMES, so a literal env table cannot be
    // represented; classify by key name so a credential-bearing table surfaces
    // as credentials rather than a generic mapping failure. An empty `envs`
    // carries nothing and does not block.
    if let Some(envs) = object.get("envs") {
        let envs = envs.as_object().ok_or(BlockedReason::McpUnmappable)?;
        if !envs.is_empty() {
            if envs.keys().any(|key| sensitive_field_reason(key).is_some()) {
                return Err(BlockedReason::Credentials);
            }
            return Err(BlockedReason::McpUnmappable);
        }
    }
    // `env_keys` forwards variable NAMES resolved from the Goose keyring/secrets
    // store at launch — the same shape as Codex `env_vars`. acps satisfies those
    // names from its own secret store at session attach.
    let env = match object.get("env_keys") {
        Some(value) => json_string_array(value).ok_or(BlockedReason::McpUnmappable)?,
        None => Vec::new(),
    };
    // `cwd` changes what `cmd` resolves against and acps cannot express it, so
    // dropping it would corrupt the server. `available_tools` is a tool filter:
    // dropping it would silently re-enable tools the user turned off. Both stay
    // outside the allowlist so such servers keep blocking. `timeout`/`bundled`/
    // `description`/`name`/`display_name` are launch metadata acps ignores.
    let allowed: BTreeSet<&str> = [
        "type",
        "name",
        "display_name",
        "description",
        "cmd",
        "args",
        "envs",
        "env_keys",
        "timeout",
        "bundled",
        "enabled",
    ]
    .into_iter()
    .collect();
    if object.keys().any(|key| !allowed.contains(key.as_str())) {
        return Err(BlockedReason::McpUnmappable);
    }
    if command_args_contain_literal_credentials(&args) {
        return Err(BlockedReason::Credentials);
    }
    Ok(McpServerConfig::Stdio(McpStdioServer {
        name: name.to_owned(),
        command,
        args,
        env,
    }))
}

fn goose_remote_extension(
    name: &str,
    object: &JsonMap<String, JsonValue>,
) -> std::result::Result<McpServerConfig, BlockedReason> {
    // A literal `headers` table carries auth material acps http servers express
    // only as secret-store references, so any headers block (credentials when a
    // header key is sensitive, otherwise an unmappable literal table).
    if let Some(headers) = object.get("headers") {
        let headers = headers.as_object().ok_or(BlockedReason::McpUnmappable)?;
        if !headers.is_empty() {
            if headers
                .keys()
                .any(|key| sensitive_field_reason(key).is_some())
            {
                return Err(BlockedReason::Credentials);
            }
            return Err(BlockedReason::McpUnmappable);
        }
    }
    // Same literal-`envs` reasoning as the stdio path: acps cannot carry a
    // literal env table on an http server, so a non-empty one blocks.
    if let Some(envs) = object.get("envs") {
        let envs = envs.as_object().ok_or(BlockedReason::McpUnmappable)?;
        if !envs.is_empty() {
            if envs.keys().any(|key| sensitive_field_reason(key).is_some()) {
                return Err(BlockedReason::Credentials);
            }
            return Err(BlockedReason::McpUnmappable);
        }
    }
    let uri = object
        .get("uri")
        .and_then(JsonValue::as_str)
        .ok_or(BlockedReason::McpUnmappable)?;
    // `socket` re-points the transport at a Unix domain socket acps http
    // servers cannot express; `env_keys` names launch-time secrets an http
    // server has no field for. Both stay outside the allowlist so such servers
    // keep blocking.
    let allowed: BTreeSet<&str> = [
        "type",
        "name",
        "description",
        "uri",
        "headers",
        "envs",
        "timeout",
        "bundled",
        "enabled",
    ]
    .into_iter()
    .collect();
    if object.keys().any(|key| !allowed.contains(key.as_str())) {
        return Err(BlockedReason::McpUnmappable);
    }
    if !mcp_http_url_is_credential_free(uri) {
        return Err(BlockedReason::Credentials);
    }
    Ok(McpServerConfig::Http(McpHttpServer {
        name: name.to_owned(),
        url: uri.to_owned(),
        headers: Vec::new(),
    }))
}

#[derive(Clone, Copy)]
enum JsonMcpDialect {
    Claude,
    OpenCode,
    Amp,
}

fn classify_json_mcp(
    builder: &mut InspectionBuilder,
    root_path: &str,
    value: JsonValue,
    dialect: JsonMcpDialect,
) {
    let Some(servers) = value.as_object() else {
        builder.block(root_path, BlockedReason::McpUnmappable);
        return;
    };
    for (name, value) in servers {
        let path = format!("{root_path}.{name}");
        match json_mcp_server(name, value, dialect) {
            Ok(server) => {
                let candidate_id = format!("mcp:{name}");
                if matches!(server, McpServerConfig::Stdio(_)) {
                    builder.executable_candidate(
                        candidate_id.clone(),
                        ExecutableCategory::CommandHelpers,
                    );
                }
                builder.add_candidate(
                    candidate_id,
                    path,
                    ManagedFieldKind::Mcp,
                    true,
                    CandidateValue::Mcp(server),
                );
            }
            Err(reason) => builder.block(path, reason),
        }
    }
}

fn json_mcp_server(
    name: &str,
    value: &JsonValue,
    dialect: JsonMcpDialect,
) -> std::result::Result<McpServerConfig, BlockedReason> {
    let object = value.as_object().ok_or(BlockedReason::McpUnmappable)?;
    if object.get("enabled").and_then(JsonValue::as_bool) == Some(false) {
        return Err(BlockedReason::McpUnmappable);
    }
    // Amp remote servers may carry a literal `headers` object.
    // acps http `headers` are `{name, value_ref}` secret-store references, so a
    // literal header table cannot be represented; classify by key name so a
    // credential-bearing table surfaces as credentials rather than a generic
    // mapping failure.
    if matches!(dialect, JsonMcpDialect::Amp)
        && let Some(headers) = object.get("headers")
    {
        let headers = headers.as_object().ok_or(BlockedReason::McpUnmappable)?;
        if headers
            .keys()
            .any(|key| sensitive_field_reason(key).is_some())
        {
            return Err(BlockedReason::Credentials);
        }
        return Err(BlockedReason::McpUnmappable);
    }
    if let Some(url) = object.get("url").and_then(JsonValue::as_str) {
        let allowed: BTreeSet<&str> = match dialect {
            JsonMcpDialect::Claude => ["url", "type"].into_iter().collect(),
            JsonMcpDialect::OpenCode => ["url", "type", "enabled"].into_iter().collect(),
            JsonMcpDialect::Amp => ["url", "type", "includeTools"].into_iter().collect(),
        };
        if object.keys().any(|key| !allowed.contains(key.as_str())) {
            return Err(BlockedReason::McpUnmappable);
        }
        if !mcp_http_url_is_credential_free(url) {
            return Err(BlockedReason::Credentials);
        }
        return Ok(McpServerConfig::Http(McpHttpServer {
            name: name.to_owned(),
            url: url.to_owned(),
            headers: Vec::new(),
        }));
    }

    let (command, args) = match object.get("command").ok_or(BlockedReason::McpUnmappable)? {
        JsonValue::String(command) => {
            let args = match object.get("args") {
                Some(value) => json_string_array(value).ok_or(BlockedReason::McpUnmappable)?,
                None => Vec::new(),
            };
            (command.clone(), args)
        }
        JsonValue::Array(command) if !command.is_empty() => {
            let mut values = command.iter().map(JsonValue::as_str);
            let command = values
                .next()
                .flatten()
                .ok_or(BlockedReason::McpUnmappable)?
                .to_owned();
            let args = values
                .map(|value| value.map(str::to_owned))
                .collect::<Option<Vec<_>>>()
                .ok_or(BlockedReason::McpUnmappable)?;
            (command, args)
        }
        _ => return Err(BlockedReason::McpUnmappable),
    };
    // Amp stdio servers carry a literal `env` object of KEY=value
    // pairs. acps stdio `env` entries are secret-store reference NAMES, so a
    // literal env table cannot be represented; classify by key name so a
    // credential-bearing table surfaces as credentials rather than a generic
    // mapping failure.
    if matches!(dialect, JsonMcpDialect::Amp)
        && let Some(env) = object.get("env")
    {
        let env = env.as_object().ok_or(BlockedReason::McpUnmappable)?;
        if env.keys().any(|key| sensitive_field_reason(key).is_some()) {
            return Err(BlockedReason::Credentials);
        }
        return Err(BlockedReason::McpUnmappable);
    }
    let allowed: BTreeSet<&str> = match dialect {
        JsonMcpDialect::Claude => ["command", "args", "type"].into_iter().collect(),
        JsonMcpDialect::OpenCode => ["command", "type", "enabled"].into_iter().collect(),
        // Amp's `includeTools` carries semantics acps cannot express, so it
        // stays outside the allowlist and blocks.
        JsonMcpDialect::Amp => ["command", "args", "type"].into_iter().collect(),
    };
    if object.keys().any(|key| !allowed.contains(key.as_str())) {
        return Err(BlockedReason::McpUnmappable);
    }
    if command_args_contain_literal_credentials(&args) {
        return Err(BlockedReason::Credentials);
    }
    Ok(McpServerConfig::Stdio(McpStdioServer {
        name: name.to_owned(),
        command,
        args,
        env: Vec::new(),
    }))
}

fn classify_toml_mcp(builder: &mut InspectionBuilder, value: TomlValue) {
    let Some(servers) = value.as_table() else {
        builder.block("mcp_servers", BlockedReason::McpUnmappable);
        return;
    };
    for (name, value) in servers {
        let path = format!("mcp_servers.{name}");
        match toml_mcp_server(name, value) {
            Ok(server) => {
                let candidate_id = format!("mcp:{name}");
                if matches!(server, McpServerConfig::Stdio(_)) {
                    builder.executable_candidate(
                        candidate_id.clone(),
                        ExecutableCategory::CommandHelpers,
                    );
                }
                builder.add_candidate(
                    candidate_id,
                    path,
                    ManagedFieldKind::Mcp,
                    true,
                    CandidateValue::Mcp(server),
                );
            }
            Err(reason) => builder.block(path, reason),
        }
    }
}

fn toml_mcp_server(
    name: &str,
    value: &TomlValue,
) -> std::result::Result<McpServerConfig, BlockedReason> {
    let table = value.as_table().ok_or(BlockedReason::McpUnmappable)?;
    if table.get("enabled").and_then(TomlValue::as_bool) == Some(false) {
        return Err(BlockedReason::McpUnmappable);
    }
    if let Some(url) = table.get("url").and_then(TomlValue::as_str) {
        // Timeouts and `required` are launch tuning with Codex-side defaults;
        // auth material (`bearer_token_env_var`, `http_headers`, …) stays
        // outside the allowlist so those servers keep blocking.
        let allowed: BTreeSet<&str> = [
            "url",
            "enabled",
            "required",
            "startup_timeout_sec",
            "startup_timeout_ms",
            "tool_timeout_sec",
            "tool_timeout_ms",
        ]
        .into_iter()
        .collect();
        if table.keys().any(|key| !allowed.contains(key.as_str())) {
            return Err(BlockedReason::McpUnmappable);
        }
        if !mcp_http_url_is_credential_free(url) {
            return Err(BlockedReason::Credentials);
        }
        return Ok(McpServerConfig::Http(McpHttpServer {
            name: name.to_owned(),
            url: url.to_owned(),
            headers: Vec::new(),
        }));
    }
    let command = table
        .get("command")
        .and_then(TomlValue::as_str)
        .ok_or(BlockedReason::McpUnmappable)?
        .to_owned();
    let args = match table.get("args") {
        Some(value) => toml_string_array(value).ok_or(BlockedReason::McpUnmappable)?,
        None => Vec::new(),
    };
    // Codex `env` is an inline table of literal KEY=value pairs. The acps MCP
    // schema has no literal-env representation (stdio `env` entries are
    // secret-store references), so a server carrying one cannot be imported;
    // classify by key name so credential-bearing tables surface as
    // credentials instead of a generic mapping failure.
    if let Some(env) = table.get("env") {
        let env = env.as_table().ok_or(BlockedReason::McpUnmappable)?;
        if env.keys().any(|key| sensitive_field_reason(key).is_some()) {
            return Err(BlockedReason::Credentials);
        }
        return Err(BlockedReason::McpUnmappable);
    }
    // Codex `cwd` changes what the launched command resolves against, and the
    // acps MCP schema cannot express it, so dropping it would corrupt the
    // server rather than degrade it.
    if table.get("cwd").is_some() {
        return Err(BlockedReason::McpUnmappable);
    }
    // Codex `env_vars` forwards variable NAMES from the launching
    // environment, either as strings or `{ name, source? }` objects. acps
    // satisfies those names from the secret store at session attach.
    let env = match table.get("env_vars") {
        Some(value) => toml_env_var_names(value).ok_or(BlockedReason::McpUnmappable)?,
        None => Vec::new(),
    };
    // Startup/tool timeouts and `required` are launch tuning with Codex-side
    // defaults; acps cannot express them and the server behaves identically
    // apart from timeout margins and Codex's own startup strictness, so they
    // are accepted and dropped. `enabled_tools`/`disabled_tools` stay outside
    // the allowlist: dropping a tool filter would silently re-enable tools
    // the user turned off.
    let allowed: BTreeSet<&str> = [
        "command",
        "args",
        "env",
        "env_vars",
        "cwd",
        "enabled",
        "required",
        "startup_timeout_sec",
        "startup_timeout_ms",
        "tool_timeout_sec",
        "tool_timeout_ms",
    ]
    .into_iter()
    .collect();
    if table.keys().any(|key| !allowed.contains(key.as_str())) {
        return Err(BlockedReason::McpUnmappable);
    }
    if command_args_contain_literal_credentials(&args) {
        return Err(BlockedReason::Credentials);
    }
    Ok(McpServerConfig::Stdio(McpStdioServer {
        name: name.to_owned(),
        command,
        args,
        env,
    }))
}

fn toml_env_var_names(value: &TomlValue) -> Option<Vec<String>> {
    let array = value.as_array()?;
    let mut names = Vec::with_capacity(array.len());
    for entry in array {
        if let Some(name) = entry.as_str() {
            names.push(name.to_owned());
            continue;
        }
        let table = entry.as_table()?;
        if table
            .keys()
            .any(|key| !matches!(key.as_str(), "name" | "source"))
        {
            return None;
        }
        names.push(table.get("name")?.as_str()?.to_owned());
    }
    Some(names)
}

fn mcp_http_url_is_credential_free(value: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(value) else {
        return false;
    };
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return false;
    }
    if url
        .path_segments()
        .is_some_and(|mut segments| segments.any(path_segment_looks_like_credential))
    {
        return false;
    }
    !url.query_pairs().any(|(name, _)| {
        let normalized = name.to_ascii_lowercase().replace('-', "_");
        sensitive_field_reason(&name).is_some()
            || matches!(
                normalized.as_str(),
                "sig" | "signature" | "access_key" | "access_key_id"
            )
            || normalized.contains("signature")
    })
}

/// Tokens embedded as URL path segments (`https://host/mcp/sk-…`) carry no
/// field name to classify, so match the widely used key prefixes instead.
/// The length floor keeps ordinary words that share a prefix (for example
/// `sk-learn`) out of the match.
fn path_segment_looks_like_credential(segment: &str) -> bool {
    let lowered = segment.to_ascii_lowercase();
    CREDENTIAL_PATH_SEGMENT_PREFIXES
        .iter()
        .any(|prefix| lowered.starts_with(prefix) && lowered.len() > prefix.len() + 8)
}

fn url_contains_userinfo(value: &str) -> bool {
    reqwest::Url::parse(value)
        .is_ok_and(|url| !url.username().is_empty() || url.password().is_some())
}

fn command_args_contain_literal_credentials(args: &[String]) -> bool {
    args.iter().enumerate().any(|(index, argument)| {
        let trimmed = argument.trim_start_matches('-');
        let name = trimmed.split_once('=').map_or(trimmed, |(name, _)| name);
        let sensitive = sensitive_field_reason(name).is_some()
            || name.to_ascii_lowercase().contains("signature");
        sensitive && (trimmed.contains('=') || args.get(index + 1).is_some())
    }) || args.iter().any(|argument| {
        argument_carries_header_credential(argument)
            || string_contains_high_confidence_credential(argument)
    })
}

/// Header-style credentials arrive as values rather than flag names
/// (`-H "Authorization: Bearer …"`, `--header "x-api-key: …"`), so the
/// name-based flag scan alone misses them.
fn argument_carries_header_credential(argument: &str) -> bool {
    if argument
        .trim_start()
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("bearer "))
    {
        return true;
    }
    let Some((name, value)) = argument.split_once(':') else {
        return false;
    };
    !value.trim().is_empty() && sensitive_field_reason(name.trim()).is_some()
}

fn split_opencode_model(value: &str) -> (Option<&str>, &str) {
    match value.split_once('/') {
        Some((provider, model)) if !provider.is_empty() && !model.is_empty() => {
            (Some(provider), model)
        }
        _ => (None, value),
    }
}

struct InspectionBuilder {
    inspection: NativeConfigInspection,
    candidates: BTreeMap<String, CandidateValue>,
    blocked_paths: BTreeSet<String>,
    executable: BTreeSet<ExecutableCategory>,
    executable_candidate_ids: BTreeSet<String>,
    residual_has_executable: bool,
}

impl InspectionBuilder {
    fn new(harness: &str, format: NativeConfigFormat, revision: String, size_bytes: usize) -> Self {
        Self {
            inspection: NativeConfigInspection {
                revision,
                harness: harness.to_owned(),
                format,
                size_bytes,
                managed_fields: Vec::new(),
                blocked_fields: Vec::new(),
                unmanaged_field_paths: Vec::new(),
                executable_categories: Vec::new(),
                warnings: Vec::new(),
            },
            candidates: BTreeMap::new(),
            blocked_paths: BTreeSet::new(),
            executable: BTreeSet::new(),
            executable_candidate_ids: BTreeSet::new(),
            residual_has_executable: false,
        }
    }

    fn has_candidate(&self, id: &str) -> bool {
        self.candidates.contains_key(id)
    }

    fn candidate(&self, id: &str) -> Option<&CandidateValue> {
        self.candidates.get(id)
    }

    fn add_candidate(
        &mut self,
        id: impl Into<String>,
        path: impl Into<String>,
        kind: ManagedFieldKind,
        compatible: bool,
        value: CandidateValue,
    ) {
        let id = id.into();
        if self.inspection.managed_fields.len() >= MAX_MANIFEST_PATHS {
            self.warn_once("manifest-truncated");
            return;
        }
        if self.candidates.contains_key(&id) {
            self.block(path, BlockedReason::ManagedUnsupported);
            return;
        }
        self.inspection.managed_fields.push(ManagedField {
            id: id.clone(),
            path: path.into(),
            kind,
            compatible,
        });
        self.candidates.insert(id, value);
    }

    fn incompatible(
        &mut self,
        id: impl Into<String>,
        path: impl Into<String>,
        kind: ManagedFieldKind,
    ) {
        if self.inspection.managed_fields.len() >= MAX_MANIFEST_PATHS {
            self.warn_once("manifest-truncated");
            return;
        }
        self.inspection.managed_fields.push(ManagedField {
            id: id.into(),
            path: path.into(),
            kind,
            compatible: false,
        });
    }

    fn add_string_candidate<F>(
        &mut self,
        id: &str,
        path: &str,
        kind: ManagedFieldKind,
        value: JsonValue,
        convert: F,
    ) where
        F: FnOnce(&str) -> Option<CandidateValue>,
    {
        match value.as_str().and_then(convert) {
            Some(value) => self.add_candidate(id, path, kind, true, value),
            None => self.incompatible(id, path, kind),
        }
    }

    fn add_toml_string_candidate<F>(
        &mut self,
        id: &str,
        path: &str,
        kind: ManagedFieldKind,
        value: TomlValue,
        convert: F,
    ) where
        F: FnOnce(&str) -> Option<CandidateValue>,
    {
        match value.as_str().and_then(convert) {
            Some(value) => self.add_candidate(id, path, kind, true, value),
            None => self.incompatible(id, path, kind),
        }
    }

    fn block(&mut self, path: impl Into<String>, reason: BlockedReason) {
        let path = path.into();
        if self.blocked_paths.contains(&path) {
            return;
        }
        if self.inspection.blocked_fields.len() >= MAX_MANIFEST_PATHS {
            self.warn_once("manifest-truncated");
            return;
        }
        self.blocked_paths.insert(path.clone());
        self.inspection
            .blocked_fields
            .push(BlockedField { path, reason });
    }

    fn executable(&mut self, category: ExecutableCategory) {
        self.residual_has_executable = true;
        self.executable.insert(category);
    }

    fn executable_candidate(&mut self, id: String, category: ExecutableCategory) {
        self.executable_candidate_ids.insert(id);
        self.executable.insert(category);
    }

    fn warn(&mut self, code: &str) {
        self.inspection.warnings.push(code.to_owned());
    }

    fn warn_once(&mut self, code: &str) {
        if !self
            .inspection
            .warnings
            .iter()
            .any(|warning| warning == code)
        {
            self.warn(code);
        }
    }

    fn finish_json(mut self, residual: Vec<u8>) -> Result<InspectedNativeConfig> {
        let value: JsonValue =
            serde_json::from_slice(&residual).map_err(|_| native_error("native_config_invalid"))?;
        collect_json_paths(&value, "", &mut self.inspection.unmanaged_field_paths);
        self.finish(residual)
    }

    fn finish_toml(mut self, residual: Vec<u8>) -> Result<InspectedNativeConfig> {
        let text =
            std::str::from_utf8(&residual).map_err(|_| native_error("native_config_invalid"))?;
        let value: TomlValue =
            toml::from_str(text).map_err(|_| native_error("native_config_invalid"))?;
        collect_toml_paths(&value, "", &mut self.inspection.unmanaged_field_paths);
        self.finish(residual)
    }

    fn finish_yaml(mut self, residual: Vec<u8>) -> Result<InspectedNativeConfig> {
        // Paths are collected from the JSON projection of the residual so the
        // dotted-path shape matches the JSON harnesses. The residual is stored
        // as YAML, so re-parse it through the same non-string-key guard used on
        // input before projecting.
        let text =
            std::str::from_utf8(&residual).map_err(|_| native_error("native_config_invalid"))?;
        let root = parse_goose_root(text)?;
        let value = JsonValue::Object(root);
        collect_json_paths(&value, "", &mut self.inspection.unmanaged_field_paths);
        self.finish(residual)
    }

    fn finish(mut self, residual: Vec<u8>) -> Result<InspectedNativeConfig> {
        if residual.len() > IMPORT_SIZE_LIMIT {
            return Err(native_error("native_config_normalized_too_large"));
        }
        self.inspection
            .managed_fields
            .sort_by(|a, b| a.id.cmp(&b.id));
        self.inspection
            .blocked_fields
            .sort_by(|a, b| a.path.cmp(&b.path));
        self.inspection.unmanaged_field_paths.sort();
        self.inspection.unmanaged_field_paths.dedup();
        if self.inspection.unmanaged_field_paths.len() > MAX_MANIFEST_PATHS {
            self.warn_once("manifest-truncated");
        }
        self.inspection
            .unmanaged_field_paths
            .truncate(MAX_MANIFEST_PATHS);
        self.inspection.executable_categories = self.executable.into_iter().collect();
        Ok(InspectedNativeConfig {
            inspection: self.inspection,
            residual,
            candidates: self.candidates,
            executable_candidate_ids: self.executable_candidate_ids,
            residual_has_executable: self.residual_has_executable,
        })
    }
}

fn parse_json_object(content: &str) -> Result<JsonMap<String, JsonValue>> {
    match serde_json::from_str::<JsonValue>(content) {
        Ok(JsonValue::Object(root)) => Ok(root),
        _ => Err(native_error("native_config_invalid")),
    }
}

fn parse_jsonc_object(content: &str) -> Result<JsonMap<String, JsonValue>> {
    let stripped = strip_jsonc_comments(content)?;
    let normalized = strip_jsonc_trailing_commas(&stripped)?;
    parse_json_object(&normalized)
}

fn parse_toml_table(content: &str) -> Result<TomlMap<String, TomlValue>> {
    match toml::from_str::<TomlValue>(content) {
        Ok(TomlValue::Table(root)) => Ok(root),
        _ => Err(native_error("native_config_invalid")),
    }
}

/// Parse a Goose `config.yaml` root into a JSON object. Goose config is YAML,
/// but the whole classification/sanitize/paths pipeline is JSON-shaped, so the
/// document is converted to JSON up front. YAML permits non-string mapping keys
/// (numbers, booleans, sequences); those have no JSON representation and no
/// legitimate place in a Goose config, so any mapping carrying one is rejected
/// as invalid rather than lossily coerced.
fn parse_goose_root(content: &str) -> Result<JsonMap<String, JsonValue>> {
    let value: YamlValue =
        serde_norway::from_str(content).map_err(|_| native_error("native_config_invalid"))?;
    match yaml_value_to_json(value)? {
        JsonValue::Object(root) => Ok(root),
        _ => Err(native_error("native_config_invalid")),
    }
}

fn yaml_value_to_json(value: YamlValue) -> Result<JsonValue> {
    match value {
        YamlValue::Null => Ok(JsonValue::Null),
        YamlValue::Bool(value) => Ok(JsonValue::Bool(value)),
        YamlValue::Number(number) => yaml_number_to_json(number),
        YamlValue::String(value) => Ok(JsonValue::String(value)),
        YamlValue::Sequence(values) => Ok(JsonValue::Array(
            values
                .into_iter()
                .map(yaml_value_to_json)
                .collect::<Result<Vec<_>>>()?,
        )),
        YamlValue::Mapping(mapping) => {
            let mut object = JsonMap::with_capacity(mapping.len());
            for (key, value) in mapping {
                // Reject non-string keys instead of stringifying them: a Goose
                // config never uses them, and a silent coercion could collide
                // two distinct keys or smuggle content past the sanitize pass.
                let YamlValue::String(key) = key else {
                    return Err(native_error("native_config_invalid"));
                };
                object.insert(key, yaml_value_to_json(value)?);
            }
            Ok(JsonValue::Object(object))
        }
        YamlValue::Tagged(_) => Err(native_error("native_config_invalid")),
    }
}

fn yaml_number_to_json(number: serde_norway::Number) -> Result<JsonValue> {
    if let Some(value) = number.as_i64() {
        return Ok(JsonValue::Number(value.into()));
    }
    if let Some(value) = number.as_u64() {
        return Ok(JsonValue::Number(value.into()));
    }
    number
        .as_f64()
        .and_then(serde_json::Number::from_f64)
        .map(JsonValue::Number)
        .ok_or_else(|| native_error("native_config_invalid"))
}

fn json_value_to_yaml(value: JsonValue) -> YamlValue {
    match value {
        JsonValue::Null => YamlValue::Null,
        JsonValue::Bool(value) => YamlValue::Bool(value),
        JsonValue::Number(number) => json_number_to_yaml(number),
        JsonValue::String(value) => YamlValue::String(value),
        JsonValue::Array(values) => {
            YamlValue::Sequence(values.into_iter().map(json_value_to_yaml).collect())
        }
        JsonValue::Object(object) => {
            let mut mapping = YamlMapping::with_capacity(object.len());
            for (key, value) in object {
                mapping.insert(YamlValue::String(key), json_value_to_yaml(value));
            }
            YamlValue::Mapping(mapping)
        }
    }
}

fn json_number_to_yaml(number: serde_json::Number) -> YamlValue {
    if let Some(value) = number.as_i64() {
        return YamlValue::Number(value.into());
    }
    if let Some(value) = number.as_u64() {
        return YamlValue::Number(value.into());
    }
    match number.as_f64() {
        Some(value) => YamlValue::Number(value.into()),
        None => YamlValue::Null,
    }
}

fn goose_yaml_bytes(root: JsonMap<String, JsonValue>) -> Result<Vec<u8>> {
    let value = json_value_to_yaml(JsonValue::Object(root));
    let text =
        serde_norway::to_string(&value).map_err(|_| native_error("native_config_invalid"))?;
    Ok(text.into_bytes())
}

fn json_bytes(root: JsonMap<String, JsonValue>) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(&JsonValue::Object(root))
        .map_err(|_| native_error("native_config_invalid"))?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn toml_bytes(root: TomlMap<String, TomlValue>) -> Result<Vec<u8>> {
    let text = toml::to_string_pretty(&TomlValue::Table(root))
        .map_err(|_| native_error("native_config_invalid"))?;
    Ok(text.into_bytes())
}

fn json_string_array(value: &JsonValue) -> Option<Vec<String>> {
    value
        .as_array()?
        .iter()
        .map(|value| value.as_str().map(str::to_owned))
        .collect()
}

fn toml_string_array(value: &TomlValue) -> Option<Vec<String>> {
    value
        .as_array()?
        .iter()
        .map(|value| value.as_str().map(str::to_owned))
        .collect()
}

fn collect_json_paths(value: &JsonValue, prefix: &str, out: &mut Vec<String>) {
    if out.len() > MAX_MANIFEST_PATHS {
        return;
    }
    match value {
        JsonValue::Object(object) if !object.is_empty() => {
            for (key, value) in object {
                let path = join_path(prefix, key);
                collect_json_paths(value, &path, out);
            }
        }
        JsonValue::Array(_) => {
            if !prefix.is_empty() {
                out.push(prefix.to_owned());
            }
        }
        _ => {
            if !prefix.is_empty() {
                out.push(prefix.to_owned());
            }
        }
    }
}

fn collect_toml_paths(value: &TomlValue, prefix: &str, out: &mut Vec<String>) {
    if out.len() > MAX_MANIFEST_PATHS {
        return;
    }
    match value {
        TomlValue::Table(table) if !table.is_empty() => {
            for (key, value) in table {
                let path = join_path(prefix, key);
                collect_toml_paths(value, &path, out);
            }
        }
        TomlValue::Array(_) => {
            if !prefix.is_empty() {
                out.push(prefix.to_owned());
            }
        }
        _ => {
            if !prefix.is_empty() {
                out.push(prefix.to_owned());
            }
        }
    }
}

fn join_path(prefix: &str, key: &str) -> String {
    if prefix.is_empty() {
        key.to_owned()
    } else {
        format!("{prefix}.{key}")
    }
}

fn sanitize_sensitive_json_object(
    object: &mut JsonMap<String, JsonValue>,
    prefix: &str,
    builder: &mut InspectionBuilder,
) {
    let keys = object.keys().cloned().collect::<Vec<_>>();
    for key in keys {
        let path = join_path(prefix, &key);
        if let Some(reason) = sensitive_field_reason(&key) {
            object.remove(&key);
            builder.block(path, reason);
            continue;
        }
        if object
            .get(&key)
            .is_some_and(json_value_contains_high_confidence_credential)
        {
            object.remove(&key);
            builder.block(path, BlockedReason::Credentials);
            continue;
        }
        if let Some(value) = object.get_mut(&key) {
            sanitize_sensitive_json_value(value, &path, builder);
        }
    }
}

fn sanitize_sensitive_json_value(
    value: &mut JsonValue,
    prefix: &str,
    builder: &mut InspectionBuilder,
) {
    match value {
        JsonValue::Object(object) => sanitize_sensitive_json_object(object, prefix, builder),
        JsonValue::Array(values) => {
            for (index, value) in values.iter_mut().enumerate() {
                sanitize_sensitive_json_value(value, &format!("{prefix}[{index}]"), builder);
            }
        }
        _ => {}
    }
}

fn sanitize_sensitive_toml_table(
    table: &mut TomlMap<String, TomlValue>,
    prefix: &str,
    builder: &mut InspectionBuilder,
) {
    let keys = table.keys().cloned().collect::<Vec<_>>();
    for key in keys {
        let path = join_path(prefix, &key);
        if let Some(reason) = sensitive_field_reason(&key) {
            table.remove(&key);
            builder.block(path, reason);
            continue;
        }
        if table
            .get(&key)
            .is_some_and(toml_value_contains_high_confidence_credential)
        {
            table.remove(&key);
            builder.block(path, BlockedReason::Credentials);
            continue;
        }
        if let Some(value) = table.get_mut(&key) {
            sanitize_sensitive_toml_value(value, &path, builder);
        }
    }
}

fn sanitize_sensitive_toml_value(
    value: &mut TomlValue,
    prefix: &str,
    builder: &mut InspectionBuilder,
) {
    match value {
        TomlValue::Table(table) => sanitize_sensitive_toml_table(table, prefix, builder),
        TomlValue::Array(values) => {
            for (index, value) in values.iter_mut().enumerate() {
                sanitize_sensitive_toml_value(value, &format!("{prefix}[{index}]"), builder);
            }
        }
        _ => {}
    }
}

fn sensitive_field_reason(key: &str) -> Option<BlockedReason> {
    let normalized = key.to_ascii_lowercase().replace('-', "_");
    let flattened = normalized.replace('_', "");
    if matches!(
        normalized.as_str(),
        "auth" | "authentication" | "login" | "login_state" | "oauth" | "oauth_state"
    ) || flattened.contains("credential")
        || flattened.contains("login")
        // `coauth…` guards Claude Code's `includeCoAuthoredBy` (and similar
        // co-author fields) from matching the `oauth` substring.
        || (flattened.contains("oauth") && !flattened.contains("coauth"))
    {
        return Some(BlockedReason::AuthenticationState);
    }
    if normalized == "key"
        || normalized.ends_with("_key")
        || flattened.contains("apikey")
        || flattened.contains("secret")
        || flattened.contains("password")
        || flattened.contains("passwd")
        || flattened.contains("bearer")
        || flattened.contains("authorization")
        // A credential-style `token` either ends the key (`authToken`,
        // `github_token`) or is followed by a value word (`tokenValue`,
        // `tokenRef`). Quantity fields — `max_tokens`,
        // `model_auto_compact_token_limit`, `…_token_weight` — follow
        // `token(s)` with a count word instead and are routine model tuning,
        // not credentials.
        || flattened.ends_with("token")
        || flattened.contains("tokenvalue")
        || flattened.contains("tokenref")
        || flattened.contains("tokenid")
    {
        return Some(BlockedReason::Credentials);
    }
    None
}

fn json_value_contains_high_confidence_credential(value: &JsonValue) -> bool {
    match value {
        JsonValue::String(value) => string_contains_high_confidence_credential(value),
        JsonValue::Array(values) => values
            .iter()
            .any(json_value_contains_high_confidence_credential),
        JsonValue::Object(object) => object
            .values()
            .any(json_value_contains_high_confidence_credential),
        _ => false,
    }
}

fn toml_value_contains_high_confidence_credential(value: &TomlValue) -> bool {
    match value {
        TomlValue::String(value) => string_contains_high_confidence_credential(value),
        TomlValue::Array(values) => values
            .iter()
            .any(toml_value_contains_high_confidence_credential),
        TomlValue::Table(table) => table
            .values()
            .any(toml_value_contains_high_confidence_credential),
        _ => false,
    }
}

fn string_contains_high_confidence_credential(value: &str) -> bool {
    let trimmed = value.trim();
    if path_segment_looks_like_credential(trimmed) || argument_carries_header_credential(trimmed) {
        return true;
    }
    reqwest::Url::parse(trimmed).is_ok_and(|url| matches!(url.scheme(), "http" | "https"))
        && !mcp_http_url_is_credential_free(trimmed)
}

fn executable_environment_key(key: &str) -> bool {
    let key = key.to_ascii_uppercase();
    matches!(
        key.as_str(),
        "BASH_ENV"
            | "CLAUDE_CODE_GIT_BASH_PATH"
            | "COMSPEC"
            | "EDITOR"
            | "ENV"
            | "GIT_ASKPASS"
            | "GIT_PAGER"
            | "GIT_SSH"
            | "GIT_SSH_COMMAND"
            | "LD_PRELOAD"
            | "NODE_OPTIONS"
            | "PAGER"
            | "PERL5OPT"
            | "PATH"
            | "PYTHONPATH"
            | "PYTHONSTARTUP"
            | "RUSTC_WRAPPER"
            | "RUBYOPT"
            | "SHELL"
            | "SSH_ASKPASS"
            | "VISUAL"
    ) || key.starts_with("DYLD_")
        || key.ends_with("_COMMAND")
        || key.ends_with("_EXECUTABLE")
        || key.ends_with("_HELPER")
}

fn strip_jsonc_comments(input: &str) -> Result<String> {
    let bytes = input.as_bytes();
    let mut output = Vec::with_capacity(input.len());
    let mut index = 0;
    let mut in_string = false;
    let mut escaped = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if in_string {
            output.push(byte);
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        if byte == b'"' {
            in_string = true;
            output.push(b'"');
            index += 1;
            continue;
        }
        if byte == b'/' && index + 1 < bytes.len() && bytes[index + 1] == b'/' {
            index += 2;
            while index < bytes.len() && bytes[index] != b'\n' {
                output.push(b' ');
                index += 1;
            }
            continue;
        }
        if byte == b'/' && index + 1 < bytes.len() && bytes[index + 1] == b'*' {
            index += 2;
            let mut closed = false;
            while index + 1 < bytes.len() {
                if bytes[index] == b'*' && bytes[index + 1] == b'/' {
                    index += 2;
                    closed = true;
                    break;
                }
                output.push(if bytes[index] == b'\n' { b'\n' } else { b' ' });
                index += 1;
            }
            if !closed {
                return Err(native_error("native_config_invalid"));
            }
            continue;
        }
        output.push(byte);
        index += 1;
    }
    if in_string {
        return Err(native_error("native_config_invalid"));
    }
    String::from_utf8(output).map_err(|_| native_error("native_config_invalid"))
}

fn strip_jsonc_trailing_commas(input: &str) -> Result<String> {
    let bytes = input.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    let mut in_string = false;
    let mut escaped = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if in_string {
            output.push(byte);
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        if byte == b'"' {
            in_string = true;
            output.push(byte);
            index += 1;
            continue;
        }
        if byte == b',' {
            let mut lookahead = index + 1;
            while lookahead < bytes.len() && bytes[lookahead].is_ascii_whitespace() {
                lookahead += 1;
            }
            if lookahead < bytes.len() && matches!(bytes[lookahead], b'}' | b']') {
                index += 1;
                continue;
            }
        }
        output.push(byte);
        index += 1;
    }
    String::from_utf8(output).map_err(|_| native_error("native_config_invalid"))
}

pub(crate) fn sha256_hex(content: &[u8]) -> String {
    let digest = Sha256::digest(content);
    format!("{digest:x}")
}

fn native_error(code: &'static str) -> StackError {
    StackError::NativeAgentConfig { code }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opencode_config(provider: &str, model: &str) -> Config {
        let mut config = crate::config::load_config_from_str(include_str!(
            "../../../tests/fixtures/valid-opencode-stack.toml"
        ))
        .expect("fixture config");
        config.agent.env.clear();
        apply_mapped_agent_provider(&mut config, provider, None).expect("provider");
        config.agent.provider.as_mut().expect("provider").model = Some(model.to_owned());
        config
    }

    #[test]
    fn claude_strips_managed_and_blocked_fields_but_keeps_unmanaged() {
        let inspected = inspect_native_config(
            "claude-code",
            Some("settings.json"),
            r#"{
              "model":"claude-sonnet",
              "apiKeyHelper":"printenv SECRET",
              "env":{"ANTHROPIC_API_KEY":"literal","KEEP_ME":"yes"},
              "permissions":{"allow":["Bash(*)"]},
              "hooks":{"Stop":[{"hooks":[{"command":"notify"}]}]},
              "theme":"dark"
            }"#,
        )
        .expect("inspect");
        let manifest = inspected.inspection();
        assert!(
            manifest
                .managed_fields
                .iter()
                .any(|field| field.id == "model")
        );
        assert!(
            manifest
                .blocked_fields
                .iter()
                .any(|field| field.path == "apiKeyHelper")
        );
        assert!(
            manifest
                .blocked_fields
                .iter()
                .any(|field| field.path == "env.ANTHROPIC_API_KEY")
        );
        assert_eq!(
            manifest.executable_categories,
            vec![ExecutableCategory::Hooks]
        );
        let residual: JsonValue = serde_json::from_slice(inspected.residual()).expect("json");
        assert_eq!(residual["env"]["KEEP_ME"], "yes");
        assert_eq!(residual["theme"], "dark");
        assert!(residual.get("model").is_none());
        assert!(residual.get("apiKeyHelper").is_none());
        assert!(residual.get("permissions").is_none());
    }

    #[test]
    fn claude_blocks_security_and_credential_controls_and_flags_command_helpers() {
        let inspected = inspect_native_config(
            "claude-code",
            Some("settings.json"),
            r#"{
              "defaultMode":"bypassPermissions",
              "skipDangerousModePermissionPrompt":true,
              "forceLoginMethod":"claudeai",
              "awsCredentialExport":"/tmp/export-creds",
              "policyHelper":{"path":"/tmp/policy"},
              "agent":"reviewer",
              "fileSuggestion":{"type":"command","command":"/tmp/suggest"},
              "theme":"dark"
            }"#,
        )
        .expect("inspect");
        let manifest = inspected.inspection();
        for path in [
            "defaultMode",
            "skipDangerousModePermissionPrompt",
            "forceLoginMethod",
            "awsCredentialExport",
            "policyHelper",
            "agent",
        ] {
            assert!(
                manifest
                    .blocked_fields
                    .iter()
                    .any(|field| field.path == path),
                "missing blocked path {path}"
            );
        }
        assert!(
            manifest
                .executable_categories
                .contains(&ExecutableCategory::CommandHelpers)
        );
        let residual: JsonValue = serde_json::from_slice(inspected.residual()).expect("json");
        assert!(residual.get("defaultMode").is_none());
        assert!(residual.get("forceLoginMethod").is_none());
        assert!(residual.get("awsCredentialExport").is_none());
        assert_eq!(residual["fileSuggestion"]["command"], "/tmp/suggest");
        assert_eq!(residual["theme"], "dark");
    }

    #[test]
    fn claude_blocks_literal_telemetry_credentials_and_flags_otel_helper() {
        let inspected = inspect_native_config(
            "claude-code",
            Some("settings.json"),
            r#"{
              "env": {
                "OTEL_EXPORTER_OTLP_HEADERS":"Authorization=Bearer literal",
                "HTTPS_PROXY":"https://user:password@example.com",
                "LANG":"en_US.UTF-8"
              },
              "otelHeadersHelper":"/tmp/headers-helper"
            }"#,
        )
        .expect("inspect");
        assert!(
            inspected
                .inspection()
                .blocked_fields
                .iter()
                .any(|field| field.path == "env.OTEL_EXPORTER_OTLP_HEADERS")
        );
        assert!(
            inspected
                .inspection()
                .blocked_fields
                .iter()
                .any(|field| field.path == "env.HTTPS_PROXY")
        );
        assert!(
            inspected
                .inspection()
                .executable_categories
                .contains(&ExecutableCategory::CommandHelpers)
        );
        let residual: JsonValue = serde_json::from_slice(inspected.residual()).expect("json");
        assert_eq!(residual["env"]["LANG"], "en_US.UTF-8");
        assert!(residual["env"].get("HTTPS_PROXY").is_none());
    }

    #[test]
    fn codex_classifies_provider_model_and_simple_mcp() {
        let inspected = inspect_native_config(
            "codex",
            Some("config.toml"),
            r#"
model = "gpt-5.5"
model_provider = "openai"
approval_policy = "never"
notify = ["notify-send"]

[mcp_servers.local]
command = "npx"
args = ["-y", "server"]
env_vars = ["MCP_TOKEN"]

[features]
web_search = true
"#,
        )
        .expect("inspect");
        let manifest = inspected.inspection();
        assert!(
            manifest
                .managed_fields
                .iter()
                .any(|field| field.id == "provider")
        );
        assert!(
            manifest
                .managed_fields
                .iter()
                .any(|field| field.id == "model")
        );
        assert!(
            manifest
                .managed_fields
                .iter()
                .any(|field| field.id == "mcp:local")
        );
        assert!(
            manifest
                .blocked_fields
                .iter()
                .any(|field| field.path == "approval_policy")
        );
        assert!(
            manifest
                .executable_categories
                .contains(&ExecutableCategory::Notifications)
        );
        assert!(
            manifest
                .executable_categories
                .contains(&ExecutableCategory::CommandHelpers)
        );
        let residual = std::str::from_utf8(inspected.residual()).expect("utf8");
        assert!(residual.contains("[features]"));
        assert!(!residual.contains("model_provider"));
        assert!(!residual.contains("mcp_servers"));
    }

    #[test]
    fn credential_scans_catch_value_style_and_non_suffix_shapes() {
        fn owned(args: &[&str]) -> Vec<String> {
            args.iter().map(|arg| (*arg).to_owned()).collect()
        }
        assert!(command_args_contain_literal_credentials(&owned(&[
            "-H",
            "Authorization: Bearer sk-live-1234"
        ])));
        assert!(command_args_contain_literal_credentials(&owned(&[
            "--header",
            "x-api-key: sk-live-1234"
        ])));
        assert!(command_args_contain_literal_credentials(&owned(&[
            "Bearer sk-live-1234"
        ])));
        assert!(!command_args_contain_literal_credentials(&owned(&[
            "--url",
            "https://example.com/mcp"
        ])));
        assert!(!command_args_contain_literal_credentials(&owned(&[
            "-H",
            "content-type: application/json"
        ])));

        assert_eq!(
            sensitive_field_reason("apiKeyId"),
            Some(BlockedReason::Credentials)
        );
        assert_eq!(
            sensitive_field_reason("tokenValue"),
            Some(BlockedReason::Credentials)
        );
        assert_eq!(
            sensitive_field_reason("secretRef"),
            Some(BlockedReason::Credentials)
        );
        assert_eq!(
            sensitive_field_reason("clientSecretId"),
            Some(BlockedReason::Credentials)
        );
        assert_eq!(
            sensitive_field_reason("authToken"),
            Some(BlockedReason::Credentials)
        );
        assert_eq!(sensitive_field_reason("model_max_output_tokens"), None);
        assert_eq!(sensitive_field_reason("max_tokens"), None);
        assert_eq!(
            sensitive_field_reason("model_auto_compact_token_limit"),
            None
        );
        assert_eq!(sensitive_field_reason("tool_output_token_limit"), None);
        assert_eq!(sensitive_field_reason("prefill_token_weight"), None);
        assert_eq!(sensitive_field_reason("includeCoAuthoredBy"), None);
        assert_eq!(sensitive_field_reason("keybinds"), None);
        assert_eq!(sensitive_field_reason("theme"), None);

        assert!(!mcp_http_url_is_credential_free(
            "https://example.com/mcp/sk-live-abcdef123456"
        ));
        assert!(!mcp_http_url_is_credential_free(
            "https://example.com/t/ghp_16charsofpayload"
        ));
        assert!(mcp_http_url_is_credential_free(
            "https://example.com/mcp/sk-learn"
        ));
        assert!(mcp_http_url_is_credential_free("https://example.com/mcp"));
    }

    #[test]
    fn neutral_fields_with_literal_credentials_are_removed_across_formats() {
        let json = inspect_native_config(
            "opencode",
            Some("opencode.json"),
            r#"{"metadata":{"label":"safe","value":"ghp_16charsofpayload"},"theme":"sk-learn"}"#,
        )
        .expect("json inspect");
        let json_residual: JsonValue =
            serde_json::from_slice(json.residual()).expect("json residual");
        assert!(json_residual.get("metadata").is_none());
        assert_eq!(json_residual["theme"], "sk-learn");
        assert!(json.inspection().blocked_fields.iter().any(|field| {
            field.path == "metadata" && field.reason == BlockedReason::Credentials
        }));

        let toml = inspect_native_config(
            "codex",
            Some("config.toml"),
            "theme = 'sk-learn'\n[metadata]\nvalue = 'Bearer ghp_16charsofpayload'\n",
        )
        .expect("toml inspect");
        let toml_residual: TomlValue =
            toml::from_str(std::str::from_utf8(toml.residual()).expect("toml utf8"))
                .expect("toml residual");
        assert!(toml_residual.get("metadata").is_none());
        assert_eq!(
            toml_residual.get("theme").and_then(TomlValue::as_str),
            Some("sk-learn")
        );

        let yaml = inspect_native_config(
            "goose",
            Some("config.yaml"),
            "metadata:\n  value: https://user:password@example.com/mcp\ntheme: sk-learn\n",
        )
        .expect("yaml inspect");
        let yaml_residual: YamlValue =
            serde_norway::from_slice(yaml.residual()).expect("yaml residual");
        let yaml_residual = yaml_residual.as_mapping().expect("yaml mapping");
        assert!(
            yaml_residual
                .get(YamlValue::String("metadata".to_owned()))
                .is_none()
        );
        assert_eq!(
            yaml_residual.get(YamlValue::String("theme".to_owned())),
            Some(&YamlValue::String("sk-learn".to_owned()))
        );
    }

    #[test]
    fn codex_mcp_maps_real_stdio_schema_and_blocks_unrepresentable_servers() {
        // Field shapes from developers.openai.com/codex/config-sample:
        // timeouts and object-form env_vars are importable; a literal `env`
        // table or a `cwd` cannot be represented in the acps MCP schema.
        let inspected = inspect_native_config(
            "codex",
            Some("config.toml"),
            r#"
[mcp_servers.tuned]
command = "npx"
args = ["-y", "server"]
env_vars = ["MCP_TOKEN", { name = "OTHER_TOKEN", source = "keychain" }]
required = true
startup_timeout_sec = 120
startup_timeout_ms = 120000
tool_timeout_sec = 300
tool_timeout_ms = 300000

[mcp_servers.filtered]
command = "npx"
args = ["-y", "server"]
disabled_tools = ["delete_everything"]

[mcp_servers.literal_env]
command = "npx"
args = ["-y", "server"]
env = { SOME_FLAG = "1" }

[mcp_servers.literal_secret_env]
command = "npx"
args = ["-y", "server"]
env = { WIDGET_API_KEY = "sk-live-1234" }

[mcp_servers.scoped]
command = "npx"
args = ["-y", "server"]
cwd = "/srv/mcp"
"#,
        )
        .expect("inspect");
        let manifest = inspected.inspection();
        assert!(
            manifest
                .managed_fields
                .iter()
                .any(|field| field.id == "mcp:tuned")
        );
        assert!(manifest.blocked_fields.iter().any(|field| {
            field.path == "mcp_servers.literal_env" && field.reason == BlockedReason::McpUnmappable
        }));
        assert!(manifest.blocked_fields.iter().any(|field| {
            field.path == "mcp_servers.literal_secret_env"
                && field.reason == BlockedReason::Credentials
        }));
        assert!(manifest.blocked_fields.iter().any(|field| {
            field.path == "mcp_servers.scoped" && field.reason == BlockedReason::McpUnmappable
        }));
        assert!(manifest.blocked_fields.iter().any(|field| {
            field.path == "mcp_servers.filtered" && field.reason == BlockedReason::McpUnmappable
        }));
        let manifest_json = serde_json::to_string(manifest).expect("manifest json");
        assert!(!manifest_json.contains("sk-live-1234"));
        let residual = std::str::from_utf8(inspected.residual()).expect("utf8");
        assert!(!residual.contains("sk-live-1234"));
        assert!(!residual.contains("mcp_servers"));

        let tuned: TomlValue = toml::from_str(
            r#"
command = "npx"
env_vars = ["MCP_TOKEN", { name = "OTHER_TOKEN", source = "keychain" }]
"#,
        )
        .expect("toml");
        let McpServerConfig::Stdio(stdio) = toml_mcp_server("tuned", &tuned).expect("mappable")
        else {
            panic!("stdio server expected");
        };
        assert_eq!(
            stdio.env,
            vec!["MCP_TOKEN".to_owned(), "OTHER_TOKEN".to_owned()]
        );
    }

    #[test]
    fn amp_maps_dotted_mcp_and_blocks_permission_and_policy_keys() {
        // Field shapes from ampcode.com/manual: flat dotted keys, MCP servers
        // under `amp.mcpServers` with literal `env`/`headers` objects, command
        // allowlists and tool-disable filters that must stay owned by acps.
        let inspected = inspect_native_config(
            "amp",
            Some("settings.json"),
            r#"{
              "amp.mcpServers": {
                "playwright": {"command": "npx", "args": ["-y", "@playwright/mcp@latest"]},
                "linear": {"url": "https://mcp.linear.app/sse"},
                "with_secret_env": {"command": "npx", "env": {"WIDGET_API_KEY": "sk-live-1234"}}
              },
              "amp.commands.allowlist": ["git status", "npm run build"],
              "amp.commands.strict": false,
              "amp.dangerouslyAllowAll": false,
              "amp.tools.disable": ["browser_navigate"],
              "amp.notifications.enabled": true
            }"#,
        )
        .expect("inspect");
        let manifest = inspected.inspection();
        for id in ["mcp:playwright", "mcp:linear"] {
            assert!(
                manifest.managed_fields.iter().any(|field| field.id == id),
                "missing managed field {id}"
            );
        }
        assert!(manifest.blocked_fields.iter().any(|field| {
            field.path == "amp.mcpServers.with_secret_env"
                && field.reason == BlockedReason::Credentials
        }));
        for path in [
            "amp.commands.allowlist",
            "amp.commands.strict",
            "amp.dangerouslyAllowAll",
        ] {
            assert!(
                manifest.blocked_fields.iter().any(|field| {
                    field.path == path && field.reason == BlockedReason::Permissions
                }),
                "missing permission block {path}"
            );
        }
        assert!(manifest.blocked_fields.iter().any(|field| {
            field.path == "amp.tools.disable" && field.reason == BlockedReason::AcpsPolicy
        }));
        let manifest_json = serde_json::to_string(manifest).expect("manifest json");
        assert!(!manifest_json.contains("sk-live-1234"));
        let residual: JsonValue = serde_json::from_slice(inspected.residual()).expect("json");
        // Benign settings survive; secrets and owned keys never do.
        assert_eq!(residual["amp.notifications.enabled"], true);
        assert!(residual.get("amp.mcpServers").is_none());
        assert!(residual.get("amp.commands.allowlist").is_none());
        assert!(residual.get("amp.tools.disable").is_none());
        assert!(
            !std::str::from_utf8(inspected.residual())
                .expect("utf8")
                .contains("sk-live-1234")
        );
    }

    #[test]
    fn pi_maps_provider_model_and_blocks_exec_permission_credential_keys() {
        // Field shapes from earendil-works/pi settings.md: `defaultProvider`
        // (bare provider id) and `defaultModel` (bare model id) select the
        // lane; `shellPath`/`npmCommand` run commands; `packages`/`skills`
        // load third-party code; `defaultProjectTrust` is a permission; a
        // credential-bearing `httpProxy` and any literal secret must never
        // survive; benign UI/thinking keys do.
        let inspected = inspect_native_config(
            "pi",
            Some("settings.json"),
            r#"{
              "defaultProvider": "anthropic",
              "defaultModel": "claude-sonnet-4-20250514",
              "shellPath": "/bin/zsh",
              "npmCommand": ["pnpm", "install"],
              "packages": ["@acme/pkg"],
              "skills": ["/home/user/skills/foo"],
              "defaultProjectTrust": "trusted",
              "httpProxy": "http://user:pass@proxy.internal:8080",
              "trackingId": "sk-live-1234",
              "defaultThinkingLevel": "high",
              "theme": "dark"
            }"#,
        )
        .expect("inspect");
        let manifest = inspected.inspection();
        let provider = manifest
            .managed_fields
            .iter()
            .find(|field| field.id == "provider")
            .expect("provider candidate");
        assert_eq!(provider.kind, ManagedFieldKind::Provider);
        assert!(provider.compatible);
        let model = manifest
            .managed_fields
            .iter()
            .find(|field| field.id == "model")
            .expect("model candidate");
        assert_eq!(model.kind, ManagedFieldKind::Model);
        assert!(model.compatible);
        assert!(manifest.blocked_fields.iter().any(|field| {
            field.path == "defaultProjectTrust" && field.reason == BlockedReason::Permissions
        }));
        assert!(
            manifest.blocked_fields.iter().any(
                |field| field.path == "httpProxy" && field.reason == BlockedReason::Credentials
            )
        );
        // `trackingId` ends in `id` but Pi documents it as an analytics id;
        // it is not a credential key, so it survives. The literal value must
        // still never leak through a managed/blocked field.
        for category in [
            ExecutableCategory::CommandHelpers,
            ExecutableCategory::Plugins,
        ] {
            assert!(
                manifest.executable_categories.contains(&category),
                "missing executable category {category:?}"
            );
        }
        let manifest_json = serde_json::to_string(manifest).expect("manifest json");
        assert!(!manifest_json.contains("anthropic"));
        assert!(!manifest_json.contains("claude-sonnet-4-20250514"));
        assert!(!manifest_json.contains("pass@proxy"));
        let residual: JsonValue = serde_json::from_slice(inspected.residual()).expect("json");
        // Benign keys survive; managed/owned/credential keys never do.
        assert_eq!(residual["defaultThinkingLevel"], "high");
        assert_eq!(residual["theme"], "dark");
        assert!(residual.get("defaultProvider").is_none());
        assert!(residual.get("defaultModel").is_none());
        assert!(residual.get("defaultProjectTrust").is_none());
        assert!(residual.get("httpProxy").is_none());
        // Executable roots stay in the residual (they are provisioned/owned
        // downstream, not credentials) but are flagged via executable
        // categories so selection requires acknowledgement.
        assert!(residual.get("packages").is_some());
    }

    #[test]
    fn pi_unmappable_provider_is_incompatible_candidate() {
        // A provider Pi supports but acps does not map for `pi` yields an
        // incompatible candidate rather than a hard failure, matching the
        // opencode/codex unmappable-provider path.
        let inspected = inspect_native_config(
            "pi",
            Some("settings.json"),
            r#"{"defaultProvider": "totally-unknown-provider"}"#,
        )
        .expect("inspect");
        let provider = inspected
            .inspection()
            .managed_fields
            .iter()
            .find(|field| field.id == "provider")
            .expect("provider candidate");
        assert!(!provider.compatible);
    }

    #[test]
    fn goose_maps_provider_model_extensions_and_blocks_mode_planner_credentials() {
        // Field shapes from block/goose config-files.md + extension.rs:
        // `GOOSE_PROVIDER`/`GOOSE_MODEL` select the lane; `extensions` carries
        // stdio (`cmd`/`args`/`env_keys`) and remote (`streamable_http`/`uri`)
        // MCP servers; `envs` literal tables and `builtin`/disabled extensions
        // block; `GOOSE_MODE`/`GOOSE_ALLOWLIST` are permissions; planner keys
        // are managed-unsupported; benign tuning keys survive.
        let inspected = inspect_native_config(
            "goose",
            Some("config.yaml"),
            r#"
GOOSE_PROVIDER: anthropic
GOOSE_MODEL: claude-sonnet-4-5
GOOSE_MODE: auto
GOOSE_ALLOWLIST: https://example.com/allowlist.yaml
GOOSE_PLANNER_MODEL: gpt-5.5
GOOSE_TEMPERATURE: 0.2
GOOSE_CONTEXT_STRATEGY: summarize
extensions:
  fetcher:
    type: stdio
    cmd: uvx
    args: ["mcp-server-fetch"]
    env_keys: ["FETCH_PROXY"]
    timeout: 300
    bundled: false
    enabled: true
  literal_env:
    type: stdio
    cmd: run
    envs:
      OPENAI_API_KEY: sk-live-abc
  remote:
    type: streamable_http
    uri: https://mcp.example.com/sse
  builtin_dev:
    type: builtin
    name: developer
  disabled:
    type: stdio
    cmd: off
    enabled: false
"#,
        )
        .expect("inspect");
        let manifest = inspected.inspection();
        assert_eq!(manifest.format, NativeConfigFormat::Yaml);

        let provider = manifest
            .managed_fields
            .iter()
            .find(|field| field.id == "provider")
            .expect("provider candidate");
        assert_eq!(provider.kind, ManagedFieldKind::Provider);
        assert!(provider.compatible);
        let model = manifest
            .managed_fields
            .iter()
            .find(|field| field.id == "model")
            .expect("model candidate");
        assert_eq!(model.kind, ManagedFieldKind::Model);

        // stdio extension with `env_keys` (name-forwarding) maps cleanly.
        assert!(
            manifest
                .managed_fields
                .iter()
                .any(|field| field.id == "mcp:fetcher" && field.compatible)
        );
        // remote streamable_http with a credential-free uri maps to http.
        assert!(
            manifest
                .managed_fields
                .iter()
                .any(|field| field.id == "mcp:remote")
        );
        // literal `envs` carrying a sensitive key blocks as credentials.
        assert!(manifest.blocked_fields.iter().any(|field| {
            field.path == "extensions.literal_env" && field.reason == BlockedReason::Credentials
        }));
        // builtin extension has nothing external to run.
        assert!(manifest.blocked_fields.iter().any(|field| {
            field.path == "extensions.builtin_dev" && field.reason == BlockedReason::McpUnmappable
        }));
        // disabled extension is unmappable.
        assert!(manifest.blocked_fields.iter().any(|field| {
            field.path == "extensions.disabled" && field.reason == BlockedReason::McpUnmappable
        }));
        // GOOSE_MODE / GOOSE_ALLOWLIST are permission surfaces.
        assert!(
            manifest
                .blocked_fields
                .iter()
                .any(|field| field.path == "GOOSE_MODE"
                    && field.reason == BlockedReason::Permissions)
        );
        assert!(manifest.blocked_fields.iter().any(|field| {
            field.path == "GOOSE_ALLOWLIST" && field.reason == BlockedReason::Permissions
        }));
        // Planner model is a second lane acps cannot provision.
        assert!(manifest.blocked_fields.iter().any(|field| {
            field.path == "GOOSE_PLANNER_MODEL" && field.reason == BlockedReason::ManagedUnsupported
        }));
        // stdio extensions surface as command-helper executables.
        assert!(
            manifest
                .executable_categories
                .contains(&ExecutableCategory::CommandHelpers)
        );

        // The manifest is value-free: no provider/model/secret literals leak.
        let manifest_json = serde_json::to_string(manifest).expect("manifest json");
        assert!(!manifest_json.contains("anthropic"));
        assert!(!manifest_json.contains("claude-sonnet-4-5"));
        assert!(!manifest_json.contains("sk-live-abc"));

        // Residual keeps only benign keys and round-trips as valid YAML.
        let residual: YamlValue =
            serde_norway::from_str(std::str::from_utf8(inspected.residual()).expect("utf8"))
                .expect("residual yaml parses");
        let residual = residual.as_mapping().expect("residual mapping");
        assert_eq!(
            residual.get(YamlValue::String("GOOSE_TEMPERATURE".to_owned())),
            Some(&YamlValue::Number(serde_norway::Number::from(0.2)))
        );
        assert_eq!(
            residual.get(YamlValue::String("GOOSE_CONTEXT_STRATEGY".to_owned())),
            Some(&YamlValue::String("summarize".to_owned()))
        );
        // Managed, permission, planner, and MCP keys never survive.
        for key in [
            "GOOSE_PROVIDER",
            "GOOSE_MODEL",
            "GOOSE_MODE",
            "GOOSE_ALLOWLIST",
            "GOOSE_PLANNER_MODEL",
            "extensions",
        ] {
            assert!(
                residual.get(YamlValue::String(key.to_owned())).is_none(),
                "residual leaked {key}"
            );
        }
    }

    #[test]
    fn goose_unmappable_provider_is_incompatible_candidate() {
        let inspected = inspect_native_config(
            "goose",
            Some("config.yaml"),
            "GOOSE_PROVIDER: totally-unknown-provider\n",
        )
        .expect("inspect");
        let provider = inspected
            .inspection()
            .managed_fields
            .iter()
            .find(|field| field.id == "provider")
            .expect("provider candidate");
        assert!(!provider.compatible);
    }

    #[test]
    fn goose_invalid_yaml_and_non_string_keys_are_redacted_errors() {
        // Malformed YAML must fail closed with a redacted error.
        let error = inspect_native_config(
            "goose",
            Some("config.yaml"),
            "GOOSE_PROVIDER: [unterminated",
        )
        .err()
        .expect("invalid yaml rejected");
        assert_eq!(error.error_code(), "native_config_invalid");

        // A mapping with a non-string key has no JSON representation and no
        // legitimate place in a Goose config; reject it without echoing the
        // sensitive value that follows.
        let error = inspect_native_config(
            "goose",
            Some("config.yaml"),
            "123: sk-live-should-not-leak\n",
        )
        .err()
        .expect("non-string key rejected");
        assert_eq!(error.error_code(), "native_config_invalid");
        assert!(!error.public_message().contains("sk-live-should-not-leak"));
    }

    #[test]
    fn opencode_accepts_jsonc_and_normalizes_to_json() {
        let inspected = inspect_native_config(
            "opencode",
            Some("opencode.jsonc"),
            r#"{
              // selection
              "model": "openai/gpt-5.5",
              "permission": "allow",
              "plugin": ["file:///workspace/plugin.js"],
              "theme": "dark",
            }"#,
        )
        .expect("inspect");
        assert_eq!(inspected.inspection().format, NativeConfigFormat::Jsonc);
        assert!(
            inspected
                .inspection()
                .warnings
                .contains(&"jsonc-normalized".to_owned())
        );
        assert!(
            inspected
                .inspection()
                .managed_fields
                .iter()
                .any(|field| field.id == "provider")
        );
        assert!(
            inspected
                .inspection()
                .managed_fields
                .iter()
                .any(|field| field.id == "model")
        );
        assert!(
            inspected
                .inspection()
                .blocked_fields
                .iter()
                .any(|field| field.path == "permission")
        );
        assert!(
            inspected
                .inspection()
                .executable_categories
                .contains(&ExecutableCategory::Plugins)
        );
        let residual: JsonValue = serde_json::from_slice(inspected.residual()).expect("json");
        assert_eq!(residual["theme"], "dark");
        assert!(residual.get("model").is_none());
    }

    #[test]
    fn invalid_jsonc_and_oversize_inputs_are_redacted_errors() {
        let error = match inspect_native_config("opencode", Some("opencode.jsonc"), "{/* secret") {
            Ok(_) => panic!("invalid config was accepted"),
            Err(error) => error,
        };
        assert_eq!(error.error_code(), "native_config_invalid");
        assert!(!error.public_message().contains("secret"));
        let oversized = "x".repeat(IMPORT_SIZE_LIMIT + 1);
        let error = match inspect_native_config("codex", Some("config.toml"), &oversized) {
            Ok(_) => panic!("oversized config was accepted"),
            Err(error) => error,
        };
        assert_eq!(error.error_code(), "native_config_too_large");
    }

    #[test]
    fn rejects_auth_state_and_project_scope_filenames() {
        for (harness, filename) in [
            ("claude-code", ".claude.json"),
            ("claude-code", "settings.local.json"),
            ("codex", "auth.json"),
            ("codex", ".codex/config.toml"),
            ("opencode", "auth.json"),
            // Pi accepts only `settings.json`: credential-bearing and
            // out-of-scope files must be rejected.
            ("pi", "models.json"),
            ("pi", "auth.json"),
            ("pi", "trust.json"),
            ("pi", "mcp.json"),
            // Goose accepts only `config.yaml`: `secrets.yaml` holds
            // keyring-fallback API keys and `permission.yaml` carries per-tool
            // approval levels, both of which must never import.
            ("goose", "secrets.yaml"),
            ("goose", "permission.yaml"),
        ] {
            let error = inspect_native_config(harness, Some(filename), "{}")
                .err()
                .expect("filename rejected");
            assert_eq!(error.error_code(), "native_config_filename_unsupported");
        }
        let error = inspect_native_config("codex", None, "model = 'gpt'")
            .err()
            .expect("filename required");
        assert_eq!(error.error_code(), "native_config_filename_required");
    }

    #[test]
    fn strips_unknown_credentials_and_managed_agent_controls() {
        let inspected = inspect_native_config(
            "claude-code",
            Some("settings.json"),
            r#"{
                "env": {
                    "OTHER_TOKEN": "literal-secret",
                    "NODE_OPTIONS": "--require ./loader.js",
                    "KEEP": "ok"
                },
                "agents": {"reviewer": {"prompt": "ignore policy"}},
                "theme": "dark"
            }"#,
        )
        .expect("inspect");
        assert!(inspected.inspection().blocked_fields.iter().any(|field| {
            field.path == "env.OTHER_TOKEN" && field.reason == BlockedReason::Credentials
        }));
        assert!(
            inspected
                .inspection()
                .blocked_fields
                .iter()
                .any(|field| field.path == "agents")
        );
        assert!(
            inspected
                .inspection()
                .executable_categories
                .contains(&ExecutableCategory::CommandHelpers)
        );
        let residual: JsonValue = serde_json::from_slice(inspected.residual()).expect("json");
        assert!(residual["env"].get("OTHER_TOKEN").is_none());
        assert_eq!(residual["env"]["KEEP"], "ok");
        assert!(residual.get("agents").is_none());

        let inspected = inspect_native_config(
            "opencode",
            Some("opencode.json"),
            r#"{"agent":{"review":{"tools":{"bash":true},"prompt":"unsafe"}},"theme":"dark"}"#,
        )
        .expect("inspect");
        assert!(
            inspected
                .inspection()
                .blocked_fields
                .iter()
                .any(|field| field.path == "agent")
        );
        let residual: JsonValue = serde_json::from_slice(inspected.residual()).expect("json");
        assert!(residual.get("agent").is_none());
    }

    #[test]
    fn jsonc_normalization_preserves_unicode() {
        let inspected = inspect_native_config(
            "opencode",
            Some("opencode.jsonc"),
            "{ // comment\n \"theme\": \"暗色\",\n}",
        )
        .expect("inspect");
        let residual: JsonValue = serde_json::from_slice(inspected.residual()).expect("json");
        assert_eq!(residual["theme"], "暗色");
    }

    #[test]
    fn default_selection_preserves_canonical_provider_and_selected_values_replace_it() {
        let current = opencode_config("openrouter", "openrouter/old-model");
        let inspected = inspect_native_config(
            "opencode",
            Some("opencode.json"),
            r#"{"model":"openai/new-model","theme":"dark"}"#,
        )
        .expect("inspect");
        let default = prepare_native_config_import(
            &inspected,
            &NativeConfigSelection {
                revision: inspected.revision().to_owned(),
                selected_managed_field_ids: Vec::new(),
                executable_settings_acknowledged: false,
            },
            &current,
            Path::new("/tmp/home"),
        )
        .expect("prepare default");
        assert_eq!(
            default
                .canonical_config
                .agent
                .provider
                .as_ref()
                .expect("provider")
                .id,
            "openrouter"
        );
        assert_eq!(
            default
                .canonical_config
                .agent
                .provider
                .as_ref()
                .and_then(|provider| provider.model.as_deref()),
            Some("openrouter/old-model")
        );

        let selected = prepare_native_config_import(
            &inspected,
            &NativeConfigSelection {
                revision: inspected.revision().to_owned(),
                selected_managed_field_ids: vec!["provider".to_owned(), "model".to_owned()],
                executable_settings_acknowledged: false,
            },
            &current,
            Path::new("/tmp/home"),
        )
        .expect("prepare selected");
        let provider = selected
            .canonical_config
            .agent
            .provider
            .as_ref()
            .expect("provider");
        assert_eq!(provider.id, "openai");
        assert_eq!(provider.model.as_deref(), Some("openai/new-model"));
        assert!(
            selected
                .canonical_config
                .agent
                .env
                .iter()
                .any(|name| name == "OPENAI_API_KEY")
        );

        let error = prepare_native_config_import(
            &inspected,
            &NativeConfigSelection {
                revision: inspected.revision().to_owned(),
                selected_managed_field_ids: vec!["model".to_owned()],
                executable_settings_acknowledged: false,
            },
            &current,
            Path::new("/tmp/home"),
        )
        .err()
        .expect("model/provider mismatch");
        assert_eq!(error.error_code(), "native_config_model_provider_mismatch");
    }

    #[test]
    fn executable_acknowledgement_is_revision_bound() {
        let current = opencode_config("openrouter", "openrouter/old-model");
        let inspected = inspect_native_config(
            "opencode",
            Some("opencode.json"),
            r#"{"plugin":["file:///tmp/plugin.js"],"theme":"dark"}"#,
        )
        .expect("inspect");
        let error = prepare_native_config_import(
            &inspected,
            &NativeConfigSelection {
                revision: inspected.revision().to_owned(),
                selected_managed_field_ids: Vec::new(),
                executable_settings_acknowledged: false,
            },
            &current,
            Path::new("/tmp/home"),
        )
        .err()
        .expect("ack required");
        assert_eq!(error.error_code(), "native_config_executable_ack_required");

        let error = validate_native_config_selection(
            &inspected,
            &NativeConfigSelection {
                revision: "different".to_owned(),
                selected_managed_field_ids: Vec::new(),
                executable_settings_acknowledged: true,
            },
        )
        .unwrap_err();
        assert_eq!(error.error_code(), "native_config_revision_mismatch");
    }

    #[test]
    fn opencode_lsp_requires_executable_acknowledgement() {
        let current = opencode_config("openrouter", "openrouter/old-model");
        let inspected = inspect_native_config(
            "opencode",
            Some("opencode.json"),
            r#"{"lsp":{"custom":{"command":["/tmp/custom-lsp"]}}}"#,
        )
        .expect("inspect");
        assert!(
            inspected
                .inspection()
                .executable_categories
                .contains(&ExecutableCategory::CommandHelpers)
        );
        let error = prepare_native_config_import(
            &inspected,
            &NativeConfigSelection {
                revision: inspected.revision().to_owned(),
                selected_managed_field_ids: Vec::new(),
                executable_settings_acknowledged: false,
            },
            &current,
            Path::new("/tmp/home"),
        )
        .err()
        .expect("LSP command requires acknowledgement");
        assert_eq!(error.error_code(), "native_config_executable_ack_required");
    }

    #[test]
    fn executable_mcp_acknowledgement_is_required_only_when_selected() {
        let current = opencode_config("openrouter", "openrouter/old-model");
        let inspected = inspect_native_config(
            "opencode",
            Some("opencode.json"),
            r#"{"mcp":{"local":{"type":"local","command":["echo","ok"]}},"theme":"dark"}"#,
        )
        .expect("inspect");
        prepare_native_config_import(
            &inspected,
            &NativeConfigSelection {
                revision: inspected.revision().to_owned(),
                selected_managed_field_ids: Vec::new(),
                executable_settings_acknowledged: false,
            },
            &current,
            Path::new("/tmp/home"),
        )
        .expect("unselected executable candidate is removed");

        let error = prepare_native_config_import(
            &inspected,
            &NativeConfigSelection {
                revision: inspected.revision().to_owned(),
                selected_managed_field_ids: vec!["mcp:local".to_owned()],
                executable_settings_acknowledged: false,
            },
            &current,
            Path::new("/tmp/home"),
        )
        .err()
        .expect("selected executable candidate requires acknowledgement");
        assert_eq!(error.error_code(), "native_config_executable_ack_required");
    }

    #[test]
    fn unmappable_mcp_never_survives_in_residual() {
        let inspected = inspect_native_config(
            "opencode",
            Some("opencode.json"),
            r#"{
                "mcp":{"remote":{"url":"https://example.com","headers":{"Authorization":"literal"}}},
                "theme":"dark"
            }"#,
        )
        .expect("inspect");
        assert!(inspected.inspection().blocked_fields.iter().any(|field| {
            field.path == "mcp.remote" && field.reason == BlockedReason::McpUnmappable
        }));
        let residual: JsonValue = serde_json::from_slice(inspected.residual()).expect("json");
        assert!(residual.get("mcp").is_none());
    }

    #[test]
    fn mcp_urls_and_arguments_cannot_embed_literal_credentials() {
        let inspected = inspect_native_config(
            "codex",
            Some("config.toml"),
            r#"
[mcp_servers.remote]
url = "https://example.com/sse?access_token=literal"

[mcp_servers.local]
command = "server"
args = ["--api-key", "literal"]

[mcp_servers.positional]
command = "server"
args = ["ghp_16charsofpayload"]

[mcp_servers.benign]
command = "server"
args = ["sk-learn"]
"#,
        )
        .expect("inspect");
        for path in [
            "mcp_servers.remote",
            "mcp_servers.local",
            "mcp_servers.positional",
        ] {
            assert!(
                inspected.inspection().blocked_fields.iter().any(|field| {
                    field.path == path && field.reason == BlockedReason::Credentials
                })
            );
        }
        assert!(
            inspected
                .inspection()
                .managed_fields
                .iter()
                .any(|field| field.id == "mcp:benign")
        );
    }

    #[test]
    fn manifest_path_collections_are_bounded() {
        let mut root = JsonMap::new();
        for index in 0..(MAX_MANIFEST_PATHS + 50) {
            root.insert(
                format!("credential_{index}"),
                JsonValue::String("x".to_owned()),
            );
        }
        let source = serde_json::to_string(&root).expect("json");
        let inspected =
            inspect_native_config("opencode", Some("opencode.json"), &source).expect("inspect");
        assert_eq!(
            inspected.inspection().blocked_fields.len(),
            MAX_MANIFEST_PATHS
        );
        assert!(
            inspected
                .inspection()
                .warnings
                .contains(&"manifest-truncated".to_owned())
        );
    }

    #[test]
    fn rebase_keeps_selected_values_and_accepts_later_canonical_changes() {
        let current = opencode_config("openrouter", "openrouter/old-model");
        let inspected = inspect_native_config(
            "opencode",
            Some("opencode.json"),
            r#"{"model":"openai/new-model","theme":"dark"}"#,
        )
        .expect("inspect");
        let mut prepared = prepare_native_config_import(
            &inspected,
            &NativeConfigSelection {
                revision: inspected.revision().to_owned(),
                selected_managed_field_ids: vec!["provider".to_owned(), "model".to_owned()],
                executable_settings_acknowledged: false,
            },
            &current,
            Path::new("/tmp/home"),
        )
        .expect("prepare");
        let mut later = current;
        later.logging.level = "debug".to_owned();
        rebase_prepared_native_config_import(&mut prepared, &later).expect("rebase");
        assert_eq!(prepared.canonical_config.logging.level, "debug");
        let provider = prepared
            .canonical_config
            .agent
            .provider
            .as_ref()
            .expect("provider");
        assert_eq!(provider.id, "openai");
        assert_eq!(provider.model.as_deref(), Some("openai/new-model"));
    }

    #[test]
    fn journal_round_trip_keeps_prepared_transaction_without_raw_manifest_values() {
        let home = tempfile::tempdir().expect("home");
        let config_path = home
            .path()
            .join(".config")
            .join("acp-stack")
            .join("acps-config.toml");
        let state_path = home
            .path()
            .join(".local")
            .join("share")
            .join("acp-stack")
            .join("state.sqlite");
        let current = opencode_config("openrouter", "openrouter/old-model");
        let inspected = inspect_native_config(
            "opencode",
            Some("opencode.json"),
            r#"{"theme":"private-setting"}"#,
        )
        .expect("inspect");
        let prepared = prepare_native_config_import(
            &inspected,
            &NativeConfigSelection {
                revision: inspected.revision().to_owned(),
                selected_managed_field_ids: Vec::new(),
                executable_settings_acknowledged: false,
            },
            &current,
            home.path(),
        )
        .expect("prepare");
        let record = NativeConfigOperationRecord {
            operation: NativeConfigOperation {
                operation_id: "nci_test_roundtrip".to_owned(),
                status: NativeConfigOperationStatus::Queued,
                harness: "opencode".to_owned(),
                revision: inspected.revision().to_owned(),
                agent_config: native_config_projection(&prepared.canonical_config),
                restart: NativeConfigRestartMetadata {
                    required: true,
                    queued: true,
                    restarted: false,
                    target_id: "opencode".to_owned(),
                },
                error: None,
            },
            transaction_fingerprint: prepared.transaction_fingerprint.clone(),
            prepared: Some(prepared),
            rollback_snapshots: Vec::new(),
            prior_config: None,
            prior_was_running: false,
            applied_file_digests: Vec::new(),
            applied_at: None,
            updated_at: chrono::Utc::now(),
            cancelled: false,
            phase: NativeConfigOperationPhase::Staged,
        };
        persist_native_config_operation(&state_path, &config_path, home.path(), &record)
            .expect("persist");
        let loaded = load_native_config_operation_journal(&state_path, &config_path, home.path())
            .expect("load");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].operation, record.operation);
        assert_eq!(
            loaded[0]
                .prepared
                .as_ref()
                .expect("prepared")
                .native_content,
            record.prepared.as_ref().expect("prepared").native_content
        );
    }

    #[test]
    fn claude_snapshot_journal_excludes_auth_state_and_digest_ignores_it() {
        let home = tempfile::tempdir().expect("home");
        let claude_state = home.path().join(".claude.json");
        prepare_owner_managed_file_path(home.path(), &claude_state).expect("state path");
        atomic_write_owner_only(
            &claude_state,
            br#"{"oauthAccessToken":"never-persist-this","hasCompletedOnboarding":false}"#,
        )
        .expect("state write");
        let snapshots =
            capture_native_config_snapshots(std::slice::from_ref(&claude_state), home.path())
                .expect("snapshot");
        let digests =
            capture_native_config_file_digests(std::slice::from_ref(&claude_state), home.path())
                .expect("digest");

        let config = opencode_config("openrouter", "openrouter/old-model");
        let config_path = home
            .path()
            .join(".config")
            .join("acp-stack")
            .join("acps-config.toml");
        let state_path = home
            .path()
            .join(".local")
            .join("share")
            .join("acp-stack")
            .join("state.sqlite");
        let record = NativeConfigOperationRecord {
            operation: NativeConfigOperation {
                operation_id: "nci_claude_snapshot".to_owned(),
                status: NativeConfigOperationStatus::Failed,
                harness: "claude-code".to_owned(),
                revision: "revision".to_owned(),
                agent_config: native_config_projection(&config),
                restart: NativeConfigRestartMetadata {
                    required: true,
                    queued: true,
                    restarted: false,
                    target_id: "claude-code".to_owned(),
                },
                error: Some(NativeConfigOperationError {
                    code: "native_config_rollback_failed".to_owned(),
                }),
            },
            transaction_fingerprint: "fingerprint".to_owned(),
            prepared: None,
            rollback_snapshots: snapshots.clone(),
            prior_config: Some(config),
            prior_was_running: true,
            applied_file_digests: digests.clone(),
            applied_at: None,
            updated_at: chrono::Utc::now(),
            cancelled: false,
            phase: NativeConfigOperationPhase::RollingBack,
        };
        persist_native_config_operation(&state_path, &config_path, home.path(), &record)
            .expect("persist");
        let journal = std::fs::read_to_string(
            state_path
                .parent()
                .expect("state parent")
                .join(JOURNAL_DIR_NAME)
                .join("nci_claude_snapshot.json"),
        )
        .expect("journal");
        assert!(!journal.contains("never-persist-this"));

        atomic_write_owner_only(
            &claude_state,
            br#"{"oauthAccessToken":"changed","hasCompletedOnboarding":false}"#,
        )
        .expect("unrelated state change");
        validate_native_config_file_digests(&digests, home.path())
            .expect("unrelated auth state is outside the owned digest");
        atomic_write_owner_only(
            &claude_state,
            br#"{"oauthAccessToken":"changed","hasCompletedOnboarding":true}"#,
        )
        .expect("owned state change");
        assert!(validate_native_config_file_digests(&digests, home.path()).is_err());
        restore_native_config_snapshots(&snapshots, home.path()).expect("restore");
        let restored: JsonValue =
            serde_json::from_slice(&std::fs::read(&claude_state).expect("restored state"))
                .expect("restored json");
        assert_eq!(restored["oauthAccessToken"], "changed");
        assert_eq!(restored["hasCompletedOnboarding"], false);
    }

    #[test]
    fn semantic_replacement_and_snapshot_restore_are_atomic_at_file_boundary() {
        let home = tempfile::tempdir().expect("home");
        let config_path = home
            .path()
            .join(".config")
            .join("acp-stack")
            .join("acps-config.toml");
        let native_path = home
            .path()
            .join(".config")
            .join("opencode")
            .join("opencode.json");
        prepare_owner_managed_file_path(home.path(), &config_path).expect("config path");
        prepare_owner_managed_file_path(home.path(), &native_path).expect("native path");
        let current = opencode_config("openrouter", "openrouter/old-model");
        let canonical = current.to_canonical_toml().expect("canonical");
        atomic_write_owner_only(&config_path, canonical.as_bytes()).expect("config");
        atomic_write_owner_only(
            &native_path,
            br#"{"old_unmanaged":true,"model":"stale/model"}"#,
        )
        .expect("native");
        let inspected =
            inspect_native_config("opencode", Some("opencode.json"), r#"{"theme":"dark"}"#)
                .expect("inspect");
        let prepared = prepare_native_config_import(
            &inspected,
            &NativeConfigSelection {
                revision: inspected.revision().to_owned(),
                selected_managed_field_ids: Vec::new(),
                executable_settings_acknowledged: false,
            },
            &current,
            home.path(),
        )
        .expect("prepare");
        let paths = prepare_native_config_file_paths(&prepared, &config_path, home.path())
            .expect("prepare paths");
        let snapshots = capture_native_config_snapshots(&paths, home.path()).expect("snapshots");
        write_native_config_files(&prepared, &config_path, home.path()).expect("write");
        let written: JsonValue =
            serde_json::from_slice(&std::fs::read(&native_path).expect("read native"))
                .expect("json");
        assert_eq!(written["theme"], "dark");
        assert_eq!(written["model"], "openrouter/old-model");
        assert!(written.get("old_unmanaged").is_none());

        restore_native_config_snapshots(&snapshots, home.path()).expect("restore");
        let restored: JsonValue =
            serde_json::from_slice(&std::fs::read(&native_path).expect("read restored"))
                .expect("json");
        assert_eq!(restored["old_unmanaged"], true);
        assert_eq!(
            std::fs::read_to_string(&config_path).expect("config"),
            canonical
        );
    }
}
