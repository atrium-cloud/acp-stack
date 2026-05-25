use crate::config::Config;
use crate::error::Result;
use crate::fs_util::{
    create_dir_owner_only, home_dir, parent_dir, pre_create_owner_only, set_owner_only_file,
};
use crate::runtime::install::agent_installer::{STEP_ADAPTER, STEP_HARNESS, STEP_INSTALL};
use crate::runtime::install::agent_registry::{RegistryCatalog, RegistryEntry};
use crate::state::{StateStore, default_state_path};

use super::install::operator_registry_override;

pub(super) fn run_agent_status() -> Result<()> {
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

    println!("agent: {}", config.agent.id);
    print_agent_status_params(&config, registry_entry);
    let installed_versions = store.latest_successful_installer_runs_for_agent(&config.agent.id)?;
    print_installed_versions(&installed_versions);
    println!("command: {}", config.agent.command);

    match store.latest_agent_capabilities(&config.agent.id)? {
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

    let lifecycle = store.query_agent_lifecycle(10)?;
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
