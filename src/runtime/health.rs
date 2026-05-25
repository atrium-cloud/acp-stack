//! Runtime health report.
//!
//! Aggregates the signals that distinguish a healthy daemon from one that is
//! struggling: SQLite reachability, workspace writability, agent process
//! state, external logging sink backlog, and the most recent dependency
//! apply. Consumed by `GET /v1/health/live`, `GET /v1/health/ready`, and the
//! `acps status` CLI. Each subsystem is reported individually so operators
//! can correlate `ok = false` to a concrete failing subsystem.
//!
//! Failure handling: unlike `security::check`, this helper never propagates
//! the underlying error. The whole point of the report is to *describe*
//! degraded state, so a SQLite query that returns `Err` becomes a row with
//! `reachable = false` and the error message captured in the report. Tests
//! exercise both the healthy and the degraded paths.

use std::path::Path;

use serde::Serialize;

use crate::api::AppState;
use crate::error::Result;
use crate::ownership;
use crate::runtime::deps_apply::{DEPS_APPLY_AGENT_ID, DEPS_APPLY_STEP};
use crate::state::{InstallerRun, StateStore};

// Threshold above which the sink subsystem is reported as failing. The sink
// worker writes to `sink_failures_summary` after at least one retry, so a
// single open failure already means external logging is lagging in a way the
// operator should know about.
const SINK_FAILURE_FAIL_THRESHOLD: i64 = 1;

// `installer_runs.status` values written by `acps deps apply` (mirroring
// `runtime/deps_apply.rs::DepApplyOutcome::status_label`). Rows tagged
// `installed` or `skipped` are healthy; `failed` and `privilege_required`
// mean the last apply attempt did not deliver the dependency. Made `pub`
// so `cli::status` can apply the same cluster heuristic with identical
// constants — duplicating these in the CLI led to drift in earlier passes.
pub const DEPS_STATUS_FAILED: &str = "failed";
pub const DEPS_STATUS_PRIVILEGE_REQUIRED: &str = "privilege_required";

// Upper bound on rows scanned when reconstructing the most recent apply
// invocation. `acps deps apply` writes one row per dep, so 50 rows comfortably
// covers any realistic apply session — operator-declared deps in the wild
// run in the single digits, so 50 leaves an order-of-magnitude headroom
// before this limit would silently truncate a cluster scan.
pub const DEPS_RECENT_ROW_LIMIT: u32 = 50;

// Rows within this duration of each other are treated as belonging to the
// same `acps deps apply` invocation. Used to aggregate per-dep rows into a
// single "most recent apply session" signal, since the schema does not
// persist an apply-run id. The window must cover the worst-case per-step
// runtime: `runtime/deps_apply.rs::DEFAULT_TIMEOUT` is 10 minutes, so a dep
// can plausibly take that long before its successor's row appears. 15
// minutes leaves slack on top of that without aliasing two distinct
// operator invocations into a single cluster.
pub const DEPS_CLUSTER_GAP_SECS: i64 = 15 * 60;

/// True when an `installer_runs.status` value (as written by `acps deps apply`)
/// represents a per-dep failure that should promote `deps` to the failing
/// list. Shared with `cli::status` so the CLI and HTTP readiness signal stay
/// in lock-step on the classification.
pub fn deps_status_is_failure(status: &str) -> bool {
    status == DEPS_STATUS_FAILED || status == DEPS_STATUS_PRIVILEGE_REQUIRED
}

#[derive(Debug, Clone, Serialize)]
pub struct HealthReport {
    pub ok: bool,
    pub failing: Vec<String>,
    pub sqlite: SqliteHealth,
    pub workspace: WorkspaceHealth,
    pub agent: AgentHealth,
    pub sink: SinkHealth,
    pub deps: DepsHealth,
}

#[derive(Debug, Clone, Serialize)]
pub struct SqliteHealth {
    pub reachable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema_version: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_event_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceHealth {
    pub writable: bool,
    pub path: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentHealth {
    pub configured: bool,
    pub id: String,
    /// `stopped` | `starting` | `running` | `stopping` — `Debug`-derived
    /// `AgentStateLabel` lower-cased so HTTP consumers see stable snake-case
    /// values instead of `Stopped`/`Running`.
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SinkHealth {
    pub enabled: bool,
    pub open_failure_count: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_failure_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_error: Option<String>,
    /// Set when the sink-state queries themselves failed (corrupt table,
    /// schema mismatch). Distinct from `latest_error`, which carries the
    /// Supabase upload error captured by the worker. A non-empty value here
    /// promotes the sink to the `failing` list regardless of
    /// `open_failure_count`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probe_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DepsHealth {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_apply_run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_apply_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_apply_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_apply_exit: Option<i32>,
    /// True when any row in the most recent apply cluster reports `failed` or
    /// `privilege_required`. `acps deps apply` writes one `installer_runs`
    /// row per declared dependency, so a single apply invocation that
    /// partially fails produces a mix of per-dep rows where the newest row
    /// alone is not representative. The cluster is reconstructed by walking
    /// the most recent rows newest-to-oldest and stopping at the first
    /// gap larger than `DEPS_CLUSTER_GAP_SECS` (15 minutes); the worst status
    /// in that cluster wins.
    pub cluster_has_failure: bool,
    /// Set when the `installer_runs` lookup for `acps deps apply` itself
    /// errored. Surfaced so a corrupt or missing table cannot make the deps
    /// section look healthy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probe_error: Option<String>,
}

impl HealthReport {
    /// Collect a fresh report from the running daemon. Locks the state store
    /// once and reads every persistent signal under that lock; the supervisor
    /// snapshot is taken after the lock is released so a slow query never
    /// blocks the bridge mutex.
    pub async fn collect(state: &AppState) -> Self {
        let sqlite;
        let sink;
        let deps;
        {
            let store = state.state.lock().await;
            sqlite = collect_sqlite(&store);
            let supabase_enabled = state
                .config
                .logging
                .supabase
                .as_ref()
                .is_some_and(|sb| sb.enabled);
            sink = collect_sink(&store, supabase_enabled);
            deps = collect_deps(&store);
        }
        let workspace = collect_workspace(&state.config.workspace.root);
        let agent = collect_agent(state).await;

        let mut failing = Vec::new();
        if !sqlite.reachable {
            failing.push("sqlite".to_owned());
        }
        if !workspace.writable {
            failing.push("workspace".to_owned());
        }
        if sink.enabled
            && (sink.probe_error.is_some()
                || sink.open_failure_count >= SINK_FAILURE_FAIL_THRESHOLD)
        {
            failing.push("sink".to_owned());
        }
        if deps.probe_error.is_some() || deps.cluster_has_failure {
            failing.push("deps".to_owned());
        }
        Self {
            ok: failing.is_empty(),
            failing,
            sqlite,
            workspace,
            agent,
            sink,
            deps,
        }
    }
}

fn collect_sqlite(store: &StateStore) -> SqliteHealth {
    let schema_version = match store.schema_version() {
        Ok(value) => value,
        Err(err) => {
            return SqliteHealth {
                reachable: false,
                schema_version: None,
                latest_event_at: None,
                error: Some(err.to_string()),
            };
        }
    };
    let latest_event_at = match store.latest_event_timestamp() {
        Ok(value) => value,
        Err(err) => {
            return SqliteHealth {
                reachable: false,
                schema_version: Some(schema_version),
                latest_event_at: None,
                error: Some(err.to_string()),
            };
        }
    };
    SqliteHealth {
        reachable: true,
        schema_version: Some(schema_version),
        latest_event_at,
        error: None,
    }
}

fn collect_workspace(root: &str) -> WorkspaceHealth {
    WorkspaceHealth {
        writable: ownership::workspace_writable(Path::new(root)),
        path: root.to_owned(),
    }
}

async fn collect_agent(state: &AppState) -> AgentHealth {
    let snapshot = state.agent_supervisor.snapshot().await;
    let id = state.config.agent.id.clone();
    AgentHealth {
        configured: !id.is_empty(),
        id,
        state: snapshot.state.as_wire_str().to_owned(),
        pid: snapshot.pid,
    }
}

fn collect_sink(store: &StateStore, enabled: bool) -> SinkHealth {
    if !enabled {
        return SinkHealth {
            enabled: false,
            open_failure_count: 0,
            latest_failure_at: None,
            latest_error: None,
            probe_error: None,
        };
    }
    // Capture probe failures as a sink finding rather than dropping them.
    // `security_check_handler` propagates the same errors as 500s; the
    // readiness report instead surfaces them in the sink subsystem so
    // `/v1/health/ready` returns 503 with `failing: ["sink"]` and the
    // operator can see the probe error directly.
    let (open_failure_count, probe_error_from_count) = match store.sink_open_failure_count() {
        Ok(count) => (count, None),
        Err(err) => (0, Some(err.to_string())),
    };
    let (latest_failure_at, latest_error, probe_error_from_summary) =
        match store.latest_sink_failure_summary() {
            Ok(Some((_window_started_at, _count, last_error, last_observed_at))) => {
                (Some(last_observed_at), last_error, None)
            }
            Ok(None) => (None, None, None),
            Err(err) => (None, None, Some(err.to_string())),
        };
    SinkHealth {
        enabled: true,
        open_failure_count,
        latest_failure_at,
        latest_error,
        probe_error: probe_error_from_count.or(probe_error_from_summary),
    }
}

fn collect_deps(store: &StateStore) -> DepsHealth {
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

    #[test]
    fn collect_sink_disabled_returns_empty_health() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = StateStore::open(dir.path().join("state.sqlite")).expect("open");
        store.migrate().expect("migrate");
        let sink = collect_sink(&store, false);
        assert!(!sink.enabled);
        assert_eq!(sink.open_failure_count, 0);
        assert!(sink.probe_error.is_none());
    }

    #[test]
    fn collect_sink_enabled_with_no_rows_reports_zero_failures() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = StateStore::open(dir.path().join("state.sqlite")).expect("open");
        store.migrate().expect("migrate");
        let sink = collect_sink(&store, true);
        assert!(sink.enabled);
        assert_eq!(sink.open_failure_count, 0);
        assert!(sink.latest_failure_at.is_none());
        assert!(sink.probe_error.is_none());
    }

    #[test]
    fn collect_sink_surfaces_probe_error_when_table_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = StateStore::open(dir.path().join("state.sqlite")).expect("open");
        // Deliberately skip `migrate()` so `sink_outbox` / `sink_failures_summary`
        // do not exist. The probe must surface this as `probe_error`
        // (regression test for the silent-swallow finding from Codex audit).
        let sink = collect_sink(&store, true);
        assert!(sink.enabled);
        assert!(
            sink.probe_error.is_some(),
            "expected probe_error when sink tables are missing, got {sink:?}"
        );
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
                log_dir: None,
                apply_run_id: Some(apply_run_id),
            })
            .expect("seed deps_apply row");
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
        // 10-min worst-case per-step timeout in `runtime/deps_apply.rs`.
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
