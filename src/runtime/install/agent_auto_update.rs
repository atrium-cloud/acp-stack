use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex as TokioMutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::{Config, parse_duration_string};
use crate::runtime::agent::supervisor::AgentSupervisor;
use crate::runtime::install::agent_updater::{AgentUpdateOptions, run_managed_agent_update};
use crate::state::StateStore;

const DISABLED_POLL_INTERVAL: Duration = Duration::from_secs(60);
/// Grace window for `shutdown` to wait on an in-flight update before detaching.
/// An update runs inside `spawn_blocking` and cannot be cancelled, so without
/// this bound a SIGTERM mid-update would hold daemon shutdown hostage.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

pub struct AgentAutoUpdater {
    handle: Option<JoinHandle<()>>,
    cancel: CancellationToken,
}

impl AgentAutoUpdater {
    pub fn spawn(
        home: PathBuf,
        config_path: PathBuf,
        state_path: PathBuf,
        state: Arc<TokioMutex<StateStore>>,
        agent_supervisor: Arc<AgentSupervisor>,
    ) -> Self {
        let cancel = CancellationToken::new();
        let cancel_inner = cancel.clone();
        let handle = tokio::spawn(async move {
            loop {
                let delay = next_delay(&config_path);
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = cancel_inner.cancelled() => return,
                }
                let config = match Config::load_from_path(&config_path) {
                    Ok(config) => config,
                    Err(err) => {
                        tracing::warn!(error = %err, "agent auto-update: failed to load config");
                        continue;
                    }
                };
                if !config
                    .agent
                    .auto_update
                    .as_ref()
                    .is_some_and(|auto_update| auto_update.enabled)
                {
                    continue;
                }
                // The auto-updater only updates an already-stopped agent and
                // never stops a running one; a live agent is skipped every cycle.
                if !agent_supervisor.try_begin_update().await {
                    if let Err(err) = append_update_lifecycle(
                        &state,
                        "agent.update.skipped",
                        "agent update skipped",
                        serde_json::json!({
                            "agent_id": config.agent.id,
                            "reason": "agent is running",
                        }),
                    )
                    .await
                    {
                        tracing::warn!(error = %err, "agent auto-update: failed to record skip");
                    }
                    continue;
                }
                if let Err(err) = append_update_lifecycle(
                    &state,
                    "agent.update.started",
                    "agent update started",
                    serde_json::json!({ "agent_id": config.agent.id }),
                )
                .await
                {
                    tracing::warn!(error = %err, "agent auto-update: failed to record start");
                }
                let home_for_task = home.clone();
                let state_path_for_task = state_path.clone();
                let result = tokio::task::spawn_blocking(move || {
                    run_managed_agent_update(
                        home_for_task,
                        state_path_for_task,
                        config,
                        AgentUpdateOptions::default(),
                    )
                })
                .await;
                agent_supervisor.finish_update().await;
                match result {
                    Ok(Ok(report)) => {
                        let (event_kind, event_message) = if report.skipped {
                            ("agent.update.skipped", "agent update skipped")
                        } else if report.has_failed_steps() {
                            ("agent.update.failed", "agent update failed")
                        } else {
                            ("agent.update.finished", "agent update finished")
                        };
                        if let Err(err) = append_update_lifecycle(
                            &state,
                            event_kind,
                            event_message,
                            serde_json::to_value(&report).unwrap_or_else(
                                |_| serde_json::json!({ "agent_id": report.agent_id }),
                            ),
                        )
                        .await
                        {
                            tracing::warn!(
                                error = %err,
                                "agent auto-update: failed to record finish"
                            );
                        }
                    }
                    Ok(Err(err)) => {
                        if let Err(record_err) = append_update_lifecycle(
                            &state,
                            "agent.update.failed",
                            "agent update failed",
                            serde_json::json!({ "error": err.to_string() }),
                        )
                        .await
                        {
                            tracing::warn!(
                                error = %record_err,
                                "agent auto-update: failed to record failure"
                            );
                        }
                    }
                    Err(err) => {
                        // A panicked task must still pair its earlier
                        // `agent.update.started` row with a terminal failure.
                        if let Err(record_err) = append_update_lifecycle(
                            &state,
                            "agent.update.failed",
                            "agent update failed",
                            serde_json::json!({ "error": err.to_string() }),
                        )
                        .await
                        {
                            tracing::warn!(
                                error = %record_err,
                                "agent auto-update: failed to record join failure"
                            );
                        }
                        tracing::warn!(error = %err, "agent auto-update task join failed");
                    }
                }
            }
        });
        Self {
            handle: Some(handle),
            cancel,
        }
    }

    pub async fn shutdown(mut self) {
        self.cancel.cancel();
        let Some(handle) = self.handle.take() else {
            return;
        };
        // Cancellation is only observed at the top `select!`, so bound the wait
        // and detach; the blocking work is reaped when the process exits.
        match tokio::time::timeout(SHUTDOWN_GRACE, handle).await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                tracing::warn!(error = ?err, "agent auto-update task did not exit cleanly");
            }
            Err(_) => {
                tracing::warn!("agent auto-update still running at shutdown; abandoning the wait");
            }
        }
    }
}

impl Drop for AgentAutoUpdater {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

fn next_delay(config_path: &PathBuf) -> Duration {
    let Ok(config) = Config::load_from_path(config_path) else {
        return DISABLED_POLL_INTERVAL;
    };
    let Some(auto_update) = config.agent.auto_update.as_ref() else {
        return DISABLED_POLL_INTERVAL;
    };
    if !auto_update.enabled {
        return DISABLED_POLL_INTERVAL;
    }
    parse_duration_string(&auto_update.frequency).unwrap_or(DISABLED_POLL_INTERVAL)
}

async fn append_update_lifecycle(
    state: &Arc<TokioMutex<StateStore>>,
    kind: &str,
    message: &str,
    payload: serde_json::Value,
) -> crate::error::Result<()> {
    let payload = payload.to_string();
    let guard = state.lock().await;
    guard.append_agent_lifecycle(kind, message, &payload)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::agent::supervisor::AgentSupervisor;

    // Loading goes through full validation, so every required section is here.
    const BASE_CONFIG: &str = r#"
[api]
bind = "127.0.0.1:7700"
public_url = "http://127.0.0.1:7700"
max_request_bytes = 1048576

[security.http]
max_request_bytes = 1048576
rate_limit_per_minute = 120
burst = 30
auth_failures_per_minute = 5
auth_block_duration = "15m"
allowed_origins = []
trust_proxy_headers = false
trusted_proxies = []

[workspace]
root = "/workspace"
uploads = "/workspace/uploads"
default_shell = "/bin/bash"
runtime_user = "acp"
max_file_bytes = 8388608

[logging]
level = "info"
local_retention_days = 30

[agent]
id = "placebo"
name = "Placebo"
command = "placebo-agent"
args = []
cwd = "/workspace"
env = []
restart = "on-crash"
"#;

    fn write_config(tempdir: &tempfile::TempDir, extra: &str) -> PathBuf {
        let path = tempdir.path().join("acps-config.toml");
        std::fs::write(&path, format!("{BASE_CONFIG}{extra}")).expect("write config");
        path
    }

    #[test]
    fn next_delay_uses_configured_frequency_when_enabled() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = write_config(
            &tempdir,
            "\n[agent.auto_update]\nenabled = true\nfrequency = \"2w\"\n",
        );
        let expected = parse_duration_string("2w").expect("2w parses");
        assert_eq!(next_delay(&path), expected);
        assert_ne!(next_delay(&path), DISABLED_POLL_INTERVAL);
    }

    #[test]
    fn next_delay_falls_back_to_poll_when_disabled() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = write_config(
            &tempdir,
            "\n[agent.auto_update]\nenabled = false\nfrequency = \"1d\"\n",
        );
        assert_eq!(next_delay(&path), DISABLED_POLL_INTERVAL);
    }

    #[test]
    fn next_delay_falls_back_to_poll_when_section_absent() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = write_config(&tempdir, "");
        assert_eq!(next_delay(&path), DISABLED_POLL_INTERVAL);
    }

    #[test]
    fn next_delay_falls_back_to_poll_when_config_missing() {
        let path = PathBuf::from("/nonexistent/acp-stack/acps-config.toml");
        assert_eq!(next_delay(&path), DISABLED_POLL_INTERVAL);
    }

    #[tokio::test]
    async fn spawn_then_shutdown_returns_promptly() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        // Disabled auto-update parks the loop on the 60s poll interval, which
        // shutdown must cancel rather than wait out.
        let config_path = write_config(
            &tempdir,
            "\n[agent.auto_update]\nenabled = false\nfrequency = \"1d\"\n",
        );
        let state_path = tempdir.path().join("state.sqlite");
        let store = StateStore::open(&state_path).expect("state open");
        store.migrate().expect("migrate");
        let state = Arc::new(TokioMutex::new(store));
        let supervisor = Arc::new(AgentSupervisor::new());

        let updater = AgentAutoUpdater::spawn(
            tempdir.path().to_path_buf(),
            config_path,
            state_path,
            state,
            supervisor,
        );

        tokio::time::timeout(Duration::from_secs(10), updater.shutdown())
            .await
            .expect("shutdown should not hang");
    }
}
