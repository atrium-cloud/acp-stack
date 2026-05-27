use std::path::Path;

use serde::Deserialize;

use crate::auth::generate_api_key;
use crate::config::Config;
use crate::error::{Result, StackError};
use crate::runtime::init_runner::{self, begin_run, finalize_run, find_resumable_run};
use crate::secrets::SecretStore;
use crate::state::{
    EVENT_SOURCE_CLI, INIT_RUN_FAILED, INIT_STEP_FAILED, INIT_STEP_PENDING, INIT_STEP_RUNNING,
    InitRunRecord, InitStepRecord, StateStore,
};

use super::InitArgs;

pub(super) fn resolve_init_run(args: &InitArgs, store: &StateStore) -> Result<InitRunRecord> {
    let args_json = serde_json::json!({
        "agent": args.agent,
        "provider": args.provider,
        "model": args.model,
        "mode": args.mode,
        "skills_source": args.skills_source,
        "skills": args.skills,
        "no_skills": args.no_skills,
        "testflight": args.testflight,
        "skip_testflight": args.skip_testflight,
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
    pub(super) provider: Option<String>,
    pub(super) model: Option<String>,
    pub(super) mode: Option<String>,
    pub(super) skills_source: Option<String>,
    #[serde(default)]
    pub(super) skills: Vec<String>,
    #[serde(default)]
    pub(super) no_skills: bool,
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
    finalize_run(store, &run.id, INIT_RUN_FAILED)?;
    Err(error)
}

pub(super) struct SecretsInitOutcome {
    pub(super) status: &'static str,
}

pub(super) fn perform_secrets_init(
    store_existed: bool,
    session_ref: &str,
    admin_ref: &str,
    secret_store: &mut SecretStore,
    store: &StateStore,
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
        });
    }
    let session_value = generate_api_key();
    let admin_value = generate_api_key();
    println!("---");
    println!("session key ({session_ref}): {session_value}");
    println!("admin key ({admin_ref}): {admin_value}");
    println!(
        "save the admin key now; it is never regenerable. use `acps reset --yes` to rotate it."
    );
    println!("---");
    secret_store.set_many([
        (session_ref, session_value.as_str()),
        (admin_ref, admin_value.as_str()),
    ])?;
    store.append_event_with_source(
        "info",
        "auth.keys_generated",
        EVENT_SOURCE_CLI,
        "generated session and admin API keys",
        &serde_json::json!({
            "session_key_ref": session_ref,
            "admin_key_ref": admin_ref,
        })
        .to_string(),
    )?;
    Ok(SecretsInitOutcome {
        status: "generated session and admin API keys",
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
