use axum::Json;
use axum::extract::State;
use serde::Deserialize;

use super::super::core::AppState;
use crate::envelope::ApiSuccess;
use crate::error::StackError;
use crate::runtime::dependencies::deps_apply::{
    DepsApplyReport, apply_dependencies, candidate_summary_line, candidates_for,
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

#[derive(Debug, Default, Deserialize)]
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

#[derive(serde::Serialize)]
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
/// is far beyond what a session-tier caller should have. The CLI route
/// (`acps deps apply`) uses the same runner.
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

    // The runner spawns subprocesses (potentially long-running install
    // commands), so park it on a blocking thread and let the async
    // runtime keep handling other requests. We hold the daemon's
    // existing StateStore mutex for the entire apply call so the
    // per-action `installer_runs` rows can't race with concurrent
    // daemon writes (opening a second SQLite connection here would
    // hit "database is locked" intermittently, breaking the "side
    // effects always audited" guarantee).
    let config = state.config.clone();
    let feature = payload.feature.clone();
    let store_handle = state.state.clone();
    let report = tokio::task::spawn_blocking(
        move || -> std::result::Result<DepsApplyReport, StackError> {
            // `blocking_lock` is the right call inside spawn_blocking;
            // the async runtime is free to schedule other tasks while
            // this thread waits. Holding it across the apply
            // serializes deps_apply with every other state-writing
            // handler, which is the correct semantics — back-to-back
            // applies should queue, not interleave.
            let store = store_handle.blocking_lock();
            store.migrate()?;
            let shell = &config.workspace.default_shell;
            apply_dependencies(&config, feature.as_deref(), Some(&store), shell)
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
