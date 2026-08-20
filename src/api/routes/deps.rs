use axum::Json;
use axum::extract::State;
use serde::Deserialize;

use super::super::core::AppState;
use crate::envelope::ApiSuccess;
use crate::error::StackError;
use crate::runtime::dependencies::deps_apply::{
    DepsApplyReport, apply_dependencies, candidate_summary_line, candidates_for,
};
use crate::state::StateStore;

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

    // The runner spawns subprocesses (potentially long-running install
    // commands), so park it on a blocking thread and let the async
    // runtime keep handling other requests.
    //
    // Two distinct locks, deliberately. `deps_apply_lock` carries the
    // "back-to-back applies queue, not interleave" guarantee. The daemon's
    // shared StateStore mutex is taken only for `migrate()` and released
    // before any install snippet runs: held across the apply it would park
    // the whole HTTP surface, because the `api.request` audit middleware
    // takes that same mutex after every response — even handlers that touch
    // no state would stop answering for the length of the install.
    //
    // The per-action `installer_runs` rows are written through a second
    // short-lived connection instead. Each of those writes is a single
    // statement, and WAL plus the store's busy timeout let it wait for the
    // daemon's connection rather than fail; `acps deps apply` already
    // records its audit rows from its own connection while the daemon is
    // live, as does the agent installer's reconnecting progress sink.
    let apply_guard = state.deps_apply_lock.clone().lock_owned().await;
    let config = state.config.clone();
    let feature = payload.feature.clone();
    let store_handle = state.state.clone();
    let report = tokio::task::spawn_blocking(
        move || -> std::result::Result<DepsApplyReport, StackError> {
            // The guard travels into the blocking task rather than staying
            // with the handler future: a client disconnect drops the future
            // while the apply keeps running here, and a guard released at
            // that point would let the next apply interleave with this one.
            let _apply_guard = apply_guard;
            // Migrate under the shared handle — it is fast, must not race
            // concurrent daemon writes, and the schema has to be current
            // before the first audit row is written (an install snippet that
            // ran without a recorded row would break the "side effects always
            // audited" guarantee).
            let state_path = {
                let store = store_handle.blocking_lock();
                store.migrate()?;
                store.path().to_path_buf()
            };
            let apply_store = StateStore::open(&state_path)?;
            let shell = &config.workspace.default_shell;
            apply_dependencies(&config, feature.as_deref(), Some(&apply_store), shell)
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
