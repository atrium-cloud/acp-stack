use std::path::{Path, PathBuf};

use axum::extract::State;
use chrono::{Duration, SecondsFormat, Utc};
use serde::Serialize;

use super::super::core::AppState;
use crate::envelope::ApiSuccess;
use crate::error::StackError;
use crate::ownership::{self, PathKind, PathPosture};
use crate::security::PathInspectionIssue;

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
    let recent_origin_counts = recent_cloudflare_origin_counts(&store)?;
    drop(store);
    let (path_postures, path_issues) = collect_path_inspections(
        &state.runtime_paths.config_path,
        &state.runtime_paths.state_path,
        &state.config.workspace.root,
    );
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
        path_issues: &path_issues,
        process_euid,
        runtime_user_uid,
        runtime_user_name,
        workspace_writable,
        workspace_root: state.config.workspace.root.as_str(),
        cloudflare: state.config.edge.cloudflare.as_ref(),
        cloudflared_available: cloudflared_available(),
        recent_direct_cloudflare_mode_requests: recent_origin_counts.direct,
        recent_missing_cloudflare_header_requests: recent_origin_counts.missing_headers,
    });
    Ok(ApiSuccess::new(SecurityCheckResponse {
        ok: findings.is_empty(),
        findings,
        auth_failure_count: counts.auth_failures,
    }))
}

#[derive(Default)]
struct RecentOriginCounts {
    direct: i64,
    missing_headers: i64,
}

fn recent_cloudflare_origin_counts(
    store: &crate::state::StateStore,
) -> std::result::Result<RecentOriginCounts, StackError> {
    let since = (Utc::now() - Duration::minutes(10)).to_rfc3339_opts(SecondsFormat::Nanos, true);
    let events = store.query_events(crate::state::LogFilter {
        limit: 500,
        since: Some(&since),
        ..Default::default()
    })?;
    let mut counts = RecentOriginCounts::default();
    for event in events {
        let Ok(payload) = serde_json::from_str::<serde_json::Value>(&event.payload_json) else {
            continue;
        };
        let Some(origin_kind) = payload_origin_kind(&payload) else {
            continue;
        };
        match origin_kind {
            "direct" => counts.direct += 1,
            "trusted_proxy_missing_cloudflare" => counts.missing_headers += 1,
            _ => {}
        }
    }
    Ok(counts)
}

fn payload_origin_kind(payload: &serde_json::Value) -> Option<&str> {
    payload
        .get("origin")
        .and_then(|origin| origin.get("origin_kind"))
        .or_else(|| {
            payload
                .get("request_origin")
                .and_then(|origin| origin.get("origin_kind"))
        })
        .and_then(serde_json::Value::as_str)
}

fn cloudflared_available() -> bool {
    let Some(path_env) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path_env).any(|dir| dir.join("cloudflared").is_file())
}

/// Inspect each runtime-managed path and return either a posture or a
/// structured inspection issue. Missing/unreadable paths are security findings:
/// a deleted state DB or unreadable age key should not make the report look
/// cleaner than the daemon's actual posture.
fn collect_path_inspections(
    config_path: &Path,
    state_path: &Path,
    workspace_root: &str,
) -> (Vec<PathPosture>, Vec<PathInspectionIssue>) {
    let mut postures = Vec::new();
    let mut issues = Vec::new();
    let config_dir = config_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let state_dir = state_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let age_key_path = config_dir.join("age.key");
    let secret_store_path = state_dir.join("secrets.age");

    let candidates: [(PathBuf, PathKind); 7] = [
        (config_dir, PathKind::ConfigDir),
        (config_path.to_path_buf(), PathKind::ConfigFile),
        (state_dir, PathKind::StateDir),
        (state_path.to_path_buf(), PathKind::StateDb),
        (age_key_path, PathKind::AgeKey),
        (secret_store_path, PathKind::SecretStore),
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
                    "security check could not inspect path"
                );
                issues.push(PathInspectionIssue {
                    path,
                    kind,
                    error: err.to_string(),
                });
            }
        }
    }
    (postures, issues)
}
