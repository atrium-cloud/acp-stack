use std::path::Path;

use serde::Deserialize;
use zeroize::Zeroizing;

use crate::auth::{
    AuthVerifierEnsureOutcome, AuthVerifierSet, ensure_auth_verifier_pair, generate_api_key,
};
use crate::config::{Config, LegacyAuthConfig};
use crate::error::{Result, StackError};
use crate::runtime::init_runner::{self, begin_run, finalize_run, find_resumable_run};
use crate::secrets::SecretStore;
use crate::state::{
    INIT_RUN_FAILED, INIT_STEP_FAILED, INIT_STEP_PENDING, INIT_STEP_RUNNING, InitRunRecord,
    InitStepRecord, StateStore,
};

use super::InitArgs;

pub(super) fn resolve_init_run(args: &InitArgs, store: &StateStore) -> Result<InitRunRecord> {
    let args_json = serde_json::json!({
        "config_import_source": args.config_import_source_label(),
        "agent": args.agent,
        "provider": args.provider,
        "api_key_ref": args.api_key_ref,
        "custom_provider": args.custom_provider,
        "provider_name": args.provider_name,
        "base_url": args.base_url,
        "provider_api": args.provider_api,
        "model": args.model,
        "mode": args.mode,
        "effort": args.effort,
        "model_name": args.model_name,
        "context": args.context,
        "output_max_tokens": args.output_max_tokens,
        "skills_source": args.skills_source,
        "skills": args.skills,
        "essential_skills": args.essential_skills,
        "no_skills": args.no_skills,
        "edge": args.edge.map(|value| value.as_config_value()),
        "exposure": args.exposure.map(|value| value.as_config_value()),
        "hostname": args.hostname,
        "cloudflare_mode": args.cloudflare_mode.as_config_value(),
        "cloudflare_api_token_ref": args.cloudflare_api_token_ref,
        "cloudflare_account_id_ref": args.cloudflare_account_id_ref,
        "cloudflared_deployment": args.cloudflared_deployment.as_config_value(),
        "supabase_url": args.supabase_url,
        "supabase_schema": args.supabase_schema,
        "supabase_api_key_ref": args.supabase_api_key_ref,
        "no_supabase": args.no_supabase,
        "skip_workspace_init": args.skip_workspace_init(),
        "testflight": args.testflight,
        "skip_testflight": args.skip_testflight,
        // Post-creation intents a bare `--resume` would otherwise drop. Custom-agent flags and
        // `--dep` declarations are deliberately absent: they land in on-disk config at creation,
        // so resume recovers them from disk.
        "agent_env_ref": args.agent_env_ref,
        "deps_apply": args.deps_apply,
        "deps_apply_yes": args.deps_apply_yes,
        "deps_apply_async": args.deps_apply_async,
        "stack_update": args.stack_update,
        "stack_update_frequency": args.stack_update_frequency,
        "agent_update": args.agent_update,
        "agent_update_frequency": args.agent_update_frequency,
        "native_config_revision": args.native_config_revision,
        "rotate_keys": args.rotate_keys,
        "fresh": args.fresh,
        "resume": args.resume,
    })
    .to_string();

    if args.resume {
        let existing = if let Some(id) = args.run_id.as_deref() {
            init_runner::lookup_run(store, id)?.ok_or_else(|| StackError::InitRunCorrupted {
                reason: format!("no init run with id `{id}`"),
            })?
        } else {
            find_resumable_run(store)?.ok_or_else(|| StackError::InitRunCorrupted {
                reason: "no resumable init run found; re-run without --resume".to_owned(),
            })?
        };
        return Ok(existing);
    }

    begin_run(store, None, args.agent.as_deref(), &args_json)
}

#[derive(Default, Deserialize)]
pub(super) struct RecordedInitArgs {
    pub(super) agent: Option<String>,
    pub(super) provider: Option<String>,
    pub(super) api_key_ref: Option<String>,
    #[serde(default)]
    pub(super) custom_provider: bool,
    pub(super) provider_name: Option<String>,
    pub(super) base_url: Option<String>,
    pub(super) provider_api: Option<String>,
    pub(super) model: Option<String>,
    pub(super) mode: Option<String>,
    pub(super) effort: Option<String>,
    pub(super) model_name: Option<String>,
    pub(super) context: Option<String>,
    pub(super) output_max_tokens: Option<String>,
    pub(super) skills_source: Option<String>,
    #[serde(default)]
    pub(super) skills: Vec<String>,
    #[serde(default)]
    pub(super) essential_skills: bool,
    #[serde(default)]
    pub(super) no_skills: bool,
    pub(super) edge: Option<String>,
    pub(super) exposure: Option<String>,
    pub(super) hostname: Option<String>,
    pub(super) cloudflare_mode: Option<String>,
    pub(super) cloudflare_api_token_ref: Option<String>,
    pub(super) cloudflare_account_id_ref: Option<String>,
    pub(super) cloudflared_deployment: Option<String>,
    pub(super) supabase_url: Option<String>,
    pub(super) supabase_schema: Option<String>,
    pub(super) supabase_api_key_ref: Option<String>,
    #[serde(default)]
    pub(super) no_supabase: bool,
    #[serde(default)]
    #[cfg_attr(not(feature = "dev-tools"), allow(dead_code))]
    pub(super) skip_workspace_init: bool,
    #[serde(default)]
    pub(super) testflight: bool,
    #[serde(default)]
    pub(super) skip_testflight: bool,
    #[serde(default)]
    pub(super) agent_env_ref: Vec<String>,
    #[serde(default)]
    pub(super) deps_apply: bool,
    #[serde(default)]
    pub(super) deps_apply_yes: bool,
    #[serde(default)]
    pub(super) deps_apply_async: bool,
    pub(super) stack_update: Option<String>,
    pub(super) stack_update_frequency: Option<String>,
    pub(super) agent_update: Option<String>,
    pub(super) agent_update_frequency: Option<String>,
    pub(super) native_config_revision: Option<String>,
    #[serde(default)]
    pub(super) rotate_keys: bool,
}

// Unknown recorded keys are otherwise tolerated, so args that can no longer be replayed must be
// rejected explicitly instead of silently resuming without them.
const REMOVED_RECORDED_ARGS: &[&str] = &["plugins", "plugins_source"];

pub(super) fn recorded_init_args(run: &InitRunRecord) -> Result<RecordedInitArgs> {
    let value: serde_json::Value =
        serde_json::from_str(&run.args_json).map_err(|source| StackError::InitRunCorrupted {
            reason: format!("init run {} has invalid args_json: {source}", run.id),
        })?;
    for key in REMOVED_RECORDED_ARGS {
        let requested = match value.get(key) {
            None | Some(serde_json::Value::Null) => false,
            Some(serde_json::Value::Array(entries)) => !entries.is_empty(),
            Some(_) => true,
        };
        if requested {
            return Err(StackError::InitRunCorrupted {
                reason: format!(
                    "init run {} was recorded with the removed `{key}` argument and cannot be resumed; start a new run",
                    run.id
                ),
            });
        }
    }
    serde_json::from_value(value).map_err(|source| StackError::InitRunCorrupted {
        reason: format!("init run {} has invalid args_json: {source}", run.id),
    })
}

pub(super) fn step_needs_resume(steps: &[InitStepRecord], kind: &str) -> bool {
    steps.iter().any(|step| {
        step.kind == kind
            && matches!(
                step.status.as_str(),
                INIT_STEP_PENDING | INIT_STEP_RUNNING | INIT_STEP_FAILED
            )
    })
}

pub(super) fn finalize_with_error(
    store: &StateStore,
    run: &InitRunRecord,
    error: StackError,
) -> Result<()> {
    let failed_step = store
        .query_init_steps(&run.id)
        .ok()
        .and_then(failed_step_for_report);
    // Print the diagnostics BEFORE the fallible settle write, so a store that fails at
    // `finalize_run` cannot swallow them.
    eprintln!("init failed in run {}", run.id);
    if let Some(step) = failed_step {
        eprintln!("failed step: {}", step.kind);
        if let Some(log_dir) = step.log_dir.as_deref()
            && !log_dir.trim().is_empty()
        {
            eprintln!("logs: {log_dir}");
        }
    }
    eprintln!("retry: acps init --resume --run-id {}", run.id);
    // A settle failure is logged rather than propagated: it must not mask the body error.
    if let Err(settle_error) = finalize_run(store, &run.id, INIT_RUN_FAILED) {
        tracing::error!(
            run_id = %run.id,
            body_error = %error,
            settle_error = %settle_error,
            "failed to settle init run as failed; the run row may be left non-terminal",
        );
    }
    Err(error)
}

fn failed_step_for_report(steps: Vec<InitStepRecord>) -> Option<InitStepRecord> {
    steps
        .into_iter()
        .filter(|step| step.status == INIT_STEP_FAILED || step.status == INIT_STEP_RUNNING)
        .max_by(|left, right| {
            let left_timestamp = step_report_timestamp(left);
            let right_timestamp = step_report_timestamp(right);
            left_timestamp
                .cmp(right_timestamp)
                .then_with(|| left.ordinal.cmp(&right.ordinal))
        })
}

fn step_report_timestamp(step: &InitStepRecord) -> &str {
    step.finished_at
        .as_deref()
        .or(step.started_at.as_deref())
        .unwrap_or("")
}

/// Freshly generated API key plaintext, carried so the driver can defer the operator handover
/// to the end of init. `Zeroizing` wipes the plaintext on drop.
pub(super) struct FreshKeys {
    pub(super) session_value: Zeroizing<String>,
    pub(super) admin_value: Zeroizing<String>,
}

pub(super) struct AuthInitOutcome {
    pub(super) status: &'static str,
    /// `Some` on fresh generation or rotation. A preserving run must never
    /// surface plaintext keys again.
    pub(super) fresh_keys: Option<FreshKeys>,
    pub(super) generated_keys: bool,
    /// True when existing verifier rows were replaced rather than created.
    pub(super) rotated_keys: bool,
}

/// What `secrets_init` does when verifier rows already exist. Hosted init always rotates: a
/// preserved run delivers a keyless result frame the backend cannot accept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum KeyPolicy {
    PreserveExisting,
    RotateExisting,
}

/// `secret_store` MUST be the caller's live handle: later steps persist that handle's whole
/// in-memory map, so rotated refs written through a separate `open` are silently rolled back.
pub(super) fn perform_auth_init(
    store: &StateStore,
    legacy_auth: Option<&LegacyAuthConfig>,
    home: &Path,
    secret_store: &mut SecretStore,
    policy: KeyPolicy,
) -> Result<AuthInitOutcome> {
    match ensure_auth_verifier_pair(store, legacy_auth, home)? {
        AuthVerifierEnsureOutcome::Preserved
        | AuthVerifierEnsureOutcome::BackfilledLegacySecrets => match policy {
            KeyPolicy::PreserveExisting => Ok(AuthInitOutcome {
                status: "preserved existing API keys",
                fresh_keys: None,
                generated_keys: false,
                rotated_keys: false,
            }),
            KeyPolicy::RotateExisting => {
                let session_value = generate_api_key();
                let admin_value = generate_api_key();
                let verifiers = AuthVerifierSet::create(&session_value, &admin_value);
                // Legacy secret-store entries MUST be rewritten BEFORE the verifier rows are
                // replaced: a failed rewrite here leaves the old keys valid, whereas the reverse
                // order could invalidate them and then lose the new plaintexts to the error path.
                if let Some(legacy_auth) = legacy_auth {
                    secret_store.set_many([
                        (legacy_auth.session_key_ref.as_str(), session_value.as_str()),
                        (legacy_auth.admin_key_ref.as_str(), admin_value.as_str()),
                    ])?;
                }
                store.replace_auth_key_pair(&verifiers)?;
                Ok(AuthInitOutcome {
                    status: "rotated session and admin API keys",
                    generated_keys: true,
                    rotated_keys: true,
                    fresh_keys: Some(FreshKeys {
                        session_value: Zeroizing::new(session_value),
                        admin_value: Zeroizing::new(admin_value),
                    }),
                })
            }
        },
        AuthVerifierEnsureOutcome::Missing => {
            let session_value = generate_api_key();
            let admin_value = generate_api_key();
            let verifiers = AuthVerifierSet::create(&session_value, &admin_value);
            store.insert_auth_key_pair(&verifiers)?;
            Ok(AuthInitOutcome {
                status: "generated session and admin API keys",
                generated_keys: true,
                rotated_keys: false,
                fresh_keys: Some(FreshKeys {
                    session_value: Zeroizing::new(session_value),
                    admin_value: Zeroizing::new(admin_value),
                }),
            })
        }
    }
}

pub(super) fn installer_postcondition_holds(
    config: &Config,
    workspace_root: &Path,
    local_bin_dir: &Path,
) -> bool {
    let (target, extra_path_dirs): (&str, Vec<&Path>) =
        if let Some(install) = config.agent.install.as_ref() {
            (install.creates.as_str(), Vec::new())
        } else {
            (config.agent.command.as_str(), vec![local_bin_dir])
        };
    crate::runtime::install::agent_installer::resolve_creates_for_init_resume(
        target,
        workspace_root,
        &extra_path_dirs,
        config.agent.expected_sha256.as_deref(),
    )
    .is_some()
}

pub(super) fn workspace_postcondition_holds(workspace: &crate::config::WorkspaceConfig) -> bool {
    crate::runtime::workspace_sources::workspace_init::all_sources_have_sentinel(workspace)
        .unwrap_or(false)
}

pub(super) fn init_complete_event_already_recorded(store: &StateStore, run_id: &str) -> bool {
    let Ok(events) = store.query_events(crate::state::EventFilter {
        limit: 64,
        kind: Some("init.completed"),
        ..crate::state::EventFilter::default()
    }) else {
        return false;
    };
    events
        .iter()
        .any(|event| event.payload_json.contains(run_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(
        ordinal: i64,
        kind: &str,
        status: &str,
        started_at: &str,
        finished_at: &str,
    ) -> InitStepRecord {
        InitStepRecord {
            id: format!("step_{ordinal}"),
            run_id: "run".to_owned(),
            ordinal,
            kind: kind.to_owned(),
            status: status.to_owned(),
            started_at: (!started_at.is_empty()).then(|| started_at.to_owned()),
            finished_at: (!finished_at.is_empty()).then(|| finished_at.to_owned()),
            log_dir: None,
            error_kind: None,
            error_detail: None,
            payload_json: "{}".to_owned(),
        }
    }

    #[test]
    fn failed_step_report_uses_latest_attempt_timestamp() {
        let steps = vec![
            step(
                10,
                "later_prior_failure",
                INIT_STEP_FAILED,
                "2026-01-01T00:00:00.000000000Z",
                "2026-01-01T00:00:01.000000000Z",
            ),
            step(
                2,
                "current_failure",
                INIT_STEP_FAILED,
                "2026-01-01T00:01:00.000000000Z",
                "2026-01-01T00:01:01.000000000Z",
            ),
        ];

        let failed_step = failed_step_for_report(steps).expect("failed step");
        assert_eq!(failed_step.kind, "current_failure");
    }

    fn run_with_args(args_json: &str) -> InitRunRecord {
        InitRunRecord {
            id: "run".to_owned(),
            started_at: "2026-01-01T00:00:00.000000000Z".to_owned(),
            finished_at: None,
            status: INIT_RUN_FAILED.to_owned(),
            runtime_user: None,
            agent_id: None,
            args_json: args_json.to_owned(),
        }
    }

    #[test]
    fn recorded_init_args_rejects_removed_plugin_request() {
        let run = run_with_args(r#"{"plugins": ["yeet"], "skills": []}"#);

        let err = match recorded_init_args(&run) {
            Ok(_) => panic!("expected removed-argument rejection"),
            Err(err) => err,
        };

        assert!(matches!(err, StackError::InitRunCorrupted { .. }));
        assert!(err.to_string().contains("`plugins`"));
    }

    #[test]
    fn recorded_init_args_tolerates_unrequested_removed_keys() {
        let run = run_with_args(r#"{"plugins": [], "plugins_source": null, "skills": ["docx"]}"#);

        let recorded = recorded_init_args(&run).expect("recorded args");

        assert_eq!(recorded.skills, vec!["docx".to_owned()]);
    }

    #[test]
    fn perform_auth_init_preserves_existing_keys_without_handover() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let state_path = tempdir.path().join("state.sqlite");
        let store = StateStore::open(&state_path).expect("state");
        store.migrate().expect("migrate");
        let verifiers = AuthVerifierSet::create("session-value", "admin-value");
        store.insert_auth_key_pair(&verifiers).expect("seed keys");

        let mut secret_store = SecretStore::open_or_create(tempdir.path()).expect("secrets");
        let outcome = perform_auth_init(
            &store,
            None,
            tempdir.path(),
            &mut secret_store,
            KeyPolicy::PreserveExisting,
        )
        .expect("outcome");

        assert_eq!(outcome.status, "preserved existing API keys");
        assert!(!outcome.generated_keys);
        assert!(!outcome.rotated_keys);
        assert!(outcome.fresh_keys.is_none());
    }

    #[test]
    fn perform_auth_init_rotates_existing_keys_and_returns_plaintext() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let state_path = tempdir.path().join("state.sqlite");
        let store = StateStore::open(&state_path).expect("state");
        store.migrate().expect("migrate");
        let verifiers = AuthVerifierSet::create("session-value", "admin-value");
        store.insert_auth_key_pair(&verifiers).expect("seed keys");

        let mut secret_store = SecretStore::open_or_create(tempdir.path()).expect("secrets");
        let outcome = perform_auth_init(
            &store,
            None,
            tempdir.path(),
            &mut secret_store,
            KeyPolicy::RotateExisting,
        )
        .expect("outcome");

        assert_eq!(outcome.status, "rotated session and admin API keys");
        assert!(outcome.generated_keys);
        assert!(outcome.rotated_keys);
        let fresh = outcome.fresh_keys.expect("rotation must return plaintext");

        let pair = store.load_auth_verifier_pair().expect("pair");
        assert!(
            pair.verify("session-value").is_none() && pair.verify("admin-value").is_none(),
            "retired keys must stop verifying"
        );
        assert!(pair.verify(fresh.session_value.as_str()).is_some());
        assert!(pair.verify(fresh.admin_value.as_str()).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn rotation_failure_in_legacy_rewrite_keeps_old_keys_valid() {
        use std::os::unix::fs::PermissionsExt;

        let tempdir = tempfile::tempdir().expect("tempdir");
        let state_path = tempdir.path().join("state.sqlite");
        let store = StateStore::open(&state_path).expect("state");
        store.migrate().expect("migrate");
        let mut secret_store = SecretStore::open_or_create(tempdir.path()).expect("secrets");
        secret_store
            .set("legacy_session", "session-value")
            .expect("seed session secret");
        secret_store
            .set("legacy_admin", "admin-value")
            .expect("seed admin secret");
        let verifiers = AuthVerifierSet::create("session-value", "admin-value");
        store.insert_auth_key_pair(&verifiers).expect("seed keys");
        let legacy_auth = LegacyAuthConfig {
            session_key_ref: "legacy_session".to_owned(),
            admin_key_ref: "legacy_admin".to_owned(),
        };

        let store_dir = secret_store
            .store_path()
            .parent()
            .expect("store dir")
            .to_path_buf();
        std::fs::set_permissions(&store_dir, std::fs::Permissions::from_mode(0o555))
            .expect("make store dir read-only");
        let result = perform_auth_init(
            &store,
            Some(&legacy_auth),
            tempdir.path(),
            &mut secret_store,
            KeyPolicy::RotateExisting,
        );
        std::fs::set_permissions(&store_dir, std::fs::Permissions::from_mode(0o755))
            .expect("restore store dir permissions");

        // The rewrite failed before the verifier replace, so the old keys must still verify.
        assert!(
            result.is_err(),
            "rotation must fail when the legacy rewrite fails"
        );
        let pair = store.load_auth_verifier_pair().expect("pair");
        assert!(pair.verify("session-value").is_some());
        assert!(pair.verify("admin-value").is_some());
    }

    #[test]
    fn perform_auth_init_rotation_rewrites_legacy_secret_refs() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let state_path = tempdir.path().join("state.sqlite");
        let store = StateStore::open(&state_path).expect("state");
        store.migrate().expect("migrate");
        let mut secret_store = SecretStore::open_or_create(tempdir.path()).expect("secrets");
        secret_store
            .set("legacy_session", "session-value")
            .expect("seed session secret");
        secret_store
            .set("legacy_admin", "admin-value")
            .expect("seed admin secret");
        let verifiers = AuthVerifierSet::create("session-value", "admin-value");
        store.insert_auth_key_pair(&verifiers).expect("seed keys");
        let legacy_auth = LegacyAuthConfig {
            session_key_ref: "legacy_session".to_owned(),
            admin_key_ref: "legacy_admin".to_owned(),
        };

        let outcome = perform_auth_init(
            &store,
            Some(&legacy_auth),
            tempdir.path(),
            &mut secret_store,
            KeyPolicy::RotateExisting,
        )
        .expect("outcome");
        let fresh = outcome.fresh_keys.expect("rotation must return plaintext");

        // A later persist through the same handle must not roll the rotated refs back.
        secret_store
            .set("unrelated", "value")
            .expect("later write through the same handle");

        // A future legacy backfill reads these refs; stale plaintexts would resurrect old keys.
        let reopened = SecretStore::open(tempdir.path()).expect("reopen secrets");
        assert_eq!(
            reopened.get("legacy_session").expect("session secret"),
            fresh.session_value.as_str()
        );
        assert_eq!(
            reopened.get("legacy_admin").expect("admin secret"),
            fresh.admin_value.as_str()
        );
    }
}
