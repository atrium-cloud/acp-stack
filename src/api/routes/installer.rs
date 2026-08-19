//! Installer run history and in-flight progress read surface.
//!
//! `GET /v1/installer/runs` reads the `installer_runs` audit table. With
//! `?active=true` it returns only in-flight steps (`status = "running"`),
//! each carrying a server-computed `elapsed_seconds` — the polling shape a
//! platform driving instance init uses to render live install progress
//! (agent harness/adapter installs can run for minutes). Log contents stay
//! in the table preview columns and on disk; this endpoint returns step
//! metadata only.
//!
//! Reads go through a short-lived second `StateStore` connection, never the
//! daemon's shared store mutex: the deps-apply route holds that mutex for its
//! whole run, and a progress poller must not block behind it. WAL plus the
//! store busy-timeout make the read see autocommit `running` inserts as they
//! land.

use axum::extract::{Query, State};
use serde::{Deserialize, Serialize};

use super::super::core::AppState;
use crate::envelope::ApiSuccess;
use crate::error::StackError;
use crate::state::{INSTALLER_STATUS_RUNNING, InstallerRun, StateStore};

/// Per-request cap on `limit=` for history queries — same reasoning as the
/// logs endpoints (an authenticated caller must not pull unbounded history
/// into one response). Active-only queries ignore `limit`: the number of
/// concurrently running steps is bounded by the installers themselves.
pub(super) const MAX_RUNS_LIMIT: u32 = 1000;

pub(super) fn default_runs_limit() -> u32 {
    100
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub(crate) struct InstallerRunsParams {
    /// When true, only in-flight (`running`) rows are returned, oldest first.
    active: bool,
    /// Scope to one agent id (`deps_apply` covers dependency installs);
    /// absent returns rows for every agent.
    agent: Option<String>,
    #[serde(default = "default_runs_limit")]
    limit: u32,
}

#[derive(Serialize)]
pub(crate) struct InstallerRunsResponse {
    runs: Vec<InstallerRunJson>,
}

#[derive(Serialize)]
struct InstallerRunJson {
    id: String,
    agent_id: Option<String>,
    operation: String,
    step: String,
    method: Option<String>,
    status: String,
    started_at: String,
    finished_at: Option<String>,
    exit_status: Option<i32>,
    version: Option<String>,
    /// Seconds since `started_at`, computed server-side so pollers need no
    /// clock sync with the daemon; present only while the row is `running`.
    #[serde(skip_serializing_if = "Option::is_none")]
    elapsed_seconds: Option<i64>,
}

impl InstallerRunJson {
    fn from_run(run: InstallerRun, now: chrono::DateTime<chrono::Utc>) -> Self {
        let elapsed_seconds = if run.status == INSTALLER_STATUS_RUNNING {
            chrono::DateTime::parse_from_rfc3339(&run.started_at)
                .ok()
                .map(|started| {
                    (now - started.with_timezone(&chrono::Utc))
                        .num_seconds()
                        .max(0)
                })
        } else {
            None
        };
        Self {
            id: run.id,
            agent_id: run.agent_id,
            operation: run.operation,
            step: run.step,
            method: run.method,
            status: run.status,
            started_at: run.started_at,
            finished_at: run.finished_at,
            exit_status: run.exit_status,
            version: run.version,
            elapsed_seconds,
        }
    }
}

pub(crate) async fn installer_runs_handler(
    Query(params): Query<InstallerRunsParams>,
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<InstallerRunsResponse>, StackError> {
    Ok(ApiSuccess::new(read_installer_runs(
        &state.runtime_paths.state_path,
        &params,
    )?))
}

/// Read runs through a fresh connection opened from the state-db path. Never
/// touches the daemon's shared store handle: deps apply holds that mutex for
/// its whole run, and a progress poller must not block behind it. The query
/// is a small indexed read, so no `spawn_blocking`.
fn read_installer_runs(
    state_path: &std::path::Path,
    params: &InstallerRunsParams,
) -> crate::error::Result<InstallerRunsResponse> {
    let store = StateStore::open(state_path)?;
    let runs = if params.active {
        store.query_active_installer_runs(params.agent.as_deref())?
    } else {
        store.query_installer_runs_filtered(
            params.agent.as_deref(),
            params.limit.min(MAX_RUNS_LIMIT),
        )?
    };
    let now = chrono::Utc::now();
    Ok(InstallerRunsResponse {
        runs: runs
            .into_iter()
            .map(|run| InstallerRunJson::from_run(run, now))
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{INSTALLER_METHOD_SHELL, INSTALLER_OPERATION_INSTALL, InstallerRunInput};

    #[test]
    fn read_path_sees_running_rows_committed_by_another_connection() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = tempdir.path().join("state.sqlite");
        // Stands in for the daemon's shared store: migrated, written to,
        // then untouched — the read path must depend only on the db path.
        let writer = StateStore::open(&path).expect("open");
        writer.migrate().expect("migrate");
        writer
            .append_installer_run(InstallerRunInput {
                agent_id: "hermes-agent",
                started_at: "2026-05-21T00:00:00.000000000Z",
                finished_at: None,
                status: INSTALLER_STATUS_RUNNING,
                stdout: "",
                stderr: "",
                exit_status: None,
                step: "harness",
                version: None,
                operation: INSTALLER_OPERATION_INSTALL,
                method: Some(INSTALLER_METHOD_SHELL),
                log_dir: None,
                apply_run_id: None,
            })
            .expect("running row");

        let response = read_installer_runs(
            &path,
            &InstallerRunsParams {
                active: true,
                agent: Some("hermes-agent".to_owned()),
                limit: 100,
            },
        )
        .expect("read via fresh connection");
        assert_eq!(response.runs.len(), 1);
        assert_eq!(response.runs[0].step, "harness");
        assert_eq!(response.runs[0].status, INSTALLER_STATUS_RUNNING);
        assert!(response.runs[0].elapsed_seconds.is_some());
    }
}
