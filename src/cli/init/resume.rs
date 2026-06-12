use std::path::Path;

use serde::Deserialize;
use zeroize::Zeroizing;

use crate::auth::generate_api_key;
use crate::config::Config;
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
        "model_name": args.model_name,
        "context": args.context,
        "output_max_tokens": args.output_max_tokens,
        "skills_source": args.skills_source,
        "skills": args.skills,
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
        // Post-creation intents a bare `--resume` would otherwise drop: the
        // deps-apply request and the stack-update choice run in late steps, and
        // `--agent-env-ref` verification is deferred past several failure points,
        // so its names are replayed and re-verified on resume. The custom-agent
        // flags and `--dep`/`--dep-system` declarations are NOT recorded — they
        // are written into the on-disk config at creation, before any step can
        // fail, so resume recovers them from disk. (Interactive env values are
        // in-memory only and cannot be replayed.)
        "agent_env_ref": args.agent_env_ref,
        "deps_apply": args.deps_apply,
        "deps_apply_yes": args.deps_apply_yes,
        "stack_update": args.stack_update,
        "stack_update_frequency": args.stack_update_frequency,
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
    pub(super) model_name: Option<String>,
    pub(super) context: Option<String>,
    pub(super) output_max_tokens: Option<String>,
    pub(super) skills_source: Option<String>,
    #[serde(default)]
    pub(super) skills: Vec<String>,
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
    pub(super) stack_update: Option<String>,
    pub(super) stack_update_frequency: Option<String>,
}

pub(super) fn recorded_init_args(run: &InitRunRecord) -> Result<RecordedInitArgs> {
    serde_json::from_str(&run.args_json).map_err(|source| StackError::InitRunCorrupted {
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
    finalize_run(store, &run.id, INIT_RUN_FAILED)?;
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

/// Freshly generated API key plaintext, returned from a first-run
/// `secrets_init` so the driver can defer the operator handover to the very end
/// of init instead of printing it at generation time, where the install /
/// workspace / testflight scroll buries it. `Zeroizing` wipes the plaintext on
/// drop.
pub(super) struct FreshKeys {
    pub(super) session_ref: String,
    pub(super) admin_ref: String,
    pub(super) session_value: Zeroizing<String>,
    pub(super) admin_value: Zeroizing<String>,
}

pub(super) struct SecretsInitOutcome {
    pub(super) status: &'static str,
    /// `Some` only on fresh generation. Existing stores must never surface
    /// plaintext keys again.
    pub(super) fresh_keys: Option<FreshKeys>,
    pub(super) generated_keys: bool,
}

pub(super) fn perform_secrets_init(
    store_existed: bool,
    session_ref: &str,
    admin_ref: &str,
    secret_store: &mut SecretStore,
) -> Result<SecretsInitOutcome> {
    let session_present = secret_store.contains(session_ref);
    let admin_present = secret_store.contains(admin_ref);
    if store_existed {
        if !admin_present {
            return Err(StackError::MissingAdminKey {
                name: admin_ref.to_owned(),
            });
        }
        if !session_present {
            return Err(StackError::MissingSessionKey {
                name: session_ref.to_owned(),
            });
        }
        return Ok(SecretsInitOutcome {
            status: "preserved existing API keys",
            fresh_keys: None,
            generated_keys: false,
        });
    }
    // The plaintext is deliberately NOT printed here. The driver renders the
    // operator handover at the end of init (see `KeyHandover` in init.rs) so a
    // long install/workspace/testflight scroll cannot bury it; the values ride
    // back in `fresh_keys`.
    let session_value = generate_api_key();
    let admin_value = generate_api_key();
    secret_store.set_many([
        (session_ref, session_value.as_str()),
        (admin_ref, admin_value.as_str()),
    ])?;
    Ok(SecretsInitOutcome {
        status: "generated session and admin API keys",
        generated_keys: true,
        fresh_keys: Some(FreshKeys {
            session_ref: session_ref.to_owned(),
            admin_ref: admin_ref.to_owned(),
            session_value: Zeroizing::new(session_value),
            admin_value: Zeroizing::new(admin_value),
        }),
    })
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

    #[test]
    fn perform_secrets_init_preserves_existing_keys_without_handover() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let mut secret_store = SecretStore::open_or_create(tempdir.path()).expect("secret store");
        secret_store
            .set_many([
                ("SESSION_REF", "session-value"),
                ("ADMIN_REF", "admin-value"),
            ])
            .expect("seed keys");

        let outcome = perform_secrets_init(true, "SESSION_REF", "ADMIN_REF", &mut secret_store)
            .expect("outcome");

        assert_eq!(outcome.status, "preserved existing API keys");
        assert!(!outcome.generated_keys);
        assert!(outcome.fresh_keys.is_none());
    }
}
