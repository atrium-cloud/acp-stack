use crate::config::Config;
use crate::error::Result;
use crate::fs_util::{
    create_dir_owner_only, home_dir, parent_dir, pre_create_owner_only, set_owner_only_file,
};
use crate::runtime::install::agent_installer::{STEP_ADAPTER, STEP_HARNESS, STEP_INSTALL};
use crate::runtime::install::agent_registry::{
    LEGACY_PLACEHOLDER_AGENT_ID, RegistryCatalog, RegistryEntry,
};
use crate::state::{StateStore, default_state_path};

use super::install::operator_registry_override;
use crate::cli::core::{OutputFormat, print_json};

pub(super) fn run_agent_status(output: OutputFormat) -> Result<()> {
    let home = home_dir()?;
    let config = Config::load_from_default_path()?;
    let registry = RegistryCatalog::load_with_override(&operator_registry_override(&home))?;
    let registry_entry = registry.lookup(&config.agent.id);
    let state_path = default_state_path(&home);
    let state_dir = parent_dir(&state_path)?;
    create_dir_owner_only(state_dir)?;
    pre_create_owner_only(&state_path)?;
    let store = StateStore::open(&state_path)?;
    store.migrate()?;
    set_owner_only_file(&state_path)?;

    let installed_versions = store.latest_successful_installer_runs_for_agent(&config.agent.id)?;
    let capabilities_record = store.latest_agent_capabilities(&config.agent.id)?;
    let latest_failure = store.latest_agent_failure(&config.agent.id)?;
    let lifecycle = store.query_agent_lifecycle(10)?;

    if output.is_json() {
        let params = agent_status_params(&config, registry_entry)
            .into_iter()
            .map(|param| match param {
                AgentStatusParamState::Configured(name, value) => {
                    serde_json::json!({ "name": name, "state": "configured", "value": value })
                }
                AgentStatusParamState::Unset(name) => {
                    serde_json::json!({ "name": name, "state": "unset" })
                }
                AgentStatusParamState::Unavailable(name) => {
                    serde_json::json!({ "name": name, "state": "unavailable" })
                }
            })
            .collect::<Vec<_>>();
        let installed_versions = installed_versions
            .iter()
            .map(|row| {
                serde_json::json!({
                    "step": &row.step,
                    "label": installed_version_label(&row.step),
                    "version": &row.version,
                    "started_at": &row.started_at,
                })
            })
            .collect::<Vec<_>>();
        let capabilities = capabilities_record.as_ref().map(|record| {
            serde_json::json!({
                "agent_id": &record.agent_id,
                "captured_at": &record.captured_at,
                "capabilities": serde_json::from_str::<serde_json::Value>(&record.capabilities_json)
                    .unwrap_or_else(|_| serde_json::Value::String(record.capabilities_json.clone())),
            })
        });
        let lifecycle = lifecycle
            .iter()
            .map(|event| {
                serde_json::json!({
                    "id": &event.id,
                    "created_at": &event.created_at,
                    "event_kind": &event.event_kind,
                    "message": &event.message,
                })
            })
            .collect::<Vec<_>>();
        print_json(&serde_json::json!({
            "agent": config.agent.id,
            "command": config.agent.command,
            "invalid_config": config.agent.id == LEGACY_PLACEHOLDER_AGENT_ID,
            "params": params,
            "installed_versions": installed_versions,
            "latest_capabilities": capabilities,
            "latest_failure": latest_failure.as_ref().map(|failure| serde_json::json!({
                "id": &failure.id,
                "created_at": &failure.created_at,
                "event_kind": &failure.event_kind,
                "message": &failure.message,
                "reason": &failure.reason,
            })),
            "recent_lifecycle": lifecycle,
        }))?;
        return Ok(());
    }

    println!("agent: {}", config.agent.id);
    if config.agent.id == LEGACY_PLACEHOLDER_AGENT_ID {
        println!("invalid config: legacy placeholder agent; select a real supported agent");
    }
    print_agent_status_params(&config, registry_entry);
    print_installed_versions(&installed_versions);
    println!("command: {}", config.agent.command);

    match capabilities_record {
        Some(record) => {
            if let Ok(capabilities) = serde_json::from_str::<
                crate::runtime::agent::acp_bridge::AgentCapabilitiesDto,
            >(&record.capabilities_json)
            {
                println!("ACP version: {}", capabilities.protocol_version);
            }
            println!("latest capabilities captured: {}", record.captured_at);
            println!("capabilities_json: {}", record.capabilities_json);
        }
        None => println!("latest capabilities: none recorded yet"),
    }

    if let Some(failure) = latest_failure {
        println!("latest failure: {}", failure.reason);
        println!("latest failure at: {}", failure.created_at);
    }

    if lifecycle.is_empty() {
        println!("recent lifecycle: (no rows)");
    } else {
        println!("recent lifecycle:");
        for event in lifecycle {
            println!(
                "  {} {} {}",
                event.created_at, event.event_kind, event.message
            );
        }
    }
    Ok(())
}

enum AgentStatusParamState {
    Configured(&'static str, String),
    Unset(&'static str),
    Unavailable(&'static str),
}

fn print_agent_status_params(config: &Config, registry_entry: Option<&RegistryEntry>) {
    let params = agent_status_params(config, registry_entry);
    let mut unset = Vec::new();
    let mut unavailable = Vec::new();

    for param in params {
        match param {
            AgentStatusParamState::Configured(name, value) => println!("{name}: {value}"),
            AgentStatusParamState::Unset(name) => unset.push(name),
            AgentStatusParamState::Unavailable(name) => unavailable.push(name),
        }
    }

    if !unset.is_empty() {
        println!("{} unset", human_list(&unset));
    }
    if !unavailable.is_empty() {
        println!("{} unavailable", human_list(&unavailable));
    }
}

fn agent_status_params(
    config: &Config,
    registry_entry: Option<&RegistryEntry>,
) -> Vec<AgentStatusParamState> {
    let provider = config
        .agent
        .provider
        .as_ref()
        .map(|provider| provider.id.clone());
    let model = config.agent.model.clone().or_else(|| {
        config
            .agent
            .provider
            .as_ref()
            .and_then(|provider| provider.model.clone())
    });
    let mode = config.agent.mode.clone();

    vec![
        agent_status_param(
            "provider",
            provider,
            registry_entry.is_some_and(|entry| entry.set_provider),
        ),
        agent_status_param(
            "model",
            model,
            registry_entry.is_some_and(|entry| entry.set_model),
        ),
        agent_status_param(
            "mode",
            mode,
            registry_entry.is_some_and(|entry| entry.set_mode),
        ),
    ]
}

fn agent_status_param(
    name: &'static str,
    configured: Option<String>,
    supported: bool,
) -> AgentStatusParamState {
    if let Some(value) = configured {
        return AgentStatusParamState::Configured(name, value);
    }
    if supported {
        AgentStatusParamState::Unset(name)
    } else {
        AgentStatusParamState::Unavailable(name)
    }
}

/// Render one line per `installer_runs.step` recorded for this agent, showing
/// the step name and the resolved version when known. Steps that ran without
/// a recorded version (shell installs) print "version unknown"
/// so the operator can tell the difference between "no install row at all"
/// and "install ran but produced no version".
fn print_installed_versions(rows: &[crate::state::InstallerRun]) {
    if rows.is_empty() {
        return;
    }
    for row in rows {
        let label = installed_version_label(&row.step);
        match row.version.as_deref() {
            Some(value) if !value.is_empty() => {
                println!("{label}: {value}");
            }
            _ => println!("{label}: version unknown"),
        }
    }
}

fn installed_version_label(step: &str) -> String {
    match step {
        STEP_INSTALL => "agent version".to_owned(),
        STEP_HARNESS => "harness version".to_owned(),
        STEP_ADAPTER => "adapter version".to_owned(),
        other => format!("{other} version"),
    }
}

fn human_list(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [single] => (*single).to_owned(),
        [first, second] => format!("{first} and {second}"),
        _ => {
            let (last, rest) = items.split_last().expect("non-empty list");
            format!("{}, and {last}", rest.join(", "))
        }
    }
}
