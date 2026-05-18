use std::path::{Path, PathBuf};

use axum::extract::State;
use chrono::{Duration, SecondsFormat, Utc};
use serde::Serialize;

use super::super::core::AppState;
use crate::envelope::ApiSuccess;
use crate::error::StackError;
use crate::ownership::{self, PathKind, PathPosture};

#[derive(Serialize)]
pub(crate) struct SecurityCheckResponse {
    ok: bool,
    findings: Vec<crate::security::SecurityFinding>,
    auth_failure_count: i64,
}

pub(crate) async fn security_check_handler(
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<SecurityCheckResponse>, StackError> {
    let store = state.state.lock().await;
    let counts = store.counts()?;
    let recent_cutoff =
        (Utc::now() - Duration::minutes(1)).to_rfc3339_opts(SecondsFormat::Nanos, true);
    let recent_auth_failures = store.count_auth_failures_since(&recent_cutoff)?;
    let sink_open_failures = store.sink_open_failure_count()?;
    let sink_last_error = store
        .latest_sink_failure_summary()?
        .and_then(|(_window, _count, last_error, _observed)| last_error);
    drop(store);
    let path_postures = collect_path_postures(&state.config.workspace.root);
    let process_euid = ownership::process_euid();
    let runtime_user_name = state.config.workspace.runtime_user.as_str();
    let runtime_user_uid = ownership::resolve_runtime_user_uid(runtime_user_name)
        .ok()
        .flatten();
    let workspace_writable = ownership::workspace_writable(Path::new(&state.config.workspace.root));
    let findings = crate::security::check(crate::security::SecurityCheckInputs {
        effective_bind: state.effective_bind.as_str(),
        http: &state.config.security.http,
        session_key_empty: state.session_key.is_empty(),
        admin_key_empty: state.admin_key.is_empty(),
        recent_auth_failures,
        sink_open_failures,
        sink_last_error: sink_last_error.as_deref(),
        session_key_value: Some(state.session_key.as_str()),
        admin_key_value: Some(state.admin_key.as_str()),
        path_postures: &path_postures,
        process_euid,
        runtime_user_uid,
        runtime_user_name,
        workspace_writable,
        workspace_root: state.config.workspace.root.as_str(),
    });
    Ok(ApiSuccess::new(SecurityCheckResponse {
        ok: findings.is_empty(),
        findings,
        auth_failure_count: counts.auth_failures,
    }))
}

/// Inspect each runtime-managed path and return the postures the security
/// check should evaluate. Skips paths that fail to stat with a tracing warn —
/// a missing or unreadable path produces zero findings (rather than failing
/// the whole self-check), which matches the "best-effort posture report"
/// shape of `acps security check`.
fn collect_path_postures(workspace_root: &str) -> Vec<PathPosture> {
    let mut postures = Vec::new();
    let home = match crate::fs_util::home_dir() {
        Ok(home) => home,
        Err(err) => {
            tracing::warn!(error = %err, "security check skipped path postures: HOME not set");
            return postures;
        }
    };

    let candidates: [(PathBuf, PathKind); 5] = [
        (config_dir(&home), PathKind::ConfigDir),
        (state_dir(&home), PathKind::StateDir),
        (crate::secrets::age_key_path(&home), PathKind::AgeKey),
        (
            crate::secrets::secret_store_path(&home),
            PathKind::SecretStore,
        ),
        (PathBuf::from(workspace_root), PathKind::WorkspaceRoot),
    ];

    for (path, kind) in candidates {
        match ownership::inspect(&path, kind) {
            Ok(posture) => postures.push(posture),
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    path = %path.display(),
                    kind = ?kind,
                    "security check could not inspect path; finding will be omitted"
                );
            }
        }
    }
    postures
}

fn config_dir(home: &Path) -> PathBuf {
    home.join(".config").join("acp-stack")
}

fn state_dir(home: &Path) -> PathBuf {
    home.join(".local").join("share").join("acp-stack")
}
