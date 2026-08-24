//! Per-subsystem runtime health report behind `/v1/health/*` and `acps status`.
//! Probe errors are never propagated: they become degraded rows in the report.

mod deps;
mod mcp;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::api::AppState;
use crate::config::{McpConfig, McpServerConfig};
use crate::error::Result;
use crate::ownership;
use crate::runtime::dependencies::deps::resolve_command_path;
use crate::runtime::dependencies::deps_apply::{DEPS_APPLY_AGENT_ID, DEPS_APPLY_STEP};
use crate::secrets::SecretStore;
use crate::state::{AgentStartedProcess, InstallerRun, StateStore};

use self::deps::collect_deps;
use self::mcp::{collect_mcp, mcp_secret_store_paths};

pub use self::deps::deps_cluster_has_failure_for_latest;

const SINK_FAILURE_FAIL_THRESHOLD: i64 = 1;

// `installer_runs.status` values written by `acps deps apply`, mirroring
// `deps_apply.rs::DepApplyOutcome::status_label`. Shared with `cli::status` so
// the CLI and the HTTP readiness signal cannot drift.
pub const DEPS_STATUS_FAILED: &str = "failed";
pub const DEPS_STATUS_PRIVILEGE_REQUIRED: &str = "privilege_required";

// Upper bound on rows scanned when reconstructing the most recent apply invocation.
pub const DEPS_RECENT_ROW_LIMIT: u32 = 50;

// Rows within this gap belong to the same apply invocation; it must exceed
// `deps_apply.rs::DEFAULT_TIMEOUT` (10 minutes) so a slow dep does not split a cluster.
pub const DEPS_CLUSTER_GAP_SECS: i64 = 15 * 60;

/// True when an `installer_runs.status` value represents a per-dep failure that
/// should promote `deps` to the failing list.
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
    pub mcp: McpHealth,
    pub prompts: PromptsHealth,
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
    /// `stopped` | `starting` | `running` | `stopping`, lower-cased so HTTP
    /// consumers see stable snake-case values.
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    pub orphaned_process_count: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub orphaned_process_pids: Vec<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub orphan_probe_error: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct AgentProcessProbe {
    started_processes: Vec<AgentStartedProcess>,
    probe_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SinkHealth {
    pub enabled: bool,
    pub open_failure_count: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_failure_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_error: Option<String>,
    /// Set when the sink-state queries themselves failed, promoting the sink to
    /// `failing` regardless of `open_failure_count`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probe_error: Option<String>,
}

/// Stuck-prompt signal: rows still `pending`/`running` whose `updated_at` is
/// older than the `[prompts]` staleness threshold.
#[derive(Debug, Clone, Serialize)]
pub struct PromptsHealth {
    pub stuck_count: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oldest_stuck_age_secs: Option<i64>,
    pub threshold_secs: i64,
    /// Set when the prompts probe query itself errored, promoting the
    /// subsystem to `failing` regardless of the count.
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
    /// `privilege_required`. The newest row alone is not representative: one
    /// apply writes one `installer_runs` row per dep, so the worst status in
    /// the cluster wins.
    pub cluster_has_failure: bool,
    /// Set when the `installer_runs` lookup itself errored, so a corrupt table
    /// cannot make the deps section look healthy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probe_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct McpHealth {
    pub configured_count: usize,
    pub failing_count: usize,
    pub servers: Vec<McpServerHealth>,
}

#[derive(Debug, Clone, Serialize)]
pub struct McpServerHealth {
    pub name: String,
    pub kind: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command_path: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub missing_secret_refs: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl HealthReport {
    /// Collect a fresh report. Every persistent signal is read under one state
    /// lock; the supervisor snapshot is taken after release so a slow query
    /// never blocks the bridge mutex.
    pub async fn collect(state: &AppState) -> Self {
        let sqlite;
        let sink;
        let deps;
        let prompts;
        let agent_process_probe;
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
            prompts = collect_prompts(&store, state.config.prompts.effective_stale_threshold());
            agent_process_probe = collect_agent_process_probe(&store);
        }
        let workspace = collect_workspace(&state.config.workspace.root);
        let agent = collect_agent(state, agent_process_probe).await;
        let mcp = collect_mcp(
            &state.config.mcp,
            &mcp_secret_store_paths(
                &state.runtime_paths.config_path,
                &state.runtime_paths.state_path,
            ),
        );

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
        if mcp.failing_count > 0 {
            failing.push("mcp".to_owned());
        }
        if prompts.probe_error.is_some() || prompts.stuck_count > 0 {
            failing.push("prompts".to_owned());
        }
        if agent.orphan_probe_error.is_some() || agent.orphaned_process_count > 0 {
            failing.push("agent".to_owned());
        }
        Self {
            ok: failing.is_empty(),
            failing,
            sqlite,
            workspace,
            agent,
            sink,
            deps,
            mcp,
            prompts,
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

async fn collect_agent(state: &AppState, process_probe: AgentProcessProbe) -> AgentHealth {
    let (id, supervisor) = match state.default_agent_target().await {
        Ok((config, target)) => (config.agent.id, target.supervisor),
        Err(err) => {
            tracing::warn!(error = %err, "failed to resolve default agent target for health report");
            (
                state.config.agent.id.clone(),
                state.agent_supervisor.clone(),
            )
        }
    };
    let snapshot = supervisor.snapshot().await;
    // Orphan detection must exclude ALL supervised pids, not just the primary's,
    // or every running secondary target is misreported as a leaked process.
    let supervised_pids = supervised_target_pids(state).await;
    let orphaned_process_pids = orphaned_agent_process_pids(&process_probe, &supervised_pids);
    AgentHealth {
        configured: !id.is_empty(),
        id,
        state: snapshot.state.as_wire_str().to_owned(),
        pid: snapshot.pid,
        orphaned_process_count: orphaned_process_pids.len(),
        orphaned_process_pids,
        orphan_probe_error: process_probe.probe_error,
    }
}

fn collect_agent_process_probe(store: &StateStore) -> AgentProcessProbe {
    match store.query_agent_started_processes() {
        Ok(started_processes) => AgentProcessProbe {
            started_processes,
            probe_error: None,
        },
        Err(err) => AgentProcessProbe {
            started_processes: Vec::new(),
            probe_error: Some(err.to_string()),
        },
    }
}

async fn supervised_target_pids(state: &AppState) -> BTreeSet<u32> {
    let mut pids = BTreeSet::new();
    for target in state.agent_targets.targets() {
        if let Some(pid) = target.supervisor.snapshot().await.pid {
            pids.insert(pid);
        }
    }
    pids
}

fn orphaned_agent_process_pids(
    process_probe: &AgentProcessProbe,
    supervised_pids: &BTreeSet<u32>,
) -> Vec<u32> {
    let mut pids = BTreeSet::new();
    for process in &process_probe.started_processes {
        if supervised_pids.contains(&process.pid) {
            continue;
        }
        if process_group_is_alive(process.pid) {
            pids.insert(process.pid);
        }
    }
    pids.into_iter().collect()
}

#[cfg(unix)]
fn process_group_is_alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // SAFETY: `kill` with signal 0 sends nothing; the negative pid probes the
    // process group whose id matches the supervised agent's spawn pid.
    let result = unsafe { libc::kill(-pid, 0) };
    if result == 0 {
        return true;
    }
    matches!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::EPERM)
    )
}

#[cfg(not(unix))]
fn process_group_is_alive(_pid: u32) -> bool {
    false
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

fn collect_prompts(store: &StateStore, threshold: std::time::Duration) -> PromptsHealth {
    let threshold_secs = i64::try_from(threshold.as_secs()).unwrap_or(i64::MAX);
    let (count, oldest_at) = match store.count_stuck_prompts(threshold) {
        Ok(pair) => pair,
        Err(err) => {
            return PromptsHealth {
                stuck_count: 0,
                oldest_stuck_age_secs: None,
                threshold_secs,
                probe_error: Some(err.to_string()),
            };
        }
    };
    let oldest_stuck_age_secs = oldest_at.as_deref().and_then(prompt_age_seconds);
    PromptsHealth {
        stuck_count: count,
        oldest_stuck_age_secs,
        threshold_secs,
        probe_error: None,
    }
}

/// RFC3339 timestamp as "seconds since now", clamped to >= 0 so database-side
/// clock skew cannot surface as a negative age.
fn prompt_age_seconds(raw: &str) -> Option<i64> {
    let parsed = chrono::DateTime::parse_from_rfc3339(raw).ok()?;
    let age = chrono::Utc::now().signed_duration_since(parsed.with_timezone(&chrono::Utc));
    Some(age.num_seconds().max(0))
}

#[cfg(test)]
mod tests;
