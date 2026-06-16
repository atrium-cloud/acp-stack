use std::io::IsTerminal;
use std::path::PathBuf;

use http::StatusCode;

use crate::cli::agent::install::{local_bin_dir, operator_registry_override, post_agent_daemon};
use crate::cli::core::{
    CliMethod, OutputFormat, daemon_base_url, local_daemon_request, print_json, resolve_admin_key,
};
use crate::config::{AgentAutoUpdateConfig, Config, DEFAULT_AGENT_AUTO_UPDATE_FREQUENCY};
use crate::error::{Result, StackError};
use crate::fs_util::{
    atomic_write_owner_only, create_dir_owner_only, home_dir, parent_dir, pre_create_owner_only,
    set_owner_only_file,
};
use crate::runtime::install::agent_registry::RegistryCatalog;
use crate::runtime::install::agent_updater::{
    AgentUpdateOptions, AgentUpdateReport, update_agent_for_config,
};
use crate::state::{StateStore, default_installer_log_base, default_state_path};

use super::{AgentUpdateArgs, AgentUpdateSetArgs, AgentUpdateSubcommand};

pub(super) fn run_agent_update(args: AgentUpdateArgs, output: OutputFormat) -> Result<()> {
    let force = args.force;
    let restart = args.restart;
    let admin_key = args.admin_key;
    match args.command {
        Some(AgentUpdateSubcommand::Set(set_args)) => {
            if output.is_json() {
                return Err(StackError::InvalidParam {
                    field: "--format",
                    reason: "agent update set does not support json output".to_owned(),
                });
            }
            run_agent_update_set(set_args)
        }
        None => run_agent_update_execute(force, restart, admin_key, output),
    }
}

fn run_agent_update_set(args: AgentUpdateSetArgs) -> Result<()> {
    if !args.auto_on && !args.auto_off && args.frequency.is_none() {
        return Err(StackError::InvalidParam {
            field: "agent.update.set",
            reason: "pass --auto-on, --auto-off, or --frequency".to_owned(),
        });
    }
    let config_path = crate::config::default_config_path()?;
    let mut config = Config::load_from_path(&config_path)?;
    // Auto-update drives registry-managed harness/adapter steps. An escape-hatch
    // agent (`[agent.install]`, no registry entry) has nothing to update, so the
    // daemon loop would only ever record `agent.update.failed`. Reject up front.
    if args.auto_on {
        let home = home_dir()?;
        let registry = RegistryCatalog::load_with_override(&operator_registry_override(&home))?;
        if registry.lookup(&config.agent.id).is_none() {
            return Err(StackError::InvalidParam {
                field: "agent.auto_update.enabled",
                reason: format!(
                    "agent `{}` is not a managed registry agent; auto-update is unavailable for escape-hatch installs",
                    config.agent.id
                ),
            });
        }
    }
    let existing = config.agent.auto_update.take();
    let mut auto_update = existing.unwrap_or_else(|| AgentAutoUpdateConfig {
        enabled: false,
        frequency: DEFAULT_AGENT_AUTO_UPDATE_FREQUENCY.to_owned(),
    });
    if args.auto_on {
        auto_update.enabled = true;
    }
    if args.auto_off {
        auto_update.enabled = false;
    }
    if let Some(frequency) = args.frequency {
        auto_update.frequency = frequency;
    }
    config.agent.auto_update = Some(auto_update);
    let canonical = config.to_canonical_toml()?;
    // Validate the serialized form round-trips before persisting.
    crate::config::load_config_from_str(&canonical)?;
    atomic_write_owner_only(&config_path, canonical.as_bytes())?;
    let auto_update = config
        .agent
        .auto_update
        .as_ref()
        .expect("auto_update set above");
    println!(
        "agent update auto: {}",
        if auto_update.enabled {
            "enabled"
        } else {
            "disabled"
        }
    );
    println!("frequency: {}", auto_update.frequency);
    Ok(())
}

fn run_agent_update_execute(
    force: bool,
    restart: bool,
    admin_key: Option<String>,
    output: OutputFormat,
) -> Result<()> {
    if !output.is_json() {
        println!("progress: preparing agent update");
    }
    let home = home_dir()?;
    let config = Config::load_from_default_path()?;
    let daemon = query_daemon_agent_state(&config)?;
    // `updating` counts as busy: a concurrent daemon-side auto-update must not
    // race a second offline update against the same install destination.
    let daemon_running = daemon
        .as_deref()
        .is_some_and(|state| matches!(state, "starting" | "running" | "stopping" | "updating"));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|source| StackError::ServeIo { source })?;
    let base_url = if restart && daemon_running {
        Some(daemon_base_url(
            config.api.public_url.as_deref(),
            &config.api.bind,
        )?)
    } else {
        None
    };
    let admin_key = if restart && daemon_running {
        Some(resolve_admin_key(
            admin_key,
            std::io::stdin().is_terminal(),
        )?)
    } else {
        None
    };

    if let (Some(base_url), Some(admin_key)) = (base_url.as_deref(), admin_key.as_deref()) {
        runtime.block_on(post_agent_daemon(base_url, "/v1/agent/stop", admin_key))?;
    }

    let report_result = update_agent_offline(
        &home,
        &config,
        AgentUpdateOptions {
            force,
            agent_running: daemon_running && !restart,
        },
    );

    let restart_result = if restart
        && daemon_running
        && let (Some(base_url), Some(admin_key)) = (base_url.as_deref(), admin_key.as_deref())
    {
        runtime
            .block_on(post_agent_daemon(base_url, "/v1/agent/start", admin_key))
            .map(|_| ())
    } else {
        Ok(())
    };

    let report = match (report_result, restart_result) {
        (Ok(report), Ok(())) => report,
        (Ok(_report), Err(err)) => return Err(err),
        (Err(err), Ok(())) => return Err(err),
        (Err(update_err), Err(restart_err)) => {
            return Err(StackError::AgentInitializeFailed {
                reason: format!(
                    "agent update failed: {update_err}; agent restart failed: {restart_err}"
                ),
            });
        }
    };

    if output.is_json() {
        print_json(&serde_json::to_value(&report).map_err(|err| {
            StackError::AgentInitializeFailed {
                reason: format!("agent update report was not JSON-serializable: {err}"),
            }
        })?)?;
        return report_failed_error(&report).map_or(Ok(()), Err);
    }
    print_update_report(&report);
    if let Some(err) = report_failed_error(&report) {
        return Err(err);
    }
    Ok(())
}

fn update_agent_offline(
    home: &std::path::Path,
    config: &Config,
    options: AgentUpdateOptions,
) -> Result<crate::runtime::install::agent_updater::AgentUpdateReport> {
    let state_path = default_state_path(home);
    let state_dir = parent_dir(&state_path)?;
    create_dir_owner_only(state_dir)?;
    pre_create_owner_only(&state_path)?;
    let store = StateStore::open(&state_path)?;
    store.migrate()?;
    set_owner_only_file(&state_path)?;

    let registry = RegistryCatalog::load_with_override(&operator_registry_override(home))?;
    let entry = registry.lookup_required(&config.agent.id)?;
    let workspace_root = PathBuf::from(config.workspace.root.clone());
    let local_bin = local_bin_dir(home);
    let log_base = default_installer_log_base(home);
    update_agent_for_config(
        config,
        entry,
        &store,
        &workspace_root,
        &local_bin,
        Some(&log_base),
        options,
    )
}

fn print_update_report(report: &crate::runtime::install::agent_updater::AgentUpdateReport) {
    println!("agent update: {}", report.agent);
    if report.skipped {
        println!(
            "skipped: {}",
            report.reason.as_deref().unwrap_or("update skipped")
        );
        return;
    }
    for step in &report.steps {
        let method = step.method.as_deref().unwrap_or("unknown");
        match (&step.installed, &step.latest) {
            (Some(installed), Some(latest)) => {
                println!(
                    "{}: {:?} via {} (installed {}, latest {})",
                    step.step, step.status, method, installed, latest
                );
            }
            _ => println!("{}: {:?} via {}", step.step, step.status, method),
        }
        if let Some(message) = step.message.as_deref() {
            println!("  {message}");
        }
    }
}

fn report_failed_error(report: &AgentUpdateReport) -> Option<StackError> {
    if !report.has_failed_steps() {
        return None;
    }
    let failed_steps = report
        .steps
        .iter()
        .filter(|step| {
            step.status == crate::runtime::install::agent_updater::AgentUpdateStepStatus::Failed
        })
        .map(|step| step.step.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    Some(StackError::AgentInitializeFailed {
        reason: format!("agent update failed for step(s): {failed_steps}"),
    })
}

fn query_daemon_agent_state(config: &Config) -> Result<Option<String>> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|source| StackError::ServeIo { source })?;
    runtime.block_on(async {
        match local_daemon_request(config, CliMethod::Get, "/v1/agent/status", None).await {
            Ok(body) => Ok(body["data"]["process_state"].as_str().map(str::to_owned)),
            Err(StackError::ServeIo { source })
                if matches!(
                    source.kind(),
                    std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                ) =>
            {
                Ok(None)
            }
            Err(StackError::AgentApiStatus { status, .. }) if status == StatusCode::NOT_FOUND => {
                Ok(None)
            }
            Err(err) => Err(err),
        }
    })
}
