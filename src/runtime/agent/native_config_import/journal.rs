//! Durable journal and persistence for native config import operations.

use super::*;

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
