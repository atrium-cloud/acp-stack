//! Dependency-apply health collectors.
//!
//! The deps signal is derived from `installer_runs` rows written by the
//! deps_apply runner. Rows are grouped by `apply_run_id` where present, with a
//! timestamp-neighborhood fallback for legacy rows that predate migration 013.

use super::*;

pub(super) fn collect_deps(store: &StateStore) -> DepsHealth {
    let rows = match store
        .query_installer_runs_filtered(Some(DEPS_APPLY_AGENT_ID), DEPS_RECENT_ROW_LIMIT)
    {
        Ok(rows) => rows,
        Err(err) => {
            return DepsHealth {
                last_apply_at: None,
                last_apply_run_id: None,
                last_apply_status: None,
                last_apply_exit: None,
                cluster_has_failure: false,
                probe_error: Some(err.to_string()),
            };
        }
    };
    // Belt-and-suspenders: the SQL filter pivots on `agent_id`, but an
    // operator who set `agent.id = "deps_apply"` would otherwise leak agent
    // installer rows into the deps signal. Cross-check `step` here so the
    // signal is bound to rows the deps_apply runner itself wrote.
    let rows: Vec<_> = rows
        .into_iter()
        .filter(|row| row.step == DEPS_APPLY_STEP)
        .collect();
    let mut iter = rows.into_iter();
    let Some(latest) = iter.next() else {
        return DepsHealth {
            last_apply_at: None,
            last_apply_run_id: None,
            last_apply_status: None,
            last_apply_exit: None,
            cluster_has_failure: false,
            probe_error: None,
        };
    };
    let latest_apply_run_id = latest.apply_run_id.clone();
    let cluster_has_failure = match deps_cluster_has_failure_for_latest(store, &latest, iter) {
        Ok(value) => value,
        Err(err) => {
            return DepsHealth {
                last_apply_at: Some(latest.started_at),
                last_apply_run_id: latest_apply_run_id,
                last_apply_status: Some(latest.status),
                last_apply_exit: latest.exit_status,
                cluster_has_failure: false,
                probe_error: Some(err.to_string()),
            };
        }
    };
    DepsHealth {
        last_apply_at: Some(latest.started_at),
        last_apply_run_id: latest_apply_run_id,
        last_apply_status: Some(latest.status),
        last_apply_exit: latest.exit_status,
        cluster_has_failure,
        probe_error: None,
    }
}

pub fn deps_cluster_has_failure_for_latest(
    store: &StateStore,
    latest: &InstallerRun,
    legacy_rows: impl Iterator<Item = InstallerRun>,
) -> Result<bool> {
    if let Some(apply_run_id) = latest.apply_run_id.as_deref() {
        let run_rows = store.query_installer_runs_for_apply_run(
            DEPS_APPLY_AGENT_ID,
            DEPS_APPLY_STEP,
            apply_run_id,
        )?;
        return Ok(run_rows
            .iter()
            .any(|row| deps_status_is_failure(&row.status)));
    }
    Ok(legacy_timestamp_cluster_has_failure(latest, legacy_rows))
}

fn legacy_timestamp_cluster_has_failure(
    latest: &InstallerRun,
    iter: impl Iterator<Item = InstallerRun>,
) -> bool {
    // Legacy rows predate migration 013 and have no apply-run identity. Keep
    // the old timestamp neighborhood as a compatibility fallback only; new
    // rows use exact `apply_run_id` grouping above.
    let mut cluster_has_failure = deps_status_is_failure(&latest.status);
    if let Ok(mut previous_at) = chrono::DateTime::parse_from_rfc3339(&latest.started_at) {
        for row in iter {
            let Ok(row_at) = chrono::DateTime::parse_from_rfc3339(&row.started_at) else {
                break;
            };
            let gap = previous_at - row_at;
            if gap.num_seconds() > DEPS_CLUSTER_GAP_SECS {
                break;
            }
            if deps_status_is_failure(&row.status) {
                cluster_has_failure = true;
            }
            previous_at = row_at;
        }
    }
    cluster_has_failure
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed_deps_apply_row(store: &StateStore, started_at: &str, status: &str, exit: Option<i32>) {
        store
            .append_installer_run(crate::state::InstallerRunInput {
                agent_id: DEPS_APPLY_AGENT_ID,
                started_at,
                finished_at: Some(started_at),
                status,
                stdout: "",
                stderr: "",
                exit_status: exit,
                step: "deps_apply",
                version: None,
                operation: crate::state::INSTALLER_OPERATION_INSTALL,
                method: Some(crate::state::INSTALLER_METHOD_SHELL),
                log_dir: None,
                apply_run_id: None,
            })
            .expect("seed deps_apply row");
    }

    fn seed_deps_apply_row_for_run(
        store: &StateStore,
        started_at: &str,
        status: &str,
        exit: Option<i32>,
        apply_run_id: &str,
    ) {
        store
            .append_installer_run(crate::state::InstallerRunInput {
                agent_id: DEPS_APPLY_AGENT_ID,
                started_at,
                finished_at: Some(started_at),
                status,
                stdout: "",
                stderr: "",
                exit_status: exit,
                step: DEPS_APPLY_STEP,
                version: None,
                operation: crate::state::INSTALLER_OPERATION_INSTALL,
                method: Some(crate::state::INSTALLER_METHOD_SHELL),
                log_dir: None,
                apply_run_id: Some(apply_run_id),
            })
            .expect("seed deps_apply row");
    }

    #[test]
    fn collect_deps_surfaces_probe_error_when_installer_runs_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = StateStore::open(dir.path().join("state.sqlite")).expect("open");
        // Skip migrate(); `installer_runs` does not exist. The probe must
        // surface this as `probe_error` instead of returning "no apply runs"
        // (regression test for the silent-swallow finding from Codex audit).
        let deps = collect_deps(&store);
        assert!(
            deps.probe_error.is_some(),
            "expected probe_error when installer_runs is missing, got {deps:?}"
        );
        assert!(deps.last_apply_at.is_none());
    }

    #[test]
    fn collect_deps_with_no_rows_reports_no_probe_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = StateStore::open(dir.path().join("state.sqlite")).expect("open");
        store.migrate().expect("migrate");
        let deps = collect_deps(&store);
        assert!(deps.probe_error.is_none());
        assert!(deps.last_apply_at.is_none());
        assert!(deps.last_apply_status.is_none());
        assert!(!deps.cluster_has_failure);
    }

    #[test]
    fn collect_deps_partial_failure_in_same_invocation_marks_cluster_failed() {
        // Regression for the Codex-audit P1: A fails at t=0, B succeeds at
        // t=5s. The latest row alone (B=installed) would falsely report
        // healthy; the cluster heuristic must surface the failure.
        let dir = tempfile::tempdir().expect("tempdir");
        let store = StateStore::open(dir.path().join("state.sqlite")).expect("open");
        store.migrate().expect("migrate");
        seed_deps_apply_row(&store, "2026-05-25T00:00:00.000000000Z", "failed", Some(1));
        seed_deps_apply_row(
            &store,
            "2026-05-25T00:00:05.000000000Z",
            "installed",
            Some(0),
        );
        let deps = collect_deps(&store);
        assert_eq!(deps.last_apply_status.as_deref(), Some("installed"));
        assert!(
            deps.cluster_has_failure,
            "older failed row within cluster window must be surfaced, got {deps:?}"
        );
    }

    #[test]
    fn collect_deps_retry_outside_cluster_window_does_not_taint_latest() {
        // Apply 1 fails at t=0. Operator fixes the dep and re-applies at
        // t=30min — outside the 15-minute cluster window, so the older
        // failed row should not taint the healthy retry. Window covers the
        // 10-min worst-case per-step timeout in `runtime/dependencies/deps_apply.rs`.
        let dir = tempfile::tempdir().expect("tempdir");
        let store = StateStore::open(dir.path().join("state.sqlite")).expect("open");
        store.migrate().expect("migrate");
        seed_deps_apply_row(&store, "2026-05-25T00:00:00.000000000Z", "failed", Some(1));
        seed_deps_apply_row(
            &store,
            "2026-05-25T00:30:00.000000000Z",
            "installed",
            Some(0),
        );
        let deps = collect_deps(&store);
        assert_eq!(deps.last_apply_status.as_deref(), Some("installed"));
        assert!(
            !deps.cluster_has_failure,
            "30-minute gap should isolate the retry cluster, got {deps:?}"
        );
    }

    #[test]
    fn collect_deps_same_apply_run_id_keeps_failure_outside_legacy_window() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = StateStore::open(dir.path().join("state.sqlite")).expect("open");
        store.migrate().expect("migrate");
        seed_deps_apply_row_for_run(
            &store,
            "2026-05-25T00:00:00.000000000Z",
            "failed",
            Some(1),
            "dap_exact",
        );
        seed_deps_apply_row_for_run(
            &store,
            "2026-05-25T01:00:00.000000000Z",
            "installed",
            Some(0),
            "dap_exact",
        );
        let deps = collect_deps(&store);
        assert_eq!(deps.last_apply_run_id.as_deref(), Some("dap_exact"));
        assert_eq!(deps.last_apply_status.as_deref(), Some("installed"));
        assert!(
            deps.cluster_has_failure,
            "same apply_run_id must group exactly even across a large timestamp gap, got {deps:?}"
        );
    }

    #[test]
    fn collect_deps_different_apply_run_id_isolates_latest_retry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = StateStore::open(dir.path().join("state.sqlite")).expect("open");
        store.migrate().expect("migrate");
        seed_deps_apply_row_for_run(
            &store,
            "2026-05-25T00:00:00.000000000Z",
            "failed",
            Some(1),
            "dap_failed",
        );
        seed_deps_apply_row_for_run(
            &store,
            "2026-05-25T00:01:00.000000000Z",
            "installed",
            Some(0),
            "dap_retry",
        );
        let deps = collect_deps(&store);
        assert_eq!(deps.last_apply_run_id.as_deref(), Some("dap_retry"));
        assert_eq!(deps.last_apply_status.as_deref(), Some("installed"));
        assert!(
            !deps.cluster_has_failure,
            "new apply_run_id must not be tainted by an older failed invocation, got {deps:?}"
        );
    }

    #[test]
    fn collect_deps_long_apply_keeps_cluster_via_walking_gap() {
        // Regression for the second Codex-audit finding: a long sequential
        // apply that writes `failed@T+0`, `installed@T+4m`, `installed@T+8m`
        // is one cluster even though T+0 is 8 minutes away from T+8m. The
        // walking-gap heuristic compares each row to its immediate
        // predecessor, so adjacent 4-minute gaps stay inside the 15-minute
        // window.
        let dir = tempfile::tempdir().expect("tempdir");
        let store = StateStore::open(dir.path().join("state.sqlite")).expect("open");
        store.migrate().expect("migrate");
        seed_deps_apply_row(&store, "2026-05-25T00:00:00.000000000Z", "failed", Some(1));
        seed_deps_apply_row(
            &store,
            "2026-05-25T00:04:00.000000000Z",
            "installed",
            Some(0),
        );
        seed_deps_apply_row(
            &store,
            "2026-05-25T00:08:00.000000000Z",
            "installed",
            Some(0),
        );
        let deps = collect_deps(&store);
        assert_eq!(deps.last_apply_status.as_deref(), Some("installed"));
        assert!(
            deps.cluster_has_failure,
            "walking-gap cluster must retain the T+0 failure across an 8-minute span of sequential rows, got {deps:?}"
        );
    }

    #[test]
    fn collect_deps_filters_by_step_to_avoid_agent_id_sentinel_collision() {
        // Belt-and-suspenders: if an operator sets `agent.id = "deps_apply"`,
        // agent installer rows would share the `agent_id` filter. The `step`
        // filter cross-checks so only rows that the deps_apply runner wrote
        // contribute to the deps signal.
        let dir = tempfile::tempdir().expect("tempdir");
        let store = StateStore::open(dir.path().join("state.sqlite")).expect("open");
        store.migrate().expect("migrate");
        // Agent installer row that happens to share `agent_id = "deps_apply"`.
        store
            .append_installer_run(crate::state::InstallerRunInput {
                agent_id: DEPS_APPLY_AGENT_ID,
                started_at: "2026-05-25T00:00:00.000000000Z",
                finished_at: Some("2026-05-25T00:00:01.000000000Z"),
                status: "failed",
                stdout: "",
                stderr: "",
                exit_status: Some(1),
                step: "install",
                version: None,
                operation: crate::state::INSTALLER_OPERATION_INSTALL,
                method: Some(crate::state::INSTALLER_METHOD_SHELL),
                log_dir: None,
                apply_run_id: None,
            })
            .expect("seed colliding agent installer row");
        let deps = collect_deps(&store);
        assert!(
            deps.last_apply_at.is_none(),
            "rows with step != DEPS_APPLY_STEP must be filtered out, got {deps:?}"
        );
        assert!(!deps.cluster_has_failure);
    }

    #[test]
    fn collect_deps_privilege_required_in_cluster_marks_failing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = StateStore::open(dir.path().join("state.sqlite")).expect("open");
        store.migrate().expect("migrate");
        seed_deps_apply_row(
            &store,
            "2026-05-25T00:00:00.000000000Z",
            "privilege_required",
            None,
        );
        seed_deps_apply_row(&store, "2026-05-25T00:00:30.000000000Z", "skipped", Some(0));
        let deps = collect_deps(&store);
        assert!(
            deps.cluster_has_failure,
            "privilege_required must count as cluster failure, got {deps:?}"
        );
    }
}
