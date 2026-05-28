use std::path::Path;
use std::time::Duration;

use crate::config::{self, Config};
use crate::error::Result;
use crate::fs_util::{
    create_dir_owner_only, home_dir, parent_dir, pre_create_owner_only, set_owner_only_dir,
    set_owner_only_file,
};
use crate::ownership;
use crate::runtime::dependencies::deps_apply::{DEPS_APPLY_AGENT_ID, DEPS_APPLY_STEP};
use crate::runtime::health::{
    DEPS_RECENT_ROW_LIMIT, deps_cluster_has_failure_for_latest, deps_status_is_failure,
};
use crate::state::{StateStore, default_state_path};

use super::core::{CliKey, OutputFormat, daemon_base_url, open_cli_key, print_json};

// `acps status` should not hang behind a dead listener or half-open tunnel.
// Other daemon-facing commands can wait for their operation; status is a
// diagnostic surface, so keep the live probe bounded and report unavailable.
const STATUS_DAEMON_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

pub(super) fn run_status(output: OutputFormat) -> Result<()> {
    let home = home_dir()?;
    let config_path = config::default_config_path()?;
    let config_dir = parent_dir(&config_path)?;
    if config_dir.exists() {
        set_owner_only_dir(config_dir)?;
    }
    if config_path.exists() {
        set_owner_only_file(&config_path)?;
    }
    let config = Config::load_from_path(&config_path)?;

    let state_path = default_state_path(&home);
    let state_dir = parent_dir(&state_path)?;
    create_dir_owner_only(state_dir)?;
    pre_create_owner_only(&state_path)?;
    let store = StateStore::open(&state_path)?;
    store.migrate()?;
    set_owner_only_file(&state_path)?;
    store.append_event_with_source(
        "info",
        "status.checked",
        crate::state::EVENT_SOURCE_CLI,
        "status checked",
        "{}",
    )?;

    let schema_version = store.schema_version()?;
    let latest_event = store
        .latest_event_timestamp()?
        .unwrap_or_else(|| "none".to_owned());

    let workspace_root = config.workspace.root.as_str();
    let agent_id = config.agent.id.as_str();

    if output.is_json() {
        print_json(&serde_json::json!({
            "config": {
                "ok": true,
                "path": config_path.display().to_string(),
            },
            "state": {
                "ok": true,
                "path": state_path.display().to_string(),
                "schema_version": schema_version,
                "latest_event": latest_event,
            },
            "workspace": {
                "ok": ownership::workspace_writable(Path::new(workspace_root)),
                "root": workspace_root,
            },
            "agent": {
                "configured": !agent_id.is_empty(),
                "id": agent_id,
            },
            "sink": sink_status_json(&store, &config)?,
            "deps": deps_status_json(&store)?,
            "prompts": prompts_status_json(&store, &config)?,
            "daemon": daemon_status_json(&config, &home),
        }))?;
        return Ok(());
    }

    println!("config:    ok ({})", config_path.display());
    println!(
        "state:     ok ({}, schema={schema_version}, latest_event={latest_event})",
        state_path.display()
    );

    if ownership::workspace_writable(Path::new(workspace_root)) {
        println!("workspace: ok ({workspace_root})");
    } else {
        println!("workspace: not writable ({workspace_root})");
    }

    if agent_id.is_empty() {
        println!("agent:     not configured");
    } else {
        println!("agent:     configured: {agent_id}");
    }

    if let Some(supabase) = config.logging.supabase.as_ref() {
        if supabase.enabled {
            print_sink_status(&store)?;
        } else {
            println!("sink:      supabase disabled");
        }
    } else {
        println!("sink:      not configured");
    }

    print_deps_status(&store)?;
    print_prompts_status(&store, &config)?;
    print_daemon_status(&config, &home);

    Ok(())
}

fn prompts_status_json(store: &StateStore, config: &Config) -> Result<serde_json::Value> {
    let threshold = config.prompts.effective_stale_threshold();
    let (count, oldest_at) = store.count_stuck_prompts(threshold)?;
    Ok(serde_json::json!({
        "ok": count == 0,
        "stuck_count": count,
        "threshold_secs": threshold.as_secs(),
        "oldest_at": oldest_at,
        "oldest_age_secs": oldest_at.as_deref().and_then(prompts_age_seconds),
    }))
}

// Mirror `runtime/health.rs::collect_prompts` so the CLI surface stays in
// step with `/v1/health/ready`. The threshold is read from the operator's
// `[prompts]` block (or its defaults) so a deployment that tunes the
// sweeper sees the same window reflected here.
fn print_prompts_status(store: &StateStore, config: &Config) -> Result<()> {
    let threshold = config.prompts.effective_stale_threshold();
    let threshold_secs = threshold.as_secs();
    let (count, oldest_at) = store.count_stuck_prompts(threshold)?;
    if count == 0 {
        println!("prompts:   ok (threshold {threshold_secs}s)");
        return Ok(());
    }
    let age_suffix = oldest_at
        .as_deref()
        .and_then(prompts_age_seconds)
        .map(|age| format!(", oldest {age}s"))
        .unwrap_or_default();
    println!("prompts:   {count} stuck (threshold {threshold_secs}s{age_suffix})");
    Ok(())
}

fn prompts_age_seconds(raw: &str) -> Option<i64> {
    let parsed = chrono::DateTime::parse_from_rfc3339(raw).ok()?;
    let age = chrono::Utc::now().signed_duration_since(parsed.with_timezone(&chrono::Utc));
    Some(age.num_seconds().max(0))
}

fn print_sink_status(store: &StateStore) -> Result<()> {
    let open = store.sink_open_failure_count()?;
    if open == 0 {
        println!("sink:      ok (supabase)");
        return Ok(());
    }
    match store.latest_sink_failure_summary()? {
        Some((_window_started_at, _count, last_error, observed_at)) => {
            let detail = last_error
                .as_deref()
                .map(|err| format!(", last error: {err}"))
                .unwrap_or_default();
            println!(
                "sink:      {open} open failures (supabase, last observed {observed_at}{detail})"
            );
        }
        None => {
            println!("sink:      {open} open failures (supabase)");
        }
    }
    Ok(())
}

fn sink_status_json(store: &StateStore, config: &Config) -> Result<serde_json::Value> {
    let Some(supabase) = config.logging.supabase.as_ref() else {
        return Ok(serde_json::json!({ "configured": false, "enabled": false }));
    };
    if !supabase.enabled {
        return Ok(serde_json::json!({
            "configured": true,
            "enabled": false,
            "kind": "supabase",
        }));
    }
    let open = store.sink_open_failure_count()?;
    let latest = store.latest_sink_failure_summary()?;
    Ok(serde_json::json!({
        "configured": true,
        "enabled": true,
        "kind": "supabase",
        "ok": open == 0,
        "open_failure_count": open,
        "latest_failure": latest.map(|(window_started_at, count, last_error, observed_at)| {
            serde_json::json!({
                "window_started_at": window_started_at,
                "count": count,
                "last_error": last_error,
                "observed_at": observed_at,
            })
        }),
    }))
}

// Mirror the lookup used in `runtime/health.rs::collect_deps`: new
// `acps deps apply` rows share an exact apply_run_id, while legacy rows fall
// back to the old timestamp cluster. The CLI surfaces a one-line summary of
// the most-recent row plus a hint when the surrounding cluster had failures.
fn print_deps_status(store: &StateStore) -> Result<()> {
    let rows: Vec<_> = store
        .query_installer_runs_filtered(Some(DEPS_APPLY_AGENT_ID), DEPS_RECENT_ROW_LIMIT)?
        .into_iter()
        // Cross-check `step` so a config with `agent.id = "deps_apply"` does
        // not pollute the deps signal with agent installer rows.
        .filter(|row| row.step == DEPS_APPLY_STEP)
        .collect();
    let mut iter = rows.into_iter();
    let Some(latest) = iter.next() else {
        println!("deps:      no apply runs");
        return Ok(());
    };
    let cluster_has_failure = deps_cluster_has_failure_for_latest(store, &latest, iter)?;
    let exit = latest
        .exit_status
        .map(|code| format!(", exit={code}"))
        .unwrap_or_default();
    let suffix = if cluster_has_failure && !deps_status_is_failure(&latest.status) {
        // Most recent row looks fine but an older row in the same apply
        // cluster failed — surface that so the operator does not interpret
        // the latest-row status as the whole apply being healthy.
        ", recent cluster had failures"
    } else {
        ""
    };
    println!(
        "deps:      last apply {} ({}{exit}{suffix})",
        latest.started_at, latest.status
    );
    Ok(())
}

fn deps_status_json(store: &StateStore) -> Result<serde_json::Value> {
    let rows: Vec<_> = store
        .query_installer_runs_filtered(Some(DEPS_APPLY_AGENT_ID), DEPS_RECENT_ROW_LIMIT)?
        .into_iter()
        .filter(|row| row.step == DEPS_APPLY_STEP)
        .collect();
    let mut iter = rows.into_iter();
    let Some(latest) = iter.next() else {
        return Ok(serde_json::json!({
            "has_apply_runs": false,
        }));
    };
    let cluster_has_failure = deps_cluster_has_failure_for_latest(store, &latest, iter)?;
    let ok = !deps_status_is_failure(&latest.status) && !cluster_has_failure;
    Ok(serde_json::json!({
        "has_apply_runs": true,
        "latest": {
            "id": latest.id,
            "agent_id": latest.agent_id,
            "started_at": latest.started_at,
            "finished_at": latest.finished_at,
            "status": latest.status,
            "exit_status": latest.exit_status,
            "step": latest.step,
            "version": latest.version,
            "log_dir": latest.log_dir,
            "apply_run_id": latest.apply_run_id,
        },
        "cluster_has_failure": cluster_has_failure,
        "ok": ok,
    }))
}

fn print_daemon_status(config: &Config, home: &Path) {
    match probe_daemon_status(config, home) {
        DaemonStatus::Ready => println!("daemon:   ready"),
        DaemonStatus::Degraded(failing) => {
            if failing.is_empty() {
                println!("daemon:   degraded");
            } else {
                println!("daemon:   degraded ({})", failing.join(", "));
            }
        }
        DaemonStatus::Unavailable(reason) => println!("daemon:   unavailable ({reason})"),
    }
}

fn daemon_status_json(config: &Config, home: &Path) -> serde_json::Value {
    match probe_daemon_status(config, home) {
        DaemonStatus::Ready => serde_json::json!({ "status": "ready", "ok": true }),
        DaemonStatus::Degraded(failing) => {
            serde_json::json!({ "status": "degraded", "ok": false, "failing": failing })
        }
        DaemonStatus::Unavailable(reason) => {
            serde_json::json!({ "status": "unavailable", "ok": false, "reason": reason })
        }
    }
}

enum DaemonStatus {
    Ready,
    Degraded(Vec<String>),
    Unavailable(String),
}

fn probe_daemon_status(config: &Config, home: &Path) -> DaemonStatus {
    let key = match open_cli_key(config, home, CliKey::Session) {
        Ok(key) => key,
        Err(err) => return DaemonStatus::Unavailable(err.public_message()),
    };
    let base_url = match daemon_base_url(config.api.public_url.as_deref(), &config.api.bind) {
        Ok(url) => url,
        Err(err) => return DaemonStatus::Unavailable(err.public_message()),
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => return DaemonStatus::Unavailable(format!("runtime unavailable: {err}")),
    };
    runtime.block_on(async move { probe_daemon_status_async(&base_url, &key).await })
}

async fn probe_daemon_status_async(base_url: &str, session_key: &str) -> DaemonStatus {
    let url = format!("{}{}", base_url.trim_end_matches('/'), "/v1/health/ready");
    let client = match reqwest::Client::builder()
        .timeout(STATUS_DAEMON_PROBE_TIMEOUT)
        .build()
    {
        Ok(client) => client,
        Err(err) => return DaemonStatus::Unavailable(format!("client unavailable: {err}")),
    };
    let response = match client.get(url).bearer_auth(session_key).send().await {
        Ok(response) => response,
        Err(err) => return DaemonStatus::Unavailable(format!("request failed: {err}")),
    };
    let status = response.status();
    let body_text = match response.text().await {
        Ok(body) => body,
        Err(err) => return DaemonStatus::Unavailable(format!("response read failed: {err}")),
    };
    if status != reqwest::StatusCode::OK && status != reqwest::StatusCode::SERVICE_UNAVAILABLE {
        return DaemonStatus::Unavailable(format!("HTTP {status}"));
    }
    let body: serde_json::Value = match serde_json::from_str(&body_text) {
        Ok(body) => body,
        Err(err) => return DaemonStatus::Unavailable(format!("invalid JSON: {err}")),
    };
    let data = body.get("data").unwrap_or(&body);
    let ok = data.get("ok").and_then(serde_json::Value::as_bool);
    if status == reqwest::StatusCode::OK && ok != Some(false) {
        return DaemonStatus::Ready;
    }
    DaemonStatus::Degraded(failing_list(data))
}

fn failing_list(data: &serde_json::Value) -> Vec<String> {
    data.get("failing")
        .and_then(serde_json::Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}
