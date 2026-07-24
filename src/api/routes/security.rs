use std::path::{Path, PathBuf};

use axum::extract::{Path as AxumPath, Query, State};
use chrono::{Duration, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

use super::super::core::AppState;
use crate::envelope::ApiSuccess;
use crate::error::StackError;
use crate::ownership::{self, PathKind, PathPosture};
use crate::security::PathInspectionIssue;
use crate::state::{
    NewSecurityFinding, NewSecurityRun, SecurityFindingRow, SecurityRunFilter, SecurityRunRecord,
};

const MAX_HISTORY_LIMIT: u32 = 500;
const DEFAULT_HISTORY_LIMIT: u32 = 20;

#[derive(Serialize)]
pub(crate) struct SecurityCheckResponse {
    pub(crate) run_id: String,
    pub(crate) status: String,
    pub(crate) ok: bool,
    pub(crate) findings: Vec<crate::security::SecurityFinding>,
    pub(crate) auth_failure_count: i64,
}

#[derive(Serialize)]
pub(crate) struct SecurityRunSummary {
    pub(crate) id: String,
    pub(crate) started_at: String,
    pub(crate) finished_at: String,
    pub(crate) status: String,
    pub(crate) ok: bool,
    pub(crate) critical_count: i64,
    pub(crate) warning_count: i64,
    pub(crate) auth_failure_count: i64,
}

impl From<SecurityRunRecord> for SecurityRunSummary {
    fn from(record: SecurityRunRecord) -> Self {
        Self {
            id: record.id,
            started_at: record.started_at,
            finished_at: record.finished_at,
            status: record.status,
            ok: record.ok,
            critical_count: record.critical_count,
            warning_count: record.warning_count,
            auth_failure_count: record.auth_failure_count,
        }
    }
}

#[derive(Serialize)]
pub(crate) struct SecurityHistoryResponse {
    pub(crate) runs: Vec<SecurityRunSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) next_cursor: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct SecurityHistoryShowResponse {
    pub(crate) run: SecurityRunSummary,
    pub(crate) findings: Vec<crate::security::SecurityFinding>,
}

#[derive(Deserialize, Default)]
pub(crate) struct SecurityHistoryQuery {
    pub(crate) limit: Option<u32>,
    pub(crate) after: Option<String>,
    pub(crate) since: Option<String>,
    pub(crate) until: Option<String>,
}

pub(crate) async fn security_check_handler(
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<SecurityCheckResponse>, StackError> {
    let started_at = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
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
    let dependency_report = crate::runtime::dependencies::deps::check_dependencies(&state.config);
    let dependency_failures = crate::security::dependency_security_failures(&dependency_report);
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
    let railway_platform = railway_platform_detected();
    let inputs_snapshot = redacted_inputs_snapshot(
        state.effective_bind.as_str(),
        &state.config.security.http,
        recent_auth_failures,
        sink_open_failures,
        process_euid,
        runtime_user_uid,
        runtime_user_name,
        workspace_writable,
        railway_platform,
        state.config.edge.cloudflare.is_some(),
        cloudflared_available(),
        recent_origin_counts.direct,
        recent_origin_counts.missing_headers,
        dependency_failures.len(),
    );
    let sandbox = &state.config.workspace.sandbox;
    let network_provider = crate::extensions::resolve_network_provider(&state.config);
    let sandbox_unavailable_reason = if sandbox.mode != crate::config::SandboxMode::Off {
        crate::runtime::sandbox::preflight(sandbox, network_provider.as_ref()).err()
    } else {
        None
    };
    let sandbox_off_but_capable = sandbox.mode == crate::config::SandboxMode::Off
        && crate::runtime::sandbox::host_supports_unshare();
    let findings = crate::security::check(crate::security::SecurityCheckInputs {
        effective_bind: state.effective_bind.as_str(),
        http: &state.config.security.http,
        recent_auth_failures,
        sink_open_failures,
        sink_last_error: sink_last_error.as_deref(),
        path_postures: &path_postures,
        path_issues: &path_issues,
        process_euid,
        runtime_user_uid,
        runtime_user_name,
        workspace_writable,
        workspace_root: state.config.workspace.root.as_str(),
        railway_platform,
        cloudflare: state.config.edge.cloudflare.as_ref(),
        cloudflared_available: cloudflared_available(),
        recent_direct_cloudflare_mode_requests: recent_origin_counts.direct,
        recent_missing_cloudflare_header_requests: recent_origin_counts.missing_headers,
        dependency_failures: &dependency_failures,
        sandbox_mode: sandbox.mode,
        sandbox_unavailable_reason,
        sandbox_off_but_capable,
    });

    // Serialize each finding's details payload once so we can hand the
    // store borrowed string slices that live for the duration of the call.
    let details_strings: Vec<Option<String>> = findings
        .iter()
        .map(|f| {
            f.details
                .as_ref()
                .map(|value| serde_json::to_string(value).expect("details payload is JSON-safe"))
        })
        .collect();
    let new_findings: Vec<NewSecurityFinding<'_>> = findings
        .iter()
        .zip(details_strings.iter())
        .map(|(finding, details_json)| NewSecurityFinding {
            code: finding.code.as_str(),
            severity: finding.severity.as_str(),
            message: finding.message.as_str(),
            details_json: details_json.as_deref(),
            remediation: finding.remediation.as_deref(),
        })
        .collect();

    let store = state.state.lock().await;
    let record = store.record_security_run(NewSecurityRun {
        started_at: started_at.as_str(),
        auth_failure_count: counts.auth_failures,
        inputs_json: inputs_snapshot.as_str(),
        findings: &new_findings,
    })?;
    drop(store);

    Ok(ApiSuccess::new(SecurityCheckResponse {
        run_id: record.id,
        status: record.status,
        ok: record.ok,
        findings,
        auth_failure_count: counts.auth_failures,
    }))
}

pub(crate) async fn security_history_handler(
    State(state): State<AppState>,
    Query(query): Query<SecurityHistoryQuery>,
) -> std::result::Result<ApiSuccess<SecurityHistoryResponse>, StackError> {
    let limit = query
        .limit
        .unwrap_or(DEFAULT_HISTORY_LIMIT)
        .clamp(1, MAX_HISTORY_LIMIT);
    let store = state.state.lock().await;
    let runs = store.query_security_runs(SecurityRunFilter {
        limit,
        after_id: query.after.as_deref(),
        since: query.since.as_deref(),
        until: query.until.as_deref(),
    })?;
    drop(store);

    let next_cursor = if runs.len() as u32 == limit {
        runs.last().map(|run| run.id.clone())
    } else {
        None
    };
    Ok(ApiSuccess::new(SecurityHistoryResponse {
        runs: runs.into_iter().map(SecurityRunSummary::from).collect(),
        next_cursor,
    }))
}

pub(crate) async fn security_history_show_handler(
    State(state): State<AppState>,
    AxumPath(run_id): AxumPath<String>,
) -> std::result::Result<ApiSuccess<SecurityHistoryShowResponse>, StackError> {
    let store = state.state.lock().await;
    let record = store
        .get_security_run(&run_id)?
        .ok_or_else(|| StackError::SecurityRunNotFound { id: run_id.clone() })?;
    let rows = store.get_findings_for_run(&record.id)?;
    drop(store);

    let findings = rows
        .into_iter()
        .map(security_finding_from_row)
        .collect::<Result<Vec<_>, StackError>>()?;
    Ok(ApiSuccess::new(SecurityHistoryShowResponse {
        run: SecurityRunSummary::from(record),
        findings,
    }))
}

fn security_finding_from_row(
    row: SecurityFindingRow,
) -> std::result::Result<crate::security::SecurityFinding, StackError> {
    let details =
        match row.details_json {
            Some(json) => Some(serde_json::from_str::<serde_json::Value>(&json).map_err(
                |source| StackError::SecurityFindingDetailsCorrupt {
                    run_id: row.run_id.clone(),
                    ordinal: row.ordinal,
                    source,
                },
            )?),
            None => None,
        };
    Ok(crate::security::SecurityFinding {
        code: row.code,
        severity: row.severity,
        message: row.message,
        details,
        remediation: row.remediation,
    })
}

/// Capture the set of facts the self-check ran against, with no key material
/// or secret content. This lands in `security_runs.inputs_json` so a historical
/// run can be reinterpreted after the runtime's effective config changes.
#[allow(clippy::too_many_arguments)]
fn redacted_inputs_snapshot(
    effective_bind: &str,
    http: &crate::config::SecurityHttpConfig,
    recent_auth_failures: i64,
    sink_open_failures: i64,
    process_euid: u32,
    runtime_user_uid: Option<u32>,
    runtime_user_name: &str,
    workspace_writable: bool,
    railway_platform: bool,
    cloudflare_configured: bool,
    cloudflared_available: bool,
    recent_direct_cloudflare_mode_requests: i64,
    recent_missing_cloudflare_header_requests: i64,
    dependency_failure_count: usize,
) -> String {
    let snapshot = serde_json::json!({
        "effective_bind": effective_bind,
        "recent_auth_failures": recent_auth_failures,
        "sink_open_failures": sink_open_failures,
        "process_euid": process_euid,
        "runtime_user_uid": runtime_user_uid,
        "runtime_user_name": runtime_user_name,
        "workspace_writable": workspace_writable,
        "railway_platform": railway_platform,
        "cloudflare_configured": cloudflare_configured,
        "cloudflared_available": cloudflared_available,
        "recent_direct_cloudflare_mode_requests": recent_direct_cloudflare_mode_requests,
        "recent_missing_cloudflare_header_requests": recent_missing_cloudflare_header_requests,
        "dependency_failure_count": dependency_failure_count,
        "http": {
            "trust_proxy_headers": http.trust_proxy_headers,
            "trusted_proxies_count": http.trusted_proxies.len(),
            "allowed_origins_count": http.allowed_origins.len(),
            "wildcard_origin": http.allowed_origins.iter().any(|o| o == "*"),
        },
    });
    serde_json::to_string(&snapshot).expect("snapshot is JSON-safe")
}

fn railway_platform_detected() -> bool {
    const RAILWAY_MARKERS: [&str; 3] = [
        "RAILWAY_PROJECT_ID",
        "RAILWAY_ENVIRONMENT_ID",
        "RAILWAY_SERVICE_ID",
    ];

    RAILWAY_MARKERS.iter().all(|name| {
        std::env::var_os(name)
            .map(|value| !value.as_os_str().is_empty())
            .unwrap_or(false)
    })
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
