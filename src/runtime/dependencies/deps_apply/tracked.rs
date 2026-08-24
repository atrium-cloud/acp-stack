//! Durable-run wrapper around the apply runner: [`apply_dependencies_tracked`]
//! owns the `deps_apply_runs` row from cross-process single-flight claim to
//! terminal settlement.

use super::*;
use crate::runtime::process_runner::{current_boot_id, process_is_live};
use crate::state::{
    DEPS_APPLY_RUN_FAILED, DEPS_APPLY_RUN_PRIVILEGE_BLOCKED, DEPS_APPLY_RUN_RUNNING,
    DEPS_APPLY_RUN_SUCCEEDED, DepsApplyRunFinish, NewDepsApplyRun,
};

/// How the tracked wrapper obtains its `deps_apply_runs` row.
#[derive(Debug, Clone)]
pub enum TrackedApplyRun<'a> {
    /// Claim a fresh row (and single-flight slot) before running.
    Claim {
        origin: &'a str,
        init_run_id: Option<&'a str>,
    },
    /// Adopt a row the spawning parent claimed before this process existed.
    Adopt { apply_run_id: &'a str },
}

/// Liveness predicate for `deps_apply_runs` rows: the stored pid must exist
/// AND, when both sides know a boot id, the boot ids must match (a reused pid
/// from a previous boot is not the apply that stamped the row).
pub fn deps_run_liveness() -> impl Fn(i64, Option<&str>) -> bool {
    let boot_id = current_boot_id();
    move |pid, row_boot_id| {
        if let (Some(current), Some(row)) = (boot_id.as_deref(), row_boot_id)
            && current != row
        {
            return false;
        }
        process_is_live(pid)
    }
}

/// Terminal `deps_apply_runs.status` for a finished report, in precedence
/// order: `failed`, then `privilege_blocked`, then `succeeded`.
pub fn run_status_for_report(report: &DepsApplyReport) -> &'static str {
    let mut privilege_blocked = false;
    for result in &report.results {
        match &result.outcome {
            DepApplyOutcome::Failed { .. } => return DEPS_APPLY_RUN_FAILED,
            DepApplyOutcome::PrivilegeRequired { .. } => privilege_blocked = true,
            DepApplyOutcome::Installed | DepApplyOutcome::AlreadyPresent => {}
        }
    }
    if privilege_blocked {
        DEPS_APPLY_RUN_PRIVILEGE_BLOCKED
    } else {
        DEPS_APPLY_RUN_SUCCEEDED
    }
}

fn finish_for_report(report: &DepsApplyReport) -> DepsApplyRunFinish<'static> {
    let mut installed = 0;
    let mut already_present = 0;
    let mut privilege_required = 0;
    let mut failed = 0;
    for result in &report.results {
        match &result.outcome {
            DepApplyOutcome::Installed => installed += 1,
            DepApplyOutcome::AlreadyPresent => already_present += 1,
            DepApplyOutcome::PrivilegeRequired { .. } => privilege_required += 1,
            DepApplyOutcome::Failed { .. } => failed += 1,
        }
    }
    let status = run_status_for_report(report);
    DepsApplyRunFinish {
        status,
        completed: report.results.len() as i64,
        installed,
        already_present,
        privilege_required,
        failed,
        error_code: (status == DEPS_APPLY_RUN_FAILED).then_some("deps.apply_failed"),
        error_detail: None,
        payload_json: "{}",
    }
}

/// Run the apply with a durable `deps_apply_runs` row around it. The claim
/// error propagates before any install snippet runs; row updates after the
/// claim are best-effort so a row write cannot abort a half-run install.
pub fn apply_dependencies_tracked(
    config: &Config,
    store: &StateStore,
    run: TrackedApplyRun<'_>,
    feature: Option<&str>,
    shell_program: &str,
    escalation: &PrivilegeEscalation,
    mut progress: impl FnMut(usize, usize, &str) -> Result<()>,
) -> Result<DepsApplyReport> {
    let is_live = deps_run_liveness();
    let boot_id = current_boot_id();
    let apply_run_id = match run {
        TrackedApplyRun::Claim {
            origin,
            init_run_id,
        } => {
            let id = crate::state::next_deps_apply_run_id();
            store.claim_deps_apply_run(
                NewDepsApplyRun {
                    id: &id,
                    origin,
                    init_run_id,
                    feature,
                    pid: Some(i64::from(std::process::id())),
                    boot_id: boot_id.as_deref(),
                    total: candidates_for(config, feature).len() as i64,
                },
                &is_live,
            )?;
            id
        }
        TrackedApplyRun::Adopt { apply_run_id } => {
            let row = store.lookup_deps_apply_run(apply_run_id)?.ok_or_else(|| {
                StackError::InvalidParam {
                    field: "apply_run_id",
                    reason: format!("no deps_apply_runs row with id `{apply_run_id}` to adopt"),
                }
            })?;
            if row.status != DEPS_APPLY_RUN_RUNNING {
                return Err(StackError::InvalidParam {
                    field: "apply_run_id",
                    reason: format!(
                        "deps_apply_runs row `{apply_run_id}` is `{}`, not running",
                        row.status
                    ),
                });
            }
            apply_run_id.to_owned()
        }
    };

    let result = apply_dependencies_with_escalation(
        config,
        feature,
        Some(store),
        shell_program,
        escalation,
        Some(&apply_run_id),
        |current, total, name| {
            if let Err(error) =
                store.update_deps_apply_progress(&apply_run_id, (current - 1) as i64, Some(name))
            {
                tracing::warn!(%error, apply_run_id, "deps apply: failed to record progress");
            }
            progress(current, total, name)
        },
    );

    match result {
        Ok(report) => {
            if let Err(error) =
                store.finish_deps_apply_run(&apply_run_id, finish_for_report(&report))
            {
                tracing::warn!(%error, apply_run_id, "deps apply: failed to finalize run row");
            }
            Ok(report)
        }
        Err(error) => {
            // Spawn/IO failures abort the loop without a report; the row must
            // still settle so it cannot wedge the single-flight slot.
            let detail = error.to_string();
            if let Err(finish_error) = store.finish_deps_apply_run(
                &apply_run_id,
                DepsApplyRunFinish {
                    status: DEPS_APPLY_RUN_FAILED,
                    completed: 0,
                    installed: 0,
                    already_present: 0,
                    privilege_required: 0,
                    failed: 0,
                    error_code: Some(error.error_code()),
                    error_detail: Some(&detail),
                    payload_json: "{}",
                },
            ) {
                tracing::warn!(error = %finish_error, apply_run_id, "deps apply: failed to finalize run row after apply error");
            }
            Err(error)
        }
    }
}
