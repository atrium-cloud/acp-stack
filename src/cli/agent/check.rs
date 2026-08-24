use crate::config::Config;
use crate::error::{Result, StackError};
use crate::fs_util::{
    create_dir_owner_only, home_dir, parent_dir, pre_create_owner_only, set_owner_only_file,
};
use crate::runtime::install::agent_registry::RegistryCatalog;
use crate::state::{StateStore, default_state_path};

use super::install::operator_registry_override;
use crate::cli::core::{OutputFormat, print_json};

// Shared with the API's `GET /v1/agent/update/status` route; re-exported under the CLI's names.
pub(super) use crate::runtime::install::agent_version_check::{
    AgentVersionStatus as AgentCheckStatus, LiveLatestVersionResolver, agent_check_has_failure,
    build_agent_check_report,
};

pub(super) fn run_agent_check(output: OutputFormat) -> Result<()> {
    let home = home_dir()?;
    let config = Config::load_from_default_path()?;
    let registry = RegistryCatalog::load_with_override(&operator_registry_override(&home))?;
    // A custom (non-registry) agent has no managed steps; skip cleanly and exit 0.
    let entry = match registry.lookup_required(&config.agent.id) {
        Ok(entry) => entry,
        Err(StackError::AgentRegistryMissing { .. }) => {
            let reason = format!(
                "agent `{}` is not a managed registry agent; nothing to check",
                config.agent.id
            );
            if output.is_json() {
                print_json(&serde_json::json!({
                    "agent": config.agent.id,
                    "ok": true,
                    "skipped": true,
                    "reason": reason,
                    "steps": [],
                }))?;
            } else {
                println!("agent check: {}", config.agent.id);
                println!("skipped: {reason}");
            }
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    let state_path = default_state_path(&home);
    let state_dir = parent_dir(&state_path)?;
    create_dir_owner_only(state_dir)?;
    pre_create_owner_only(&state_path)?;
    let store = StateStore::open(&state_path)?;
    store.migrate()?;
    set_owner_only_file(&state_path)?;
    let installed_rows = store.latest_successful_installer_runs_for_agent(&config.agent.id)?;

    let resolver = LiveLatestVersionResolver;
    let report = build_agent_check_report(entry, &config.agent, &installed_rows, &resolver);
    let has_failure = agent_check_has_failure(&report);

    if output.is_json() {
        let steps = report
            .iter()
            .map(|(step, status)| serde_json::json!({ "step": step, "result": status }))
            .collect::<Vec<_>>();
        print_json(&serde_json::json!({
            "agent": config.agent.id,
            "ok": !has_failure,
            "steps": steps,
        }))?;
    } else {
        println!("agent check: {}", config.agent.id);
    }
    if report.is_empty() {
        if !output.is_json() {
            println!(
                "no installer runs recorded for `{}`; run `acps agent install` first",
                config.agent.id
            );
        }
        return Ok(());
    }
    if !output.is_json() {
        for (step, status) in &report {
            match status {
                AgentCheckStatus::UpToDate { version } => {
                    println!("{step}: up-to-date ({version})");
                }
                AgentCheckStatus::Stale { installed, latest } => {
                    println!("{step}: stale (installed {installed}, latest {latest})");
                }
                AgentCheckStatus::Unknown { reason } => {
                    println!("{step}: unknown ({reason})");
                }
                AgentCheckStatus::NotInstalled => {
                    println!("{step}: not installed");
                }
            }
        }
    }
    if has_failure {
        return Err(StackError::AgentCheckStale);
    }
    Ok(())
}
