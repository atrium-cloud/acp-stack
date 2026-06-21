use crate::config::{self, Config};
use crate::error::{Result, StackError};
use crate::fs_util::atomic_write_owner_only;

use super::super::core::{OutputFormat, print_json};
use super::{AgentDefaultArgs, AgentDefaultCommand};

pub(super) fn run_agent_default(args: AgentDefaultArgs, output: OutputFormat) -> Result<()> {
    match args.command {
        AgentDefaultCommand::Set(args) => run_agent_default_set(args.agent, output),
    }
}

fn run_agent_default_set(agent_id: String, output: OutputFormat) -> Result<()> {
    let config_path = config::default_config_path()?;
    let mut config = Config::load_from_path(&config_path)?;
    let agent = config
        .array
        .target(&agent_id)
        .map(|target| target.agent.clone())
        .ok_or_else(|| StackError::InvalidParam {
            field: "agent",
            reason: format!("unknown Array target `{agent_id}`"),
        })?;
    config.array.primary_target = agent_id.clone();
    config.agent = agent;
    let canonical = config.to_canonical_toml()?;
    let validated = config::load_config_from_str(&canonical)?;
    atomic_write_owner_only(&config_path, validated.to_canonical_toml()?.as_bytes())?;

    if output.is_json() {
        print_json(&serde_json::json!({
            "primary_target": agent_id,
        }))?;
    } else {
        println!("agent default: {agent_id}");
    }
    Ok(())
}
