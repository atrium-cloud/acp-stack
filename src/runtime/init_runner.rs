//! Init run orchestrator: records each `acps init` phase as a durable, resumable `init_steps` row.

use std::path::Path;

use crate::error::{Result, StackError};
use crate::runtime::install::agent_installer::persist_step_logs_to_disk;
use crate::state::{
    INIT_RUN_FAILED, INIT_RUN_SUCCEEDED, INIT_STEP_FAILED, INIT_STEP_RUNNING, INIT_STEP_SKIPPED,
    INIT_STEP_SUCCEEDED, InitRunRecord, InitStepRecord, NewInitRun, NewInitStep, StateStore,
};

/// Stable `init_steps.kind` identifiers. A new phase needs a constant here AND an entry in the CLI driver's ordinal map.
pub mod step_kind {
    pub const CONFIG_VALIDATE: &str = "config_validate";
    pub const STATE_INIT: &str = "state_init";
    pub const SECRETS_INIT: &str = "secrets_init";
    pub const AGENT_INSTALL: &str = "agent_install";
    pub const NATIVE_CONFIG_IMPORT: &str = "native_config_import";
    pub const AGENT_SKILLS_INSTALL: &str = "agent_skills_install";
    pub const DEPS_APPLY: &str = "deps_apply";
    pub const CAPABILITY_PROBE: &str = "capability_probe";
    pub const MCP_CONFIGURE: &str = "mcp_configure";
    pub const PROVIDER_CONFIGURE: &str = "provider_configure";
    pub const WORKSPACE_MATERIALIZE: &str = "workspace_materialize";
    pub const AGENT_HEADLESS_CONFIG: &str = "agent_headless_config";
    pub const EDGE_ARTIFACTS: &str = "edge_artifacts";
    pub const INIT_COMPLETE: &str = "init_complete";
    pub const TESTFLIGHT: &str = "testflight";
}

/// Outcome of executing one step. Returned by the body closure.
#[derive(Debug)]
pub struct StepOutcome {
    /// Optional on-disk log directory for steps that capture output.
    pub log_dir: Option<String>,
    /// Step payload merged into `init_steps.payload_json`; must be a valid JSON object literal.
    pub payload_json: String,
    /// The body launched its work in the background. The row still records `succeeded` (the launch
    /// succeeded and must not poison `finalize_init_run`); the disposition reports `Background`.
    pub background: bool,
}

impl StepOutcome {
    pub fn empty() -> Self {
        Self {
            log_dir: None,
            payload_json: "{}".to_owned(),
            background: false,
        }
    }

    pub fn with_payload(payload_json: impl Into<String>) -> Self {
        Self {
            log_dir: None,
            payload_json: payload_json.into(),
            background: false,
        }
    }

    pub fn background_with_payload(payload_json: impl Into<String>) -> Self {
        Self {
            log_dir: None,
            payload_json: payload_json.into(),
            background: true,
        }
    }
}

/// Result of [`record_step`]: body ran, body launched background work, or the verifier let it be skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepDisposition {
    Executed,
    Background,
    Skipped,
}

/// Begin a new init run, recording the args context.
pub fn begin_run(
    store: &StateStore,
    runtime_user: Option<&str>,
    agent_id: Option<&str>,
    args_json: &str,
) -> Result<InitRunRecord> {
    store.create_init_run(NewInitRun {
        runtime_user,
        agent_id,
        args_json,
    })
}

/// Resume the most recent `pending`/`running`/`failed` run, scanning past newer succeeded rows.
pub fn find_resumable_run(store: &StateStore) -> Result<Option<InitRunRecord>> {
    store.latest_non_terminal_init_run()
}

/// Lookup a specific run by id.
pub fn lookup_run(store: &StateStore, run_id: &str) -> Result<Option<InitRunRecord>> {
    store.lookup_init_run(run_id)
}

/// Mark an init run as `succeeded` or `failed`.
pub fn finalize_run(store: &StateStore, run_id: &str, status: &str) -> Result<()> {
    store.finalize_init_run(run_id, status)
}

/// Execute one phase, persisting its row to `init_steps`. Ordinals are caller-managed and unique per run;
/// a prior `succeeded` row whose `verify` still passes replays as `skipped` without running `body`.
pub fn record_step(
    store: &StateStore,
    run: &InitRunRecord,
    ordinal: i64,
    kind: &str,
    verify: impl FnOnce() -> Result<bool>,
    body: impl FnOnce() -> Result<StepOutcome>,
) -> Result<StepDisposition> {
    record_step_with_default_log_dir(store, run, ordinal, kind, None, verify, body)
}

/// Like [`record_step`] but stamps `default_log_dir` on the row whether the body succeeds OR fails,
/// so a mid-body failure still points at the captured output instead of `log_dir = NULL`.
#[allow(clippy::too_many_arguments)]
pub fn record_step_with_default_log_dir(
    store: &StateStore,
    run: &InitRunRecord,
    ordinal: i64,
    kind: &str,
    default_log_dir: Option<&str>,
    verify: impl FnOnce() -> Result<bool>,
    body: impl FnOnce() -> Result<StepOutcome>,
) -> Result<StepDisposition> {
    let prior = store.lookup_init_step(&run.id, ordinal)?;

    let step_id = match prior.as_ref() {
        Some(existing) => {
            if existing.kind != kind {
                return Err(StackError::InitRunCorrupted {
                    reason: format!(
                        "ordinal {ordinal} of run {} was recorded as `{}` but driver now claims `{}`",
                        run.id, existing.kind, kind
                    ),
                });
            }
            existing.id.clone()
        }
        None => {
            let record = store.append_init_step(NewInitStep {
                run_id: &run.id,
                ordinal,
                kind,
                payload_json: "{}",
            })?;
            record.id
        }
    };

    if let Some(existing) = prior.as_ref()
        && matches!(
            existing.status.as_str(),
            INIT_STEP_SUCCEEDED | INIT_STEP_SKIPPED
        )
        && verifier_holds(verify)
    {
        // Both `succeeded` and `skipped` are verifier-eligible so chained resumes against the same
        // run don't re-execute a verified step just because the prior resume marked it `skipped`.
        store.mark_init_step_skipped(&step_id, &resume_payload(existing))?;
        return Ok(StepDisposition::Skipped);
    }

    store.mark_init_step_running(&step_id)?;
    match run_step_body(kind, body) {
        Ok(outcome) => {
            let log_dir = outcome.log_dir.as_deref().or(default_log_dir);
            store.mark_init_step_succeeded(&step_id, log_dir, &outcome.payload_json)?;
            Ok(if outcome.background {
                StepDisposition::Background
            } else {
                StepDisposition::Executed
            })
        }
        Err(error) => {
            settle_failed_step(store, &step_id, default_log_dir, &error);
            Err(error)
        }
    }
}

/// Converts a body panic into a typed [`StackError`] so the failure arm still settles the durable row;
/// letting the unwind through strands the `init_steps` row at `running`.
fn run_step_body<T>(kind: &str, body: impl FnOnce() -> Result<T>) -> Result<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
        Ok(result) => result,
        Err(payload) => Err(StackError::InitStepPanicked {
            kind: kind.to_owned(),
            message: panic_payload_message(payload.as_ref()),
        }),
    }
}

/// Treats BOTH an error and a panic as "postcondition not proven" so the body re-runs; the verifier
/// runs before the row is marked `running`, so an escaping unwind would strand it at `pending`.
fn verifier_holds(verify: impl FnOnce() -> Result<bool>) -> bool {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(verify))
        .map(|result| result.unwrap_or(false))
        .unwrap_or(false)
}

/// Best-effort extraction of a panic's message.
pub(crate) fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "panic with a non-string payload".to_owned()
    }
}

/// Mark a step `failed`. A failing settlement write is logged but must never replace the body error.
fn settle_failed_step(
    store: &StateStore,
    step_id: &str,
    default_log_dir: Option<&str>,
    error: &StackError,
) {
    if let Err(settle_error) = store.mark_init_step_failed(
        step_id,
        default_log_dir,
        error.error_code(),
        &error.to_string(),
        "{}",
    ) {
        tracing::error!(
            step_id = %step_id,
            body_error = %error,
            settle_error = %settle_error,
            "failed to record init step failure; the durable row may be left `running`",
        );
    }
}

/// Variant of [`record_step`] that persists an installer draft row alongside the init step.
#[allow(clippy::too_many_arguments)]
pub fn record_step_with_log_dir(
    store: &StateStore,
    run: &InitRunRecord,
    ordinal: i64,
    kind: &str,
    agent_id_for_logs: &str,
    log_base: Option<&Path>,
    verify: impl FnOnce() -> Result<bool>,
    body: impl FnOnce() -> Result<(
        crate::runtime::install::agent_installer::InstallerRowDraft,
        StepOutcome,
    )>,
) -> Result<StepDisposition> {
    let prior = store.lookup_init_step(&run.id, ordinal)?;
    let step_id = match prior.as_ref() {
        Some(existing) => {
            if existing.kind != kind {
                return Err(StackError::InitRunCorrupted {
                    reason: format!(
                        "ordinal {ordinal} of run {} was recorded as `{}` but driver now claims `{}`",
                        run.id, existing.kind, kind
                    ),
                });
            }
            existing.id.clone()
        }
        None => {
            let record = store.append_init_step(NewInitStep {
                run_id: &run.id,
                ordinal,
                kind,
                payload_json: "{}",
            })?;
            record.id
        }
    };

    if let Some(existing) = prior.as_ref()
        && matches!(
            existing.status.as_str(),
            INIT_STEP_SUCCEEDED | INIT_STEP_SKIPPED
        )
        && verifier_holds(verify)
    {
        store.mark_init_step_skipped(&step_id, &resume_payload(existing))?;
        return Ok(StepDisposition::Skipped);
    }

    store.mark_init_step_running(&step_id)?;
    match run_step_body(kind, body) {
        Ok((mut draft, outcome)) => {
            // A missing audit copy fails the step rather than recording `succeeded` without logs.
            if let Err(error) = persist_step_logs_to_disk(&mut draft, agent_id_for_logs, log_base) {
                settle_failed_step(store, &step_id, None, &error);
                return Err(error);
            }
            let log_dir = draft.log_dir.clone().or(outcome.log_dir);
            store.mark_init_step_succeeded(&step_id, log_dir.as_deref(), &outcome.payload_json)?;
            Ok(if outcome.background {
                StepDisposition::Background
            } else {
                StepDisposition::Executed
            })
        }
        Err(error) => {
            settle_failed_step(store, &step_id, None, &error);
            Err(error)
        }
    }
}

/// Build a `skipped` step's payload: the prior payload plus `resume.verified = true`.
fn resume_payload(existing: &InitStepRecord) -> String {
    let parsed: serde_json::Value = serde_json::from_str(&existing.payload_json)
        .unwrap_or_else(|_| serde_json::Value::Object(Default::default()));
    let mut object = match parsed {
        serde_json::Value::Object(map) => map,
        _ => serde_json::Map::new(),
    };
    object.insert("resume".to_owned(), serde_json::json!({ "verified": true }));
    serde_json::Value::Object(object).to_string()
}

/// Terminal run status to pass to [`finalize_run`]: any errored step means `INIT_RUN_FAILED`.
pub fn terminal_status_for(dispositions: &[StepDisposition], step_errored: bool) -> &'static str {
    if step_errored {
        INIT_RUN_FAILED
    } else {
        let _ = (dispositions, INIT_STEP_RUNNING, INIT_STEP_FAILED);
        INIT_RUN_SUCCEEDED
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{INIT_RUN_PENDING, INIT_STEP_SKIPPED};
    use tempfile::tempdir;

    fn open_store() -> (tempfile::TempDir, StateStore) {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("state.sqlite");
        let store = StateStore::open(&path).expect("open");
        store.migrate().expect("migrate");
        (dir, store)
    }

    #[test]
    fn record_step_runs_body_on_first_call() {
        let (_dir, store) = open_store();
        let run = begin_run(&store, None, None, "{}").expect("begin");
        let mut called = false;
        let disposition = record_step(
            &store,
            &run,
            1,
            step_kind::AGENT_INSTALL,
            || Ok(false),
            || {
                called = true;
                Ok(StepOutcome::empty())
            },
        )
        .expect("step");
        assert!(called);
        assert_eq!(disposition, StepDisposition::Executed);
        let steps = store.query_init_steps(&run.id).expect("steps");
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].status, INIT_STEP_SUCCEEDED);
    }

    #[test]
    fn record_step_reports_background_disposition_with_a_succeeded_row() {
        let (_dir, store) = open_store();
        let run = begin_run(&store, None, None, "{}").expect("begin");
        let disposition = record_step(
            &store,
            &run,
            1,
            step_kind::DEPS_APPLY,
            || Ok(false),
            || {
                Ok(StepOutcome::background_with_payload(
                    r#"{"apply_run_id":"dap_bg","background":true}"#,
                ))
            },
        )
        .expect("step");
        assert_eq!(disposition, StepDisposition::Background);
        let steps = store.query_init_steps(&run.id).expect("steps");
        assert_eq!(steps[0].status, INIT_STEP_SUCCEEDED);
        assert!(steps[0].payload_json.contains("\"background\":true"));
    }

    #[test]
    fn record_step_skips_when_verifier_passes_after_prior_success() {
        let (_dir, store) = open_store();
        let run = begin_run(&store, None, None, "{}").expect("begin");
        record_step(
            &store,
            &run,
            1,
            step_kind::AGENT_INSTALL,
            || Ok(false),
            || Ok(StepOutcome::with_payload(r#"{"attempt":1}"#)),
        )
        .expect("first run");

        let mut called_again = false;
        let disposition = record_step(
            &store,
            &run,
            1,
            step_kind::AGENT_INSTALL,
            || Ok(true),
            || {
                called_again = true;
                Ok(StepOutcome::empty())
            },
        )
        .expect("resume");
        assert!(!called_again);
        assert_eq!(disposition, StepDisposition::Skipped);
        let steps = store.query_init_steps(&run.id).expect("steps");
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].status, INIT_STEP_SKIPPED);
        assert!(
            steps[0].payload_json.contains("\"resume\""),
            "skipped payload should retain resume marker: {}",
            steps[0].payload_json
        );
        assert!(steps[0].payload_json.contains("\"attempt\":1"));
    }

    #[test]
    fn record_step_reruns_body_when_verifier_fails() {
        let (_dir, store) = open_store();
        let run = begin_run(&store, None, None, "{}").expect("begin");
        record_step(
            &store,
            &run,
            1,
            step_kind::AGENT_INSTALL,
            || Ok(false),
            || Ok(StepOutcome::empty()),
        )
        .expect("first run");

        let mut called_again = false;
        record_step(
            &store,
            &run,
            1,
            step_kind::AGENT_INSTALL,
            || Ok(false),
            || {
                called_again = true;
                Ok(StepOutcome::empty())
            },
        )
        .expect("resume");
        assert!(called_again, "verifier=false must re-run body");
    }

    #[test]
    fn record_step_marks_failed_and_propagates_error() {
        let (_dir, store) = open_store();
        let run = begin_run(&store, None, None, "{}").expect("begin");
        let error = record_step(
            &store,
            &run,
            1,
            step_kind::AGENT_INSTALL,
            || Ok(false),
            || {
                Err(StackError::AgentInitializeFailed {
                    reason: "synthetic".into(),
                })
            },
        )
        .expect_err("must propagate");
        assert!(error.to_string().contains("synthetic"));
        let steps = store.query_init_steps(&run.id).expect("steps");
        assert_eq!(steps[0].status, INIT_STEP_FAILED);
        assert_eq!(steps[0].error_kind.as_deref(), Some(error.error_code()));
    }

    #[test]
    fn record_step_with_default_log_dir_records_log_dir_on_failure() {
        let (_dir, store) = open_store();
        let run = begin_run(&store, None, None, "{}").expect("begin");
        record_step_with_default_log_dir(
            &store,
            &run,
            1,
            step_kind::WORKSPACE_MATERIALIZE,
            Some("/tmp/workspace-init-logs/irun_test"),
            || Ok(false),
            || {
                Err(StackError::AgentInitializeFailed {
                    reason: "clone bombed".into(),
                })
            },
        )
        .expect_err("must propagate body error");
        let steps = store.query_init_steps(&run.id).expect("steps");
        assert_eq!(steps[0].status, INIT_STEP_FAILED);
        assert_eq!(
            steps[0].log_dir.as_deref(),
            Some("/tmp/workspace-init-logs/irun_test"),
            "failed step must record the pre-computed log_dir for audit",
        );
    }

    #[test]
    fn record_step_reuses_existing_failed_row_on_resume() {
        let (_dir, store) = open_store();
        let run = begin_run(&store, None, None, "{}").expect("begin");
        let _ = record_step(
            &store,
            &run,
            1,
            step_kind::AGENT_INSTALL,
            || Ok(false),
            || {
                Err(StackError::AgentInitializeFailed {
                    reason: "first".into(),
                })
            },
        );
        let _ = record_step(
            &store,
            &run,
            1,
            step_kind::AGENT_INSTALL,
            || Ok(false),
            || Ok(StepOutcome::empty()),
        )
        .expect("retry");
        let steps = store.query_init_steps(&run.id).expect("steps");
        assert_eq!(steps.len(), 1, "ordinal reused, no duplicate row");
        assert_eq!(steps[0].status, INIT_STEP_SUCCEEDED);
    }

    #[test]
    fn find_resumable_run_picks_latest_unfinished_or_failed() {
        let (_dir, store) = open_store();
        let succeeded = begin_run(&store, None, None, "{}").expect("begin");
        finalize_run(&store, &succeeded.id, INIT_RUN_SUCCEEDED).expect("finalize");
        let in_flight = begin_run(&store, None, None, "{}").expect("begin");
        let found = find_resumable_run(&store)
            .expect("find")
            .expect("there should be a resumable");
        assert_eq!(found.id, in_flight.id);
        assert_eq!(found.status, INIT_RUN_PENDING);

        finalize_run(&store, &in_flight.id, INIT_RUN_FAILED).expect("finalize");
        let still_resumable = find_resumable_run(&store)
            .expect("find")
            .expect("failed run should still be resumable");
        assert_eq!(still_resumable.id, in_flight.id);
        assert_eq!(still_resumable.status, INIT_RUN_FAILED);

        let later_success = begin_run(&store, None, None, "{}").expect("begin");
        finalize_run(&store, &later_success.id, INIT_RUN_SUCCEEDED).expect("finalize");
        let found = find_resumable_run(&store)
            .expect("find")
            .expect("failed older run still wins");
        assert_eq!(found.id, in_flight.id);

        finalize_run(&store, &in_flight.id, INIT_RUN_SUCCEEDED).expect("clear");
        let none = find_resumable_run(&store).expect("find");
        assert!(none.is_none(), "all-succeeded table returns None");
    }

    #[test]
    fn resume_payload_survives_nested_objects_across_multiple_skips() {
        let step = InitStepRecord {
            id: "istep_x".to_owned(),
            run_id: "irun_x".to_owned(),
            ordinal: 1,
            kind: step_kind::AGENT_INSTALL.to_owned(),
            status: INIT_STEP_SUCCEEDED.to_owned(),
            started_at: None,
            finished_at: None,
            log_dir: None,
            error_kind: None,
            error_detail: None,
            payload_json: r#"{"installer_run_id":"ins_1","resume":{"verified":true}}"#.to_owned(),
        };
        let payload = resume_payload(&step);
        let parsed: serde_json::Value =
            serde_json::from_str(&payload).expect("resume payload must be valid JSON");
        assert_eq!(parsed["installer_run_id"], "ins_1");
        assert_eq!(parsed["resume"]["verified"], true);
    }

    #[test]
    fn record_step_converts_a_body_panic_into_a_failed_row_and_typed_error() {
        let (_dir, store) = open_store();
        let run = begin_run(&store, None, None, "{}").expect("begin");
        let error = record_step(
            &store,
            &run,
            1,
            step_kind::PROVIDER_CONFIGURE,
            || Ok(false),
            || -> Result<StepOutcome> { panic!("boom inside the step body") },
        )
        .expect_err("a panicking body must surface as an error, not unwind the caller");
        match &error {
            StackError::InitStepPanicked { kind, message } => {
                assert_eq!(kind, step_kind::PROVIDER_CONFIGURE);
                assert!(
                    message.contains("boom inside the step body"),
                    "panic message not captured: {message}"
                );
            }
            other => panic!("expected InitStepPanicked, got {other:?}"),
        }
        assert_eq!(error.error_code(), "init.step_panicked");
        let steps = store.query_init_steps(&run.id).expect("steps");
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].status, INIT_STEP_FAILED);
        assert_eq!(steps[0].error_kind.as_deref(), Some("init.step_panicked"));
        assert!(
            steps[0]
                .error_detail
                .as_deref()
                .unwrap_or_default()
                .contains("boom inside the step body"),
            "durable error_detail must carry the panic message for post-mortem",
        );
    }

    #[test]
    fn record_step_treats_a_verifier_panic_as_unproven_and_reruns_the_body() {
        let (_dir, store) = open_store();
        let run = begin_run(&store, None, None, "{}").expect("begin");
        record_step(
            &store,
            &run,
            1,
            step_kind::AGENT_INSTALL,
            || Ok(false),
            || Ok(StepOutcome::empty()),
        )
        .expect("first run leaves a succeeded row for the verifier to consult");

        let mut reran = false;
        let disposition = record_step(
            &store,
            &run,
            1,
            step_kind::AGENT_INSTALL,
            || -> Result<bool> { panic!("verifier blew up reading external state") },
            || {
                reran = true;
                Ok(StepOutcome::empty())
            },
        )
        .expect("a verifier panic must not propagate; the body re-runs");
        assert!(reran, "verifier panic must fall through to a body re-run");
        assert_eq!(disposition, StepDisposition::Executed);
        let steps = store.query_init_steps(&run.id).expect("steps");
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].status, INIT_STEP_SUCCEEDED);
    }

    #[test]
    fn record_step_rejects_ordinal_kind_drift() {
        let (_dir, store) = open_store();
        let run = begin_run(&store, None, None, "{}").expect("begin");
        record_step(
            &store,
            &run,
            1,
            step_kind::AGENT_INSTALL,
            || Ok(false),
            || Ok(StepOutcome::empty()),
        )
        .expect("first");
        let err = record_step(
            &store,
            &run,
            1,
            step_kind::CONFIG_VALIDATE,
            || Ok(false),
            || Ok(StepOutcome::empty()),
        )
        .expect_err("kind drift must error");
        assert!(matches!(err, StackError::InitRunCorrupted { .. }));
    }
}
