use std::io::{self, IsTerminal, Write};

use serde_json::Value;

use crate::config::{ArrayTargetConfig, Config};
use crate::error::{Result, StackError};
use crate::fs_util::home_dir;
use crate::runtime::agent::switch::{
    AgentSwitchPlan, AgentSwitchProviderStatus, AgentSwitchRequest as PlannedAgentSwitchRequest,
    plan_agent_switch,
};
use crate::runtime::install::agent_registry::{RegistryCatalog, RegistryEntry};

use super::install::operator_registry_override;
use super::{AgentSetArgs, AgentSwitchArgs};
use crate::cli::core::{CliMethod, daemon_base_url, daemon_request, resolve_admin_key};

pub(super) fn run_agent_switch(args: AgentSwitchArgs) -> Result<()> {
    let interactive = io::stdin().is_terminal();
    let admin_key = resolve_admin_key(args.admin_key.clone(), interactive)?;
    let home = home_dir()?;
    let config = Config::load_from_default_path()?;
    let registry = RegistryCatalog::load_with_override(&operator_registry_override(&home))?;
    if let Some(target) = config.array.target(&args.agent) {
        print_existing_target_switch_plan(&config, target)?;
    } else {
        let plan = plan_agent_switch(
            &config,
            &registry,
            PlannedAgentSwitchRequest {
                target_agent: args.agent.clone(),
                provider_id: args.provider.clone(),
                api_key_ref: args.api_key_ref.clone(),
            },
        )?;
        let target_entry = registry.lookup_required(&plan.target_agent_id)?;
        print_switch_plan(
            &plan.provider_status,
            target_entry,
            &plan,
            args.drop_configs,
        );
    }

    let base_url = daemon_base_url(config.api.public_url.as_deref(), &config.api.bind)?;
    let request = serde_json::json!({
        "agent": args.agent,
        "provider": args.provider,
        "api_key_ref": args.api_key_ref,
        "drop": args.drop_configs,
    });
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|source| StackError::ServeIo { source })?;
    let body = runtime.block_on(async {
        daemon_request(
            &base_url,
            CliMethod::Post,
            "/v1/agent/switch",
            &admin_key,
            Some(&request),
        )
        .await
    })?;
    let data = body.get("data").unwrap_or(&body);
    print_switch_result(data);

    let models = data
        .get("models")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let set_model = data
        .get("set_model")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    if set_model && interactive {
        if let Some(model) = prompt_model_choice(&models)? {
            super::set::run_agent_set(AgentSetArgs {
                custom_provider: false,
                provider: None,
                provider_name: None,
                base_url: None,
                provider_api: None,
                model: Some(model),
                model_name: None,
                context: None,
                output_max_tokens: None,
                mode: None,
                api_key_ref: None,
            })?;
            if data
                .get("restarted")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                runtime.block_on(async {
                    daemon_request(
                        &base_url,
                        CliMethod::Post,
                        "/v1/agent/restart",
                        &admin_key,
                        None,
                    )
                    .await
                    .map(|_| ())
                })?;
                println!("agent restart: running");
            }
        }
    } else if set_model {
        print_model_follow_up(&models);
    }
    Ok(())
}

fn print_existing_target_switch_plan(config: &Config, target: &ArrayTargetConfig) -> Result<()> {
    let agent = &target.id;
    if config.array.primary_target == *agent {
        return Err(StackError::InvalidParam {
            field: "agent",
            reason: format!("agent `{agent}` is already the default target"),
        });
    }
    let required_env_refs = &target.agent.env;
    println!(
        "agent switch plan: {} -> {agent}",
        config.array.primary_target
    );
    println!("will select existing Array target config");
    println!("migrated as-is: workspace, MCP, permissions, auth, and secrets config");
    if !required_env_refs.is_empty() {
        println!("required_env_refs: {}", required_env_refs.join(", "));
    }
    println!("requires input: none");
    Ok(())
}

fn print_switch_plan(
    provider_status: &AgentSwitchProviderStatus,
    target_entry: &RegistryEntry,
    plan: &AgentSwitchPlan,
    drop_configs: bool,
) {
    let target_sets_model = target_entry.set_model;
    println!(
        "agent switch plan: {} -> {}",
        plan.old_agent_id, plan.target_agent_id
    );
    if let Some(harness) = target_entry
        .harness
        .as_ref()
        .filter(|harness| !harness.install.is_provided_by_adapter())
    {
        println!("will install harness: {}", harness.id);
    }
    if let Some(adapter) = target_entry.adapter.as_ref() {
        println!("will install adapter: {}", adapter.id);
    }
    if drop_configs {
        println!("will drop source agent-owned config after successful switch");
    }
    for migration in &plan.secret_migrations {
        println!(
            "will copy secret ref if missing: {} -> {}",
            migration.from_ref, migration.to_ref
        );
    }
    if !plan.required_env_refs.is_empty() {
        println!("required_env_refs: {}", plan.required_env_refs.join(", "));
    }
    match provider_status {
        AgentSwitchProviderStatus::NotApplicable => {
            println!("migrated as-is: workspace, MCP, permissions, auth, and secrets config");
            if target_sets_model {
                println!("requires input: model");
            } else {
                println!("requires input: none");
            }
        }
        AgentSwitchProviderStatus::Reused {
            provider_id,
            api_key_ref,
        } => {
            println!(
                "migrated as-is: workspace, MCP, permissions, auth, secrets config, provider {provider_id}"
            );
            if let Some(api_key_ref) = api_key_ref {
                println!("migrated api_key_ref: {api_key_ref}");
            }
            if target_sets_model {
                println!("requires input: model");
            } else {
                println!("requires input: none");
            }
        }
        AgentSwitchProviderStatus::Set {
            provider_id,
            api_key_ref,
        } => {
            println!("migrated as-is: workspace, MCP, permissions, auth, and secrets config");
            println!("set from input: provider {provider_id}");
            if let Some(api_key_ref) = api_key_ref {
                println!("set from input: api_key_ref {api_key_ref}");
            }
            if target_sets_model {
                println!("requires input: model");
            } else {
                println!("requires input: none");
            }
        }
    }
}

fn print_switch_result(data: &Value) {
    let old_agent = data
        .get("old_agent_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let agent = data.get("agent_id").and_then(Value::as_str).unwrap_or("");
    println!("agent switch: {old_agent} -> {agent}");
    let provider_status = data
        .get("provider_status")
        .and_then(Value::as_str)
        .unwrap_or("");
    if provider_status == "not_applicable" {
        println!("provider: not applicable");
    } else {
        let provider = data.get("provider").and_then(Value::as_str).unwrap_or("");
        println!("provider: {provider} ({provider_status})");
        if let Some(api_key_ref) = data.get("api_key_ref").and_then(Value::as_str) {
            println!("api_key_ref: {api_key_ref}");
        }
    }
    if let Some(install) = data.get("install") {
        let outcome = install
            .get("outcome")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let path = install.get("path").and_then(Value::as_str).unwrap_or("");
        println!("agent install: {outcome}");
        println!("path: {path}");
    }
    if let Some(skills_port) = data.get("skills_port") {
        print_skills_port(skills_port);
    }
    if let Some(cleaned_configs) = data.get("cleaned_configs").and_then(Value::as_array) {
        for cleaned in cleaned_configs {
            let label = cleaned
                .get("label")
                .and_then(Value::as_str)
                .unwrap_or("config");
            let path = cleaned.get("path").and_then(Value::as_str).unwrap_or("");
            println!("cleaned {label}: {path}");
        }
    }
    if let Some(errors) = data.get("cleanup_errors").and_then(Value::as_array) {
        for error in errors {
            if let Some(error) = error.as_str() {
                println!("cleanup warning: {error}");
            }
        }
    }
    let restarted = data
        .get("restarted")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    println!("restarted: {restarted}");
}

fn print_skills_port(skills_port: &Value) {
    let status = skills_port
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let source_root = skills_port
        .get("source_root")
        .and_then(Value::as_str)
        .unwrap_or("");
    let target_root = skills_port
        .get("target_root")
        .and_then(Value::as_str)
        .unwrap_or("");
    match status {
        "shared" => println!("skills port: shared path {target_root}"),
        "none_found" => println!("skills port: none found in {source_root}"),
        "copied" => {
            let copied = skills_port
                .get("copied")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            let overwritten = skills_port
                .get("overwritten")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            println!("skills port: copied {copied}, overwritten {overwritten} -> {target_root}");
        }
        _ => println!("skills port: {status}"),
    }
}

fn print_model_follow_up(models: &[String]) {
    if models.is_empty() {
        println!("available model values: (none advertised)");
    } else {
        println!("available model values:");
        for model in models {
            println!("{model}");
        }
    }
    println!("set model: acps agent set --model <model-id>");
}

fn prompt_model_choice(models: &[String]) -> Result<Option<String>> {
    if models.is_empty() {
        print_model_follow_up(models);
        return Ok(None);
    }
    println!("available model values:");
    for (index, model) in models.iter().enumerate() {
        println!("  {}. {model}", index + 1);
    }
    print!("Select model number or id [blank to set later]: ");
    io::stdout()
        .flush()
        .map_err(|source| StackError::ServeIo { source })?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .map_err(|source| StackError::ServeIo { source })?;
    let answer = answer.trim();
    if answer.is_empty() {
        println!("set model: acps agent set --model <model-id>");
        return Ok(None);
    }
    if let Ok(index) = answer.parse::<usize>() {
        if index == 0 {
            return Err(StackError::InvalidParam {
                field: "model",
                reason: "model selection is out of range".to_owned(),
            });
        }
        return models
            .get(index - 1)
            .cloned()
            .map(Some)
            .ok_or_else(|| StackError::InvalidParam {
                field: "model",
                reason: "model selection is out of range".to_owned(),
            });
    }
    if models.iter().any(|model| model == answer) {
        return Ok(Some(answer.to_owned()));
    }
    Err(StackError::InvalidParam {
        field: "model",
        reason: format!("agent did not advertise `{answer}` as an available model"),
    })
}
