use std::path::Path;

use crate::config::Config;
use crate::error::{Result, StackError};
use crate::runtime::agent::acp_bridge::AgentSessionConfigCategory;
use crate::runtime::agent::model_discovery::{
    fetch_session_config, model_value_is_explicit_without_discovery, validate_advertised_value,
};
use crate::runtime::agent::native_config_import::{
    NativeConfigOperation, NativeConfigOperationError, NativeConfigOperationPhase,
    NativeConfigOperationRecord, NativeConfigOperationStatus, NativeConfigRestartMetadata,
    capture_native_config_file_digests, capture_native_config_snapshots,
    load_native_config_operation_journal, native_config_projection,
    native_config_transaction_paths, persist_native_config_operation,
    prepare_native_config_file_paths, prepare_native_config_import,
    rebase_prepared_native_config_import, restore_native_config_snapshots, sha256_hex,
    validate_native_config_secret_refs, write_native_config_files,
};

use super::PendingInitNativeConfig;

pub(super) fn prepare_for_new_init(
    pending: &mut PendingInitNativeConfig,
    config: &Config,
    home: &Path,
) -> Result<()> {
    pending.prepared = Some(prepare_native_config_import(
        &pending.inspected,
        &pending.selection,
        config,
        home,
    )?);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn stage_for_init(
    pending: Option<&PendingInitNativeConfig>,
    recorded_revision: Option<&str>,
    recorded_operation: Option<&NativeConfigOperation>,
    init_run_id: &str,
    config: &Config,
    config_path: &Path,
    state_path: &Path,
    home: &Path,
) -> Result<Option<NativeConfigOperationRecord>> {
    let Some(revision) = pending
        .map(|pending| pending.selection.revision.as_str())
        .or(recorded_revision)
    else {
        return Ok(None);
    };
    let operation_id = init_operation_id(init_run_id);
    let records = load_native_config_operation_journal(state_path, config_path, home)?;
    if let Some(record) = records
        .into_iter()
        .find(|record| record.operation.operation_id == operation_id)
    {
        if record.operation.revision != revision || record.operation.harness != config.agent.id {
            return Err(StackError::InitRunCorrupted {
                reason: "native config import journal does not match the init run".to_owned(),
            });
        }
        return Ok(Some(record));
    }
    if let Some(operation) = recorded_operation {
        if operation.operation_id != operation_id
            || operation.revision != revision
            || operation.harness != config.agent.id
            || operation.status != NativeConfigOperationStatus::Applied
            || operation.restart.required
            || operation.restart.queued
            || operation.restart.restarted
        {
            return Err(StackError::InitRunCorrupted {
                reason: "recorded native config import result does not match the init run"
                    .to_owned(),
            });
        }
        return Ok(Some(NativeConfigOperationRecord {
            operation: operation.clone(),
            transaction_fingerprint: String::new(),
            prepared: None,
            rollback_snapshots: Vec::new(),
            prior_config: None,
            prior_was_running: false,
            applied_file_digests: Vec::new(),
            applied_at: None,
            updated_at: chrono::Utc::now(),
            cancelled: false,
            phase: NativeConfigOperationPhase::Applied,
        }));
    }
    let pending = pending.ok_or_else(|| StackError::InitRunCorrupted {
        reason: "native config import journal is missing for resumed init".to_owned(),
    })?;
    let prepared = pending
        .prepared
        .clone()
        .ok_or_else(|| StackError::InitRunCorrupted {
            reason: "native config import was not prepared before starter config creation"
                .to_owned(),
        })?;
    if prepared.revision != revision
        || prepared.harness != config.agent.id
        || prepared.base_config_revision != sha256_hex(config.to_canonical_toml()?.as_bytes())
    {
        return Err(StackError::InitRunCorrupted {
            reason: "prepared native config import does not match the starter config".to_owned(),
        });
    }
    let now = chrono::Utc::now();
    let operation = NativeConfigOperation {
        operation_id: operation_id.clone(),
        status: NativeConfigOperationStatus::Queued,
        harness: prepared.harness.clone(),
        revision: prepared.revision.clone(),
        agent_config: native_config_projection(&prepared.canonical_config),
        restart: NativeConfigRestartMetadata {
            required: false,
            queued: false,
            restarted: false,
            target_id: config.array.primary_target.clone(),
        },
        error: None,
    };
    let record = NativeConfigOperationRecord {
        operation,
        transaction_fingerprint: prepared.transaction_fingerprint.clone(),
        prepared: Some(prepared),
        rollback_snapshots: Vec::new(),
        prior_config: None,
        prior_was_running: false,
        applied_file_digests: Vec::new(),
        applied_at: None,
        updated_at: now,
        cancelled: false,
        phase: NativeConfigOperationPhase::Staged,
    };
    persist_native_config_operation(state_path, config_path, home, &record)?;
    Ok(Some(record))
}

pub(super) fn apply_for_init(
    record: &mut NativeConfigOperationRecord,
    config_path: &Path,
    state_path: &Path,
    home: &Path,
) -> Result<(Config, NativeConfigOperation)> {
    if record.phase == NativeConfigOperationPhase::Applied {
        return Ok((
            Config::load_from_path(config_path)?,
            record.operation.clone(),
        ));
    }
    if record.phase == NativeConfigOperationPhase::RollingBack {
        restore_native_config_snapshots(&record.rollback_snapshots, home)?;
        reset_for_retry(record);
        persist_native_config_operation(state_path, config_path, home, record)?;
    }
    if !matches!(
        record.phase,
        NativeConfigOperationPhase::Staged | NativeConfigOperationPhase::Applying
    ) {
        return Err(StackError::InitRunCorrupted {
            reason: "native config import journal has an invalid onboarding phase".to_owned(),
        });
    }
    let prepared = record
        .prepared
        .clone()
        .ok_or_else(|| StackError::InitRunCorrupted {
            reason: "native config import journal omitted its prepared transaction".to_owned(),
        })?;
    validate_native_config_secret_refs(&prepared, home)?;

    if record.phase == NativeConfigOperationPhase::Staged {
        let current = Config::load_from_path(config_path)?;
        if sha256_hex(current.to_canonical_toml()?.as_bytes()) != prepared.base_config_revision
            || current.agent.id != prepared.harness
        {
            return Err(StackError::NativeAgentConfig {
                code: "native_config_base_config_changed",
            });
        }
        let paths = prepare_native_config_file_paths(&prepared, config_path, home)?;
        record.rollback_snapshots = capture_native_config_snapshots(&paths, home)?;
        record.prior_config = Some(current);
        record.updated_at = chrono::Utc::now();
        record.phase = NativeConfigOperationPhase::Applying;
        persist_native_config_operation(state_path, config_path, home, record)?;
    }

    let applying = record.clone();
    let paths = native_config_transaction_paths(
        config_path,
        &prepared.native_path,
        &prepared.harness,
        home,
    );
    let apply_result = (|| -> Result<Vec<_>> {
        write_native_config_files(&prepared, config_path, home)?;
        if prepared.imported_model
            && !model_value_is_explicit_without_discovery(&prepared.canonical_config)
        {
            let model = native_config_projection(&prepared.canonical_config)
                .model
                .ok_or(StackError::NativeAgentConfig {
                    code: "native_config_model_invalid",
                })?;
            let response = fetch_session_config(home, &prepared.canonical_config)?;
            validate_advertised_value(&response, AgentSessionConfigCategory::Model, &model)?;
        }
        capture_native_config_file_digests(&paths, home)
    })();
    let digests = match apply_result {
        Ok(digests) => digests,
        Err(error) => {
            let error_code = error.error_code().to_owned();
            record.operation.status = NativeConfigOperationStatus::Failed;
            record.operation.error = Some(NativeConfigOperationError {
                code: error_code.clone(),
            });
            record.updated_at = chrono::Utc::now();
            record.phase = NativeConfigOperationPhase::RollingBack;
            persist_native_config_operation(state_path, config_path, home, record)?;
            if restore_native_config_snapshots(&record.rollback_snapshots, home).is_err() {
                record.operation.error = Some(NativeConfigOperationError {
                    code: "native_config_rollback_failed".to_owned(),
                });
                record.updated_at = chrono::Utc::now();
                persist_native_config_operation(state_path, config_path, home, record)?;
                return Err(StackError::NativeAgentConfig {
                    code: "native_config_rollback_failed",
                });
            }
            reset_for_retry(record);
            persist_native_config_operation(state_path, config_path, home, record)?;
            return Err(error);
        }
    };
    record.operation.status = NativeConfigOperationStatus::Applied;
    record.operation.agent_config = native_config_projection(&prepared.canonical_config);
    record.operation.restart = NativeConfigRestartMetadata {
        required: false,
        queued: false,
        restarted: false,
        target_id: prepared.canonical_config.array.primary_target.clone(),
    };
    record.operation.error = None;
    record.prepared = None;
    record.applied_file_digests = digests;
    record.applied_at = Some(chrono::Utc::now());
    record.updated_at = chrono::Utc::now();
    record.phase = NativeConfigOperationPhase::Applied;
    if let Err(error) = persist_native_config_operation(state_path, config_path, home, record) {
        *record = applying;
        return Err(error);
    }
    Ok((
        Config::load_from_path(config_path)?,
        record.operation.clone(),
    ))
}

pub(super) fn cancel_applied_for_init(
    operation_id: &str,
    revision: &str,
    config_path: &Path,
    state_path: &Path,
    home: &Path,
) -> Result<NativeConfigOperation> {
    let mut record = load_native_config_operation_journal(state_path, config_path, home)?
        .into_iter()
        .find(|record| record.operation.operation_id == operation_id)
        .ok_or(StackError::NativeAgentConfig {
            code: "native_config_operation_not_found",
        })?;
    if record.operation.revision != revision {
        return Err(StackError::NativeAgentConfig {
            code: "native_config_revision_mismatch",
        });
    }
    if record.cancelled
        && record.operation.status == NativeConfigOperationStatus::Cancelled
        && record.phase == NativeConfigOperationPhase::Terminal
    {
        return Ok(record.operation);
    }
    if record.phase != NativeConfigOperationPhase::Applied {
        return Err(StackError::NativeAgentConfig {
            code: "native_config_rollback_conflict",
        });
    }
    // Later init steps legitimately rewrite the canonical config (and may
    // reprovision native files) after the onboarding apply, so this rollback
    // does not gate on applied-file digests the way the runtime cancel does:
    // the backend has already decided to fail the init, and restoring the
    // pre-import snapshots is the safest terminal state for the instance.
    let prior_config = record
        .prior_config
        .as_ref()
        .ok_or(StackError::NativeAgentConfig {
            code: "native_config_rollback_failed",
        })?;
    restore_native_config_snapshots(&record.rollback_snapshots, home)?;
    record.operation.status = NativeConfigOperationStatus::Cancelled;
    record.operation.agent_config = native_config_projection(prior_config);
    record.operation.error = None;
    record.operation.restart = NativeConfigRestartMetadata {
        required: false,
        queued: false,
        restarted: false,
        target_id: record.operation.restart.target_id.clone(),
    };
    record.cancelled = true;
    record.prepared = None;
    record.rollback_snapshots.clear();
    record.prior_config = None;
    record.prior_was_running = false;
    record.applied_file_digests.clear();
    record.updated_at = chrono::Utc::now();
    record.phase = NativeConfigOperationPhase::Terminal;
    persist_native_config_operation(state_path, config_path, home, &record)?;
    Ok(record.operation)
}

pub(super) fn rebase_for_init(
    record: &mut NativeConfigOperationRecord,
    current: &Config,
    config_path: &Path,
    state_path: &Path,
    home: &Path,
) -> Result<()> {
    if record.phase == NativeConfigOperationPhase::Applied {
        return Ok(());
    }
    let prepared = record
        .prepared
        .as_mut()
        .ok_or_else(|| StackError::InitRunCorrupted {
            reason: "native config import journal omitted its prepared transaction".to_owned(),
        })?;
    rebase_prepared_native_config_import(prepared, current)?;
    record.transaction_fingerprint = prepared.transaction_fingerprint.clone();
    record.operation.agent_config = native_config_projection(&prepared.canonical_config);
    record.updated_at = chrono::Utc::now();
    persist_native_config_operation(state_path, config_path, home, record)
}

fn reset_for_retry(record: &mut NativeConfigOperationRecord) {
    record.operation.status = NativeConfigOperationStatus::Queued;
    record.operation.error = None;
    record.operation.restart = NativeConfigRestartMetadata {
        required: false,
        queued: false,
        restarted: false,
        target_id: record.operation.restart.target_id.clone(),
    };
    record.rollback_snapshots.clear();
    record.prior_config = None;
    record.prior_was_running = false;
    record.applied_file_digests.clear();
    record.applied_at = None;
    record.updated_at = chrono::Utc::now();
    record.phase = NativeConfigOperationPhase::Staged;
}

fn init_operation_id(run_id: &str) -> String {
    let digest = sha256_hex(run_id.as_bytes());
    format!("nci_init_{}", &digest[..24])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs_util::{atomic_write_owner_only, prepare_owner_managed_file_path};
    use crate::runtime::agent::native_config_import::{
        NativeConfigSelection, inspect_native_config,
    };
    use crate::secrets::SecretStore;

    fn config_without_provider() -> Config {
        let mut config = crate::config::load_config_from_str(include_str!(
            "../../../tests/fixtures/valid-opencode-stack.toml"
        ))
        .expect("config");
        config.agent.env.clear();
        config.agent.provider = None;
        config.agent.model = None;
        config
    }

    // Amp is provider/model-opaque, so its canonical config carries no
    // provider/model; the import path only appends MCP servers. Round-trips
    // through the fixture with the agent id retargeted so no new fixture file
    // is needed.
    fn amp_config() -> Config {
        let mut config = config_without_provider();
        config.agent.id = "amp".to_owned();
        config
    }

    // Pi is provider-selecting, so its starter config carries no provider until
    // the import applies `defaultProvider`/`defaultModel`. Retargeting the
    // fixture to `pi` avoids a new fixture file.
    fn pi_config() -> Config {
        let mut config = config_without_provider();
        config.agent.id = "pi".to_owned();
        config
    }

    // Goose is provider-selecting, so its starter config carries no provider
    // until the import applies `GOOSE_PROVIDER`/`GOOSE_MODEL`. Retargeting the
    // fixture to `goose` avoids a new fixture file.
    fn goose_config() -> Config {
        let mut config = config_without_provider();
        config.agent.id = "goose".to_owned();
        config
    }

    #[test]
    fn onboarding_import_applies_without_restart_and_resumes_from_journal() {
        let home = tempfile::tempdir().expect("home");
        SecretStore::open_or_create(home.path()).expect("secret store");
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
        prepare_owner_managed_file_path(home.path(), &config_path).expect("config path");
        let config = config_without_provider();
        atomic_write_owner_only(
            &config_path,
            config.to_canonical_toml().expect("canonical").as_bytes(),
        )
        .expect("config write");
        let inspected =
            inspect_native_config("opencode", Some("opencode.json"), r#"{"theme":"dark"}"#)
                .expect("inspect");
        let revision = inspected.revision().to_owned();
        let pending = PendingInitNativeConfig {
            inspected,
            selection: NativeConfigSelection {
                revision: revision.clone(),
                selected_managed_field_ids: Vec::new(),
                executable_settings_acknowledged: false,
            },
            prepared: None,
        };
        let mut pending = pending;
        prepare_for_new_init(&mut pending, &config, home.path()).expect("prepare");
        let mut record = stage_for_init(
            Some(&pending),
            Some(&revision),
            None,
            "init-run-1",
            &config,
            &config_path,
            &state_path,
            home.path(),
        )
        .expect("stage")
        .expect("record");
        let (_, operation) =
            apply_for_init(&mut record, &config_path, &state_path, home.path()).expect("apply");
        assert_eq!(operation.status, NativeConfigOperationStatus::Applied);
        assert!(!operation.restart.required);
        assert!(!operation.restart.queued);
        assert!(!operation.restart.restarted);
        let native_path = home
            .path()
            .join(".config")
            .join("opencode")
            .join("opencode.json");
        let native: serde_json::Value =
            serde_json::from_slice(&std::fs::read(native_path).expect("native")).expect("json");
        assert_eq!(native["theme"], "dark");
        crate::runtime::agent::native_config_import::remove_native_config_operation_journal(
            &state_path,
            &operation.operation_id,
        )
        .expect("expire journal");

        let resumed = stage_for_init(
            None,
            Some(&revision),
            Some(&operation),
            "init-run-1",
            &Config::load_from_path(&config_path).expect("config"),
            &config_path,
            &state_path,
            home.path(),
        )
        .expect("resume")
        .expect("record");
        assert_eq!(resumed.phase, NativeConfigOperationPhase::Applied);
    }

    #[test]
    fn cancel_applied_for_init_restores_snapshots_and_is_idempotent() {
        let home = tempfile::tempdir().expect("home");
        SecretStore::open_or_create(home.path()).expect("secret store");
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
        prepare_owner_managed_file_path(home.path(), &config_path).expect("config path");
        let config = config_without_provider();
        atomic_write_owner_only(
            &config_path,
            config.to_canonical_toml().expect("canonical").as_bytes(),
        )
        .expect("config write");
        let pre_import_config = std::fs::read(&config_path).expect("pre-import config");
        let inspected =
            inspect_native_config("opencode", Some("opencode.json"), r#"{"theme":"dark"}"#)
                .expect("inspect");
        let revision = inspected.revision().to_owned();
        let mut pending = PendingInitNativeConfig {
            inspected,
            selection: NativeConfigSelection {
                revision: revision.clone(),
                selected_managed_field_ids: Vec::new(),
                executable_settings_acknowledged: false,
            },
            prepared: None,
        };
        prepare_for_new_init(&mut pending, &config, home.path()).expect("prepare");
        let mut record = stage_for_init(
            Some(&pending),
            Some(&revision),
            None,
            "init-run-cancel",
            &config,
            &config_path,
            &state_path,
            home.path(),
        )
        .expect("stage")
        .expect("record");
        let (_, operation) =
            apply_for_init(&mut record, &config_path, &state_path, home.path()).expect("apply");
        let native_path = home
            .path()
            .join(".config")
            .join("opencode")
            .join("opencode.json");
        assert!(native_path.exists());

        let mismatch = cancel_applied_for_init(
            &operation.operation_id,
            &"f".repeat(64),
            &config_path,
            &state_path,
            home.path(),
        )
        .expect_err("revision mismatch must fail");
        assert_eq!(mismatch.error_code(), "native_config_revision_mismatch");

        let cancelled = cancel_applied_for_init(
            &operation.operation_id,
            &operation.revision,
            &config_path,
            &state_path,
            home.path(),
        )
        .expect("cancel");
        assert_eq!(cancelled.status, NativeConfigOperationStatus::Cancelled);
        assert_eq!(cancelled.agent_config, native_config_projection(&config));
        assert!(!native_path.exists());
        assert_eq!(
            std::fs::read(&config_path).expect("post-cancel config"),
            pre_import_config
        );
        let journal = load_native_config_operation_journal(&state_path, &config_path, home.path())
            .expect("journal");
        let record = journal
            .iter()
            .find(|record| record.operation.operation_id == operation.operation_id)
            .expect("cancelled record");
        assert_eq!(record.phase, NativeConfigOperationPhase::Terminal);
        assert!(record.cancelled);

        // A backend retry of the same cancel must succeed without touching
        // the restored files again.
        let repeated = cancel_applied_for_init(
            &operation.operation_id,
            &operation.revision,
            &config_path,
            &state_path,
            home.path(),
        )
        .expect("repeat cancel");
        assert_eq!(repeated.status, NativeConfigOperationStatus::Cancelled);

        let missing = cancel_applied_for_init(
            "nci_init_missing",
            &operation.revision,
            &config_path,
            &state_path,
            home.path(),
        )
        .expect_err("unknown operation must fail");
        assert_eq!(missing.error_code(), "native_config_operation_not_found");
    }

    #[test]
    fn amp_mcp_only_import_stages_and_applies() {
        let home = tempfile::tempdir().expect("home");
        SecretStore::open_or_create(home.path()).expect("secret store");
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
        prepare_owner_managed_file_path(home.path(), &config_path).expect("config path");
        let config = amp_config();
        atomic_write_owner_only(
            &config_path,
            config.to_canonical_toml().expect("canonical").as_bytes(),
        )
        .expect("config write");
        let inspected = inspect_native_config(
            "amp",
            Some("settings.json"),
            r#"{"amp.mcpServers":{"docs":{"url":"https://mcp.example.com/mcp"}}}"#,
        )
        .expect("inspect");
        let revision = inspected.revision().to_owned();
        let mcp_field_id = inspected
            .inspection()
            .managed_fields
            .iter()
            .find(|field| field.id == "mcp:docs")
            .expect("mcp candidate")
            .id
            .clone();
        let mut pending = PendingInitNativeConfig {
            inspected,
            selection: NativeConfigSelection {
                revision: revision.clone(),
                selected_managed_field_ids: vec![mcp_field_id],
                executable_settings_acknowledged: false,
            },
            prepared: None,
        };
        prepare_for_new_init(&mut pending, &config, home.path()).expect("prepare");
        let mut record = stage_for_init(
            Some(&pending),
            Some(&revision),
            None,
            "init-run-amp",
            &config,
            &config_path,
            &state_path,
            home.path(),
        )
        .expect("stage")
        .expect("record");
        let (_, operation) =
            apply_for_init(&mut record, &config_path, &state_path, home.path()).expect("apply");
        assert_eq!(operation.status, NativeConfigOperationStatus::Applied);
        // Provider stays absent for the provider/model-opaque harness.
        assert!(operation.agent_config.provider.is_none());
        let applied = Config::load_from_path(&config_path).expect("applied config");
        assert!(
            applied
                .mcp
                .servers
                .iter()
                .any(|server| server.name() == "docs")
        );
        let native_path = home
            .path()
            .join(".config")
            .join("amp")
            .join("settings.json");
        assert!(native_path.exists());
    }

    #[test]
    fn pi_provider_import_stages_and_applies() {
        // Selecting the provider (not the model) keeps `imported_model` false
        // so apply does not trigger live model discovery, exercising the full
        // stage/apply/journal round-trip for a provider-selecting harness.
        let home = tempfile::tempdir().expect("home");
        let mut secrets = SecretStore::open_or_create(home.path()).expect("secret store");
        // The anthropic provider lane requires `ANTHROPIC_API_KEY`; apply
        // validates the secret ref exists before writing.
        secrets
            .set("ANTHROPIC_API_KEY", "test-key")
            .expect("seed secret");
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
        prepare_owner_managed_file_path(home.path(), &config_path).expect("config path");
        let config = pi_config();
        atomic_write_owner_only(
            &config_path,
            config.to_canonical_toml().expect("canonical").as_bytes(),
        )
        .expect("config write");
        let inspected = inspect_native_config(
            "pi",
            Some("settings.json"),
            r#"{"defaultProvider":"anthropic","defaultModel":"claude-sonnet-4-20250514","theme":"dark"}"#,
        )
        .expect("inspect");
        let revision = inspected.revision().to_owned();
        let provider_field_id = inspected
            .inspection()
            .managed_fields
            .iter()
            .find(|field| field.id == "provider")
            .expect("provider candidate")
            .id
            .clone();
        let mut pending = PendingInitNativeConfig {
            inspected,
            selection: NativeConfigSelection {
                revision: revision.clone(),
                selected_managed_field_ids: vec![provider_field_id],
                executable_settings_acknowledged: false,
            },
            prepared: None,
        };
        prepare_for_new_init(&mut pending, &config, home.path()).expect("prepare");
        let mut record = stage_for_init(
            Some(&pending),
            Some(&revision),
            None,
            "init-run-pi",
            &config,
            &config_path,
            &state_path,
            home.path(),
        )
        .expect("stage")
        .expect("record");
        let (_, operation) =
            apply_for_init(&mut record, &config_path, &state_path, home.path()).expect("apply");
        assert_eq!(operation.status, NativeConfigOperationStatus::Applied);
        assert_eq!(
            operation.agent_config.provider.as_deref(),
            Some("anthropic")
        );
        let applied = Config::load_from_path(&config_path).expect("applied config");
        assert_eq!(
            applied
                .agent
                .provider
                .as_ref()
                .map(|provider| provider.id.as_str()),
            Some("anthropic")
        );
        // The provider lane wires its API-key env ref into canonical config.
        assert!(
            applied
                .agent
                .env
                .iter()
                .any(|name| name == "ANTHROPIC_API_KEY")
        );
        let native_path = home.path().join(".pi").join("agent").join("settings.json");
        assert!(native_path.exists());
        let native: serde_json::Value =
            serde_json::from_slice(&std::fs::read(native_path).expect("native")).expect("json");
        // The benign residual key survives alongside the provisioned settings.
        assert_eq!(native["theme"], "dark");
        // `defaultProvider`/`defaultModel` are managed and never land in the
        // native residual.
        assert!(native.get("defaultProvider").is_none());
    }

    #[test]
    fn goose_provider_import_stages_and_applies() {
        // Selecting the provider (not the model) keeps `imported_model` false so
        // apply does not trigger live model discovery, and exercises the full
        // stage/apply/journal round-trip plus the goose YAML residual +
        // provisioning composition (residual written first, then
        // `GOOSE_*` provisioning merged into the same `config.yaml`).
        let home = tempfile::tempdir().expect("home");
        let mut secrets = SecretStore::open_or_create(home.path()).expect("secret store");
        // The anthropic provider lane requires `ANTHROPIC_API_KEY`; apply
        // validates the secret ref exists before writing.
        secrets
            .set("ANTHROPIC_API_KEY", "test-key")
            .expect("seed secret");
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
        prepare_owner_managed_file_path(home.path(), &config_path).expect("config path");
        let config = goose_config();
        atomic_write_owner_only(
            &config_path,
            config.to_canonical_toml().expect("canonical").as_bytes(),
        )
        .expect("config write");
        let inspected = inspect_native_config(
            "goose",
            Some("config.yaml"),
            "GOOSE_PROVIDER: anthropic\nGOOSE_MODEL: claude-sonnet-4-5\nGOOSE_TEMPERATURE: 0.2\n",
        )
        .expect("inspect");
        let revision = inspected.revision().to_owned();
        let provider_field_id = inspected
            .inspection()
            .managed_fields
            .iter()
            .find(|field| field.id == "provider")
            .expect("provider candidate")
            .id
            .clone();
        let mut pending = PendingInitNativeConfig {
            inspected,
            selection: NativeConfigSelection {
                revision: revision.clone(),
                selected_managed_field_ids: vec![provider_field_id],
                executable_settings_acknowledged: false,
            },
            prepared: None,
        };
        prepare_for_new_init(&mut pending, &config, home.path()).expect("prepare");
        let mut record = stage_for_init(
            Some(&pending),
            Some(&revision),
            None,
            "init-run-goose",
            &config,
            &config_path,
            &state_path,
            home.path(),
        )
        .expect("stage")
        .expect("record");
        let (_, operation) =
            apply_for_init(&mut record, &config_path, &state_path, home.path()).expect("apply");
        assert_eq!(operation.status, NativeConfigOperationStatus::Applied);
        assert_eq!(
            operation.agent_config.provider.as_deref(),
            Some("anthropic")
        );
        let applied = Config::load_from_path(&config_path).expect("applied config");
        assert_eq!(
            applied
                .agent
                .provider
                .as_ref()
                .map(|provider| provider.id.as_str()),
            Some("anthropic")
        );
        assert!(
            applied
                .agent
                .env
                .iter()
                .any(|name| name == "ANTHROPIC_API_KEY")
        );
        let native_path = home
            .path()
            .join(".config")
            .join("goose")
            .join("config.yaml");
        assert!(native_path.exists());
        let native: serde_norway::Value =
            serde_norway::from_str(&std::fs::read_to_string(native_path).expect("native"))
                .expect("yaml");
        let native = native.as_mapping().expect("mapping");
        // The benign residual key survives.
        assert_eq!(
            native.get(serde_norway::Value::String("GOOSE_TEMPERATURE".to_owned())),
            Some(&serde_norway::Value::Number(serde_norway::Number::from(
                0.2
            )))
        );
        // Provisioning merged the canonical provider back in as `GOOSE_PROVIDER`
        // (not carried from the managed import residual, which stripped it).
        assert_eq!(
            native.get(serde_norway::Value::String("GOOSE_PROVIDER".to_owned())),
            Some(&serde_norway::Value::String("anthropic".to_owned()))
        );
    }
}
