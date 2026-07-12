//! Init run orchestrator.
//!
//! Wraps each logical phase of `acps init` (config validation, state
//! migration, secrets, agent install, Agent Skills install, workspace lanes,
//! headless config, edge artifacts, init-complete event, testflight) so that:
//!
//! 1. Every executed/resumed phase is recorded as an `init_steps` row keyed
//!    by `(run_id, ordinal)`. The CLI driver maintains the ordinal list as
//!    the canonical phase order — the runner does not invent step numbers.
//! 2. On rerun the orchestrator consults the prior row before invoking
//!    the phase body. If the prior status was `succeeded` and the caller-
//!    supplied verifier still passes, the row is replayed as `skipped` and
//!    the body is not executed. Anything else (`failed`, `pending`,
//!    `running`, or `succeeded` with a failing verifier) re-runs the body.
//! 3. The aggregate `init_runs.status` is settled by the driver via
//!    [`finalize_run`]: `succeeded` if every step succeeded or was
//!    skipped; `failed` if the body returned an error.
//!
//! Step bodies are arbitrary closures that may capture mutable state. The
//! runner does not constrain their argument shape — phases that mutate
//! config or secret-store state simply close over the values they need.

use std::path::Path;

use crate::error::{Result, StackError};
use crate::runtime::install::agent_installer::persist_step_logs_to_disk;
use crate::state::{
    INIT_RUN_FAILED, INIT_RUN_SUCCEEDED, INIT_STEP_FAILED, INIT_STEP_RUNNING, INIT_STEP_SKIPPED,
    INIT_STEP_SUCCEEDED, InitRunRecord, InitStepRecord, NewInitRun, NewInitStep, StateStore,
};

/// Stable identifiers for each phase of `acps init`. Persisted as
/// `init_steps.kind`. Adding a new phase means adding a constant here AND
/// extending the ordinal map in the CLI driver.
pub mod step_kind {
    pub const CONFIG_VALIDATE: &str = "config_validate";
    pub const STATE_INIT: &str = "state_init";
    pub const SECRETS_INIT: &str = "secrets_init";
    pub const AGENT_INSTALL: &str = "agent_install";
    pub const NATIVE_CONFIG_IMPORT: &str = "native_config_import";
    pub const AGENT_SKILLS_INSTALL: &str = "agent_skills_install";
    pub const DEPS_APPLY: &str = "deps_apply";
    pub const PROVIDER_CONFIGURE: &str = "provider_configure";
    pub const WORKSPACE_MATERIALIZE: &str = "workspace_materialize";
    pub const AGENT_HEADLESS_CONFIG: &str = "agent_headless_config";
    pub const EDGE_ARTIFACTS: &str = "edge_artifacts";
    pub const INIT_COMPLETE: &str = "init_complete";
    pub const TESTFLIGHT: &str = "testflight";
}

/// Outcome of executing one step. Returned by the body closure.
pub struct StepOutcome {
    /// Optional on-disk log directory. Steps that produce a log tree (the
    /// installer, workspace materialization) populate this so the operator
    /// can audit a failed run without re-running it; `None` for steps with
    /// no significant captured output.
    pub log_dir: Option<String>,
    /// Step-specific payload merged into `init_steps.payload_json`. Must be
    /// a valid JSON object literal (validated by the state layer's
    /// `json_valid` check).
    pub payload_json: String,
}

impl StepOutcome {
    pub fn empty() -> Self {
        Self {
            log_dir: None,
            payload_json: "{}".to_owned(),
        }
    }

    pub fn with_payload(payload_json: impl Into<String>) -> Self {
        Self {
            log_dir: None,
            payload_json: payload_json.into(),
        }
    }
}

/// Result of [`record_step`]. `Executed` means the body ran; `Skipped`
/// means a prior `succeeded` row's verifier still passes and the body was
/// not invoked. Driver code uses this to skip post-execution side effects
/// (e.g. re-printing logs) on resume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepDisposition {
    Executed,
    Skipped,
}

/// Begin a new init run, recording the args context. Returns the new run
/// record so the driver can persist the id and reference it from each
/// step.
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

/// Resume the most recent unfinished or failed run, if any. Returns
/// `None` only when no `pending`/`running`/`failed` row exists. A run
/// that completed successfully is not resumable — there is nothing to
/// continue — but a `failed` row IS resumable because that is the exact
/// case the operator wants to retry. Scans past newer succeeded rows so
/// a fresh `acps init` landing succeeded does not shadow an older
/// failed one.
pub fn find_resumable_run(store: &StateStore) -> Result<Option<InitRunRecord>> {
    store.latest_non_terminal_init_run()
}

/// Lookup a specific run by id. Used by `acps init resume --run <id>` so
/// the operator can target a historical row directly.
pub fn lookup_run(store: &StateStore, run_id: &str) -> Result<Option<InitRunRecord>> {
    store.lookup_init_run(run_id)
}

/// Mark an init run as `succeeded` or `failed`. The driver calls this
/// after the final step has settled.
pub fn finalize_run(store: &StateStore, run_id: &str, status: &str) -> Result<()> {
    store.finalize_init_run(run_id, status)
}

/// Execute one phase, persisting its row to `init_steps`. The ordinal is
/// caller-managed; passing the same ordinal twice within a run hits the
/// table's UNIQUE constraint and surfaces as an error.
///
/// Resume semantics: if a row with `(run_id, ordinal)` already exists and
/// its status is `succeeded`, `verify` is consulted. If `verify` returns
/// `Ok(true)` the row is replayed as `skipped` and `body` is not
/// executed; otherwise the existing row is reset to `running` and `body`
/// is re-invoked.
///
/// `body` returns either `Ok(StepOutcome)` (recorded as `succeeded`) or
/// `Err(StackError)` (recorded as `failed` with the error's
/// `event_kind()` as `error_kind`). The error is then propagated to the
/// caller so the driver can decide whether to abort or continue.
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

/// Like [`record_step`] but stamps `default_log_dir` onto the
/// `init_steps` row whether the body succeeds OR fails. Used by phases
/// (workspace materialization, in-process installers) that pre-create
/// the on-disk log directory before invoking the body — so a failure
/// halfway through the body still points the operator at the captured
/// stdout/stderr instead of recording `log_dir = NULL`. The success
/// path lets the body override via [`StepOutcome::log_dir`].
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
        && verify().unwrap_or(false)
    {
        // Postcondition still holds; replay as `skipped` and leave the
        // body unexecuted. We accept BOTH `succeeded` and `skipped` as
        // verifier-eligible so a chain of resumes against the same run
        // doesn't re-execute a verified step just because the prior
        // resume already marked it `skipped`. Verifier errors are
        // swallowed as `false` — a verifier that can't read its input
        // has to re-run the step.
        store.mark_init_step_skipped(&step_id, &resume_payload(existing))?;
        return Ok(StepDisposition::Skipped);
    }

    store.mark_init_step_running(&step_id)?;
    match body() {
        Ok(outcome) => {
            let log_dir = outcome.log_dir.as_deref().or(default_log_dir);
            store.mark_init_step_succeeded(&step_id, log_dir, &outcome.payload_json)?;
            Ok(StepDisposition::Executed)
        }
        Err(error) => {
            store.mark_init_step_failed(
                &step_id,
                default_log_dir,
                error.error_code(),
                &error.to_string(),
                "{}",
            )?;
            Err(error)
        }
    }
}

/// Variant of [`record_step`] that takes a draft row from the installer
/// module (already carrying captured stdout/stderr + log_dir) and persists
/// it alongside the init step. Used by the `agent_install` phase so the
/// underlying `installer_runs` row's log_dir can be mirrored onto the
/// init step row without re-walking the filesystem.
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
        && verify().unwrap_or(false)
    {
        store.mark_init_step_skipped(&step_id, &resume_payload(existing))?;
        return Ok(StepDisposition::Skipped);
    }

    store.mark_init_step_running(&step_id)?;
    match body() {
        Ok((mut draft, outcome)) => {
            // Persisting log files is best-attempted but we'd rather mark
            // the step as `failed` with a clear error than `succeeded`
            // with logs missing — the audit copy is the entire point of
            // this column. Same semantics as the installer module.
            persist_step_logs_to_disk(&mut draft, agent_id_for_logs, log_base)?;
            let log_dir = draft.log_dir.clone().or(outcome.log_dir);
            store.mark_init_step_succeeded(&step_id, log_dir.as_deref(), &outcome.payload_json)?;
            Ok(StepDisposition::Executed)
        }
        Err(error) => {
            store.mark_init_step_failed(
                &step_id,
                None,
                error.error_code(),
                &error.to_string(),
                "{}",
            )?;
            Err(error)
        }
    }
}

/// Build the payload JSON to record on a `skipped` step. Preserves the
/// prior payload (so installer_run correlations and attempt counters
/// survive resume) and flags `resume.verified = true` so log readers can
/// distinguish a fresh-success run from a verifier-skipped one. Uses
/// serde_json's tree model so nested objects round-trip safely — the
/// previous string-manipulation form mishandled payloads whose closing
/// brace was preceded by another `}` (e.g. an already-merged
/// `resume.verified` block on a third resume).
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

/// Aggregate result for the driver to call [`finalize_run`] with. The
/// driver tracks per-step dispositions and decides terminal status from
/// the set: any `failed` step means `INIT_RUN_FAILED`; otherwise
/// `INIT_RUN_SUCCEEDED`.
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
        // Regression: prior to this helper, a phase that pre-created an
        // on-disk log directory and then failed in the body left
        // `init_steps.log_dir = NULL`. The operator would then have no
        // pointer from SQLite to the captured failure logs. The helper
        // stamps `default_log_dir` on both the success and failure paths.
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
        // Resume succeeds on second attempt.
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

        // Even a failed prior run is resumable — the operator wants to
        // retry the failed step, not start over. Scan past any newer
        // succeeded row that may have landed since.
        finalize_run(&store, &in_flight.id, INIT_RUN_FAILED).expect("finalize");
        let still_resumable = find_resumable_run(&store)
            .expect("find")
            .expect("failed run should still be resumable");
        assert_eq!(still_resumable.id, in_flight.id);
        assert_eq!(still_resumable.status, INIT_RUN_FAILED);

        // Once every prior run is succeeded, there is nothing to resume.
        let later_success = begin_run(&store, None, None, "{}").expect("begin");
        finalize_run(&store, &later_success.id, INIT_RUN_SUCCEEDED).expect("finalize");
        // The failed `in_flight` row is still the latest non-terminal-or-failed,
        // since later_success is now terminal-succeeded.
        let found = find_resumable_run(&store)
            .expect("find")
            .expect("failed older run still wins");
        assert_eq!(found.id, in_flight.id);

        // Clear out the failed row and confirm a pure-success table yields None.
        finalize_run(&store, &in_flight.id, INIT_RUN_SUCCEEDED).expect("clear");
        let none = find_resumable_run(&store).expect("find");
        assert!(none.is_none(), "all-succeeded table returns None");
    }

    #[test]
    fn resume_payload_survives_nested_objects_across_multiple_skips() {
        // Regression: the prior string-trim form of resume_payload
        // chewed off the trailing `}` of an already-merged
        // `resume.verified` block on the second resume, producing
        // invalid JSON that the state-layer json_valid CHECK then
        // rejected — breaking chained resume.
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
