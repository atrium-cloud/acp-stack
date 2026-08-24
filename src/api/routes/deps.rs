use axum::Json;
use axum::extract::{Path, Query, State};
use serde::{Deserialize, Serialize};

use super::super::core::AppState;
use super::installer::InstallerRunJson;
use crate::envelope::ApiSuccess;
use crate::error::StackError;
use crate::runtime::dependencies::deps_apply::{
    DEPS_APPLY_AGENT_ID, DEPS_APPLY_STEP, DepsApplyReport, TrackedApplyRun,
    apply_dependencies_tracked, candidate_summary_line, candidates_for, deps_run_liveness,
    escalation_for,
};
use crate::state::{
    DEPS_APPLY_ORIGIN_API, DEPS_APPLY_RUN_FAILED, DEPS_APPLY_RUN_PRIVILEGE_BLOCKED,
    DEPS_APPLY_RUN_RUNNING, DepsApplyRunRecord, StateStore,
};

pub(crate) async fn deps_get_handler(
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<crate::runtime::dependencies::deps::DepsReport>, StackError> {
    Ok(ApiSuccess::new(
        crate::runtime::dependencies::deps::check_dependencies(&state.config),
    ))
}

pub(crate) async fn deps_check_handler(
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<crate::runtime::dependencies::deps::DepsReport>, StackError> {
    Ok(ApiSuccess::new(
        crate::runtime::dependencies::deps::check_dependencies(&state.config),
    ))
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub(crate) struct DepsApplyBody {
    /// Operator must set `confirmation = true` for the apply to run.
    /// Mirrors the CLI's `--yes` flag; without it the endpoint returns
    /// a structured preview without spawning any subprocess.
    #[serde(default)]
    confirmation: bool,
    /// Optional `feature` filter — only deps whose `feature` matches
    /// are eligible. `None` means apply every actionable dep.
    #[serde(default)]
    feature: Option<String>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub(crate) struct DepsApplyResponse {
    /// True when subprocesses ran. False on a preview call
    /// (`confirmation = false`); the operator sees the candidate list
    /// without any side effects.
    applied: bool,
    /// One-line summaries of every candidate action, including
    /// scope=system warnings.
    candidates: Vec<String>,
    /// Full apply report when `applied = true`. `None` on preview.
    #[serde(skip_serializing_if = "Option::is_none")]
    report: Option<DepsApplyReport>,
}

/// Admin-tier (declared at the router): runs operator-declared shell
/// snippets, including `scope = "system"` actions, so the blast radius
/// is far beyond what a session-tier caller should have — and when the
/// daemon is non-root with passwordless sudo configured, system-scope
/// actions escalate through `sudo -n`. The CLI route (`acps deps apply`)
/// uses the same runner.
pub(crate) async fn deps_apply_handler(
    State(state): State<AppState>,
    body: Option<Json<DepsApplyBody>>,
) -> std::result::Result<ApiSuccess<DepsApplyResponse>, StackError> {
    let Json(payload) = body.unwrap_or_default();
    let candidates = candidates_for(&state.config, payload.feature.as_deref());
    let summaries: Vec<String> = candidates.iter().map(candidate_summary_line).collect();
    if !payload.confirmation {
        return Ok(ApiSuccess::new(DepsApplyResponse {
            applied: false,
            candidates: summaries,
            report: None,
        }));
    }

    // Single-flight is enforced twice: the durable authority is the `deps_apply_runs`
    // claim inside `apply_dependencies_tracked` (which also covers other processes),
    // while `deps_apply_lock` is the in-process fast path so a concurrent request 409s
    // instead of racing the claim. Never a queue.
    //
    // The daemon's shared StateStore mutex is taken only for `migrate()` and released
    // before any install snippet runs; held across the apply it would park the whole
    // HTTP surface, since the `api.request` audit middleware takes it after every
    // response. Audit rows go through a second short-lived connection instead.
    let Ok(apply_guard) = state.deps_apply_lock.clone().try_lock_owned() else {
        // Best-effort id lookup for the 409 body.
        let running = match StateStore::open(&state.runtime_paths.state_path)
            .and_then(|store| store.running_deps_apply_run())
        {
            Ok(running) => running,
            Err(error) => {
                tracing::warn!(%error, "deps apply: failed to read the running apply row for the in-flight rejection");
                None
            }
        };
        return Err(StackError::DepsApplyInFlight {
            apply_run_id: running.map(|run| run.id).unwrap_or_default(),
        });
    };
    let config = state.config.clone();
    let feature = payload.feature.clone();
    let store_handle = state.state.clone();
    let report = tokio::task::spawn_blocking(
        move || -> std::result::Result<DepsApplyReport, StackError> {
            // The guard travels into the blocking task, not the handler future: a
            // client disconnect drops the future while the apply keeps running, and a
            // guard released there would let the next apply interleave with this one.
            let _apply_guard = apply_guard;
            // Migrate under the shared handle: the schema must be current before the
            // first audit row, or an install snippet could run unrecorded.
            let state_path = {
                let store = store_handle.blocking_lock();
                store.migrate()?;
                store.path().to_path_buf()
            };
            let apply_store = StateStore::open(&state_path)?;
            // Holding the guard proves no in-process apply is live, so a self-owned
            // `running` row is a prior apply whose terminal write failed. Its pid stays
            // live for the daemon's lifetime, so only this clears it before the claim.
            let cleared = apply_store.fail_self_owned_stale_deps_apply_runs(
                i64::from(std::process::id()),
                crate::runtime::process_runner::current_boot_id().as_deref(),
            )?;
            if cleared > 0 {
                tracing::warn!(
                    cleared,
                    "deps apply: settled self-owned run row(s) left running by a failed terminal write"
                );
            }
            let shell = &config.workspace.default_shell;
            let escalation = escalation_for(&config, feature.as_deref());
            apply_dependencies_tracked(
                &config,
                &apply_store,
                TrackedApplyRun::Claim {
                    origin: DEPS_APPLY_ORIGIN_API,
                    init_run_id: None,
                },
                feature.as_deref(),
                shell,
                &escalation,
                |_, _, _| Ok(()),
            )
        },
    )
    .await
    .map_err(|err| StackError::AgentInitializeFailed {
        reason: format!("deps apply thread join failed: {err}"),
    })??;

    Ok(ApiSuccess::new(DepsApplyResponse {
        applied: true,
        candidates: summaries,
        report: Some(report),
    }))
}

/// Per-request cap on `limit=` for run-history queries; mirrors the
/// installer-runs endpoint's bound.
const MAX_DEPS_APPLY_RUNS_LIMIT: u32 = 1000;

fn default_deps_apply_runs_limit() -> u32 {
    50
}

#[derive(Deserialize, Default, schemars::JsonSchema)]
#[serde(default)]
pub(crate) struct DepsApplyRunsParams {
    #[serde(default = "default_deps_apply_runs_limit")]
    limit: u32,
}

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct DepsApplyRunsResponse {
    runs: Vec<DepsApplyRunJson>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct DepsApplyRunProgressJson {
    completed: i64,
    total: i64,
    current_dep: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct DepsApplyRunCountsJson {
    installed: i64,
    already_present: i64,
    privilege_required: i64,
    failed: i64,
}

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct DepsApplyRunErrorJson {
    code: String,
    detail: Option<String>,
}

/// One `deps_apply_runs` row for the polling surface. `running` rows are
/// reconciled against process liveness before serialization, so a crashed
/// apply reads as `failed` + `retryable` here rather than `running` forever.
#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct DepsApplyRunJson {
    apply_run_id: String,
    /// `running`, `succeeded`, `failed`, or `privilege_blocked`.
    status: String,
    /// Surface that started the apply: `init`, `init_background`, `cli`, or
    /// `api`.
    origin: String,
    init_run_id: Option<String>,
    feature: Option<String>,
    started_at: String,
    finished_at: Option<String>,
    progress: DepsApplyRunProgressJson,
    counts: DepsApplyRunCountsJson,
    /// Whether the owning process is alive right now; meaningful while
    /// `status = "running"`.
    live: bool,
    /// True for terminal non-success states a re-`POST /v1/deps/apply` can
    /// retry (the apply is idempotent over already-installed deps).
    retryable: bool,
    log_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<DepsApplyRunErrorJson>,
}

impl DepsApplyRunJson {
    fn from_record(
        record: DepsApplyRunRecord,
        is_live: &dyn Fn(i64, Option<&str>) -> bool,
    ) -> Self {
        let live = record.status == DEPS_APPLY_RUN_RUNNING
            && record
                .pid
                .map(|pid| is_live(pid, record.boot_id.as_deref()))
                .unwrap_or(false);
        let retryable = matches!(
            record.status.as_str(),
            DEPS_APPLY_RUN_FAILED | DEPS_APPLY_RUN_PRIVILEGE_BLOCKED
        );
        Self {
            apply_run_id: record.id,
            status: record.status,
            origin: record.origin,
            init_run_id: record.init_run_id,
            feature: record.feature,
            started_at: record.started_at,
            finished_at: record.finished_at,
            progress: DepsApplyRunProgressJson {
                completed: record.completed,
                total: record.total,
                current_dep: record.current_dep,
            },
            counts: DepsApplyRunCountsJson {
                installed: record.installed,
                already_present: record.already_present,
                privilege_required: record.privilege_required,
                failed: record.failed,
            },
            live,
            retryable,
            log_dir: record.log_dir,
            error: record.error_code.map(|code| DepsApplyRunErrorJson {
                code,
                detail: record.error_detail,
            }),
        }
    }
}

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct DepsApplyRunDetailResponse {
    #[serde(flatten)]
    run: DepsApplyRunJson,
    /// Per-action `installer_runs` rows sharing this apply_run_id, oldest
    /// first. Step metadata only, never captured log contents.
    actions: Vec<InstallerRunJson>,
}

/// Open a fresh read connection and reconcile stale `running` rows first, so
/// every response reflects real process liveness. Never touches the daemon's
/// shared store mutex — a poll must answer while an apply holds it.
fn open_reconciled_deps_store(state: &AppState) -> crate::error::Result<StateStore> {
    let store = StateStore::open(&state.runtime_paths.state_path)?;
    let is_live = deps_run_liveness();
    if let Err(error) = store.reconcile_stale_deps_apply_runs(&is_live) {
        tracing::warn!(%error, "deps apply runs: stale-row reconcile failed; serving unreconciled rows");
    }
    Ok(store)
}

pub(crate) async fn deps_apply_runs_handler(
    Query(params): Query<DepsApplyRunsParams>,
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<DepsApplyRunsResponse>, StackError> {
    let store = open_reconciled_deps_store(&state)?;
    let is_live = deps_run_liveness();
    let runs = store.query_deps_apply_runs(params.limit.min(MAX_DEPS_APPLY_RUNS_LIMIT))?;
    Ok(ApiSuccess::new(DepsApplyRunsResponse {
        runs: runs
            .into_iter()
            .map(|record| DepsApplyRunJson::from_record(record, &is_live))
            .collect(),
    }))
}

pub(crate) async fn deps_apply_run_latest_handler(
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<DepsApplyRunDetailResponse>, StackError> {
    let store = open_reconciled_deps_store(&state)?;
    let record =
        store
            .latest_deps_apply_run()?
            .ok_or_else(|| StackError::DepsApplyRunNotFound {
                apply_run_id: "latest".to_owned(),
            })?;
    deps_apply_run_detail(&store, record)
}

pub(crate) async fn deps_apply_run_get_handler(
    Path(apply_run_id): Path<String>,
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<DepsApplyRunDetailResponse>, StackError> {
    let store = open_reconciled_deps_store(&state)?;
    let record = store
        .lookup_deps_apply_run(&apply_run_id)?
        .ok_or(StackError::DepsApplyRunNotFound { apply_run_id })?;
    deps_apply_run_detail(&store, record)
}

fn deps_apply_run_detail(
    store: &StateStore,
    record: DepsApplyRunRecord,
) -> std::result::Result<ApiSuccess<DepsApplyRunDetailResponse>, StackError> {
    let actions = store.query_installer_runs_for_apply_run(
        DEPS_APPLY_AGENT_ID,
        DEPS_APPLY_STEP,
        &record.id,
    )?;
    let now = chrono::Utc::now();
    let is_live = deps_run_liveness();
    Ok(ApiSuccess::new(DepsApplyRunDetailResponse {
        run: DepsApplyRunJson::from_record(record, &is_live),
        actions: actions
            .into_iter()
            .map(|action| InstallerRunJson::from_run(action, now))
            .collect(),
    }))
}
