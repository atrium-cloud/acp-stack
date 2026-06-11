use clap::{Args, Subcommand, ValueEnum};
use std::process::Command;

use crate::cli::core::{OutputFormat, print_json};
use crate::config::{Config, StackUpdatePolicy};
use crate::error::{Result, StackError};
use crate::fs_util::{
    atomic_write_owner_only, create_dir_owner_only, home_dir, parent_dir, pre_create_owner_only,
    set_owner_only_file,
};
use crate::runtime::install::stack_updater::{
    StackUpdateOptions, StackUpdateTarget, check_stack_update, install_stack_update,
};
use crate::state::{StateStore, default_state_path};

#[derive(Debug, Subcommand)]
pub enum UpdateCommand {
    /// Check GitHub Releases for an acp-stack update.
    Check,
    /// Install an acp-stack release into the current binary directory.
    Install(UpdateInstallArgs),
    /// Configure acp-stack self-update policy.
    Set(UpdateSetArgs),
}

#[derive(Debug, Args)]
pub struct UpdateInstallArgs {
    /// Install the latest non-prerelease GitHub Release.
    #[arg(long, conflicts_with = "version")]
    latest: bool,
    /// Install a specific GitHub Release tag, such as v0.2.0.
    #[arg(long, conflicts_with = "latest")]
    version: Option<String>,
    /// Permit major-version or manifest-marked breaking releases.
    #[arg(long = "allow-breaking")]
    allow_breaking: bool,
    /// Internal systemd timer entrypoint. Enforces configured policy.
    #[arg(long, hide = true)]
    auto: bool,
    /// Internal systemd updater hook. Restarts this service only after install.
    #[arg(long = "restart-service", hide = true)]
    restart_service: Option<String>,
}

#[derive(Debug, Args)]
pub struct UpdateSetArgs {
    /// Self-update policy.
    #[arg(long, value_enum)]
    policy: Option<UpdatePolicyArg>,
    /// Auto-update frequency, such as 12h, 1d, 3d, or 4w.
    #[arg(long)]
    frequency: Option<String>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum UpdatePolicyArg {
    Compatible,
    SecurityCritical,
    Manual,
}

pub(super) fn run_update_command(command: UpdateCommand, output: OutputFormat) -> Result<()> {
    match command {
        UpdateCommand::Check => run_update_check(output),
        UpdateCommand::Install(args) => run_update_install(args, output),
        UpdateCommand::Set(args) => {
            if output.is_json() {
                return Err(StackError::InvalidParam {
                    field: "--format",
                    reason: "update set does not support json output".to_owned(),
                });
            }
            run_update_set(args)
        }
    }
}

fn run_update_check(output: OutputFormat) -> Result<()> {
    let (config, store) = load_config_and_state()?;
    let report = check_stack_update(
        &config,
        &store,
        StackUpdateOptions {
            target: StackUpdateTarget::Latest,
            version: None,
            allow_breaking: false,
            auto: false,
        },
    )?;
    print_report(&report, output)
}

fn run_update_install(args: UpdateInstallArgs, output: OutputFormat) -> Result<()> {
    let (target, version) = resolve_install_target(&args)?;
    let (config, store) = load_config_and_state()?;
    let report = install_stack_update(
        &config,
        &store,
        StackUpdateOptions {
            target,
            version,
            allow_breaking: args.allow_breaking,
            auto: args.auto,
        },
    )?;
    if report.status == crate::runtime::install::stack_updater::StackUpdateStatus::Installed
        && let Some(service) = args.restart_service.as_deref()
    {
        restart_systemd_service(service)?;
    }
    print_report(&report, output)
}

fn restart_systemd_service(service: &str) -> Result<()> {
    if service.trim().is_empty() || service.contains('/') {
        return Err(StackError::InvalidParam {
            field: "--restart-service",
            reason: "service name must be non-empty and must not contain '/'".to_owned(),
        });
    }
    let status = Command::new("systemctl")
        .args(["try-restart", service])
        .status()
        .map_err(|source| StackError::AgentInitializeFailed {
            reason: format!("failed to run systemctl try-restart {service}: {source}"),
        })?;
    if status.success() {
        return Ok(());
    }
    Err(StackError::AgentInitializeFailed {
        reason: format!("systemctl try-restart {service} exited with {status}"),
    })
}

fn resolve_install_target(args: &UpdateInstallArgs) -> Result<(StackUpdateTarget, Option<String>)> {
    match (args.latest, args.version.as_ref()) {
        (true, None) => Ok((StackUpdateTarget::Latest, None)),
        (false, Some(version)) => Ok((StackUpdateTarget::Version, Some(version.clone()))),
        _ => Err(StackError::InvalidParam {
            field: "acps update install",
            reason: "pass exactly one of --latest or --version <tag>".to_owned(),
        }),
    }
}

fn run_update_set(args: UpdateSetArgs) -> Result<()> {
    if args.policy.is_none() && args.frequency.is_none() {
        return Err(StackError::InvalidParam {
            field: "update.set",
            reason: "pass --policy or --frequency".to_owned(),
        });
    }
    let config_path = crate::config::default_config_path()?;
    let mut config = Config::load_from_path(&config_path)?;
    if let Some(policy) = args.policy {
        config.updates.acp_stack.policy = match policy {
            UpdatePolicyArg::Compatible => StackUpdatePolicy::Compatible,
            UpdatePolicyArg::SecurityCritical => StackUpdatePolicy::SecurityCritical,
            UpdatePolicyArg::Manual => StackUpdatePolicy::Manual,
        };
    }
    if let Some(frequency) = args.frequency {
        config.updates.acp_stack.frequency = frequency;
    }
    let canonical = config.to_canonical_toml()?;
    crate::config::load_config_from_str(&canonical)?;
    atomic_write_owner_only(&config_path, canonical.as_bytes())?;
    println!(
        "acp-stack update policy: {}",
        policy_label(config.updates.acp_stack.policy)
    );
    println!("frequency: {}", config.updates.acp_stack.frequency);
    Ok(())
}

fn load_config_and_state() -> Result<(Config, StateStore)> {
    let home = home_dir()?;
    let config = Config::load_from_default_path()?;
    let state_path = default_state_path(&home);
    let state_dir = parent_dir(&state_path)?;
    create_dir_owner_only(state_dir)?;
    pre_create_owner_only(&state_path)?;
    let store = StateStore::open(&state_path)?;
    store.migrate()?;
    set_owner_only_file(&state_path)?;
    Ok((config, store))
}

fn print_report(
    report: &crate::runtime::install::stack_updater::StackUpdateReport,
    output: OutputFormat,
) -> Result<()> {
    if output.is_json() {
        print_json(
            &serde_json::to_value(report).map_err(|source| StackError::ConfigWrite {
                path: std::path::PathBuf::from("stack-update-report.json"),
                source: std::io::Error::other(source),
            })?,
        )?;
        return Ok(());
    }
    println!("acp-stack update: {:?}", report.status);
    println!("current: {}", report.current_version);
    if let Some(tag) = report.target_tag.as_deref() {
        println!("target tag: {tag}");
    }
    if let Some(version) = report.target_version.as_deref() {
        println!("target version: {version}");
    }
    println!("decision: {:?}", report.decision);
    if let Some(message) = report.message.as_deref() {
        println!("{message}");
    }
    Ok(())
}

fn policy_label(policy: StackUpdatePolicy) -> &'static str {
    match policy {
        StackUpdatePolicy::Compatible => "compatible",
        StackUpdatePolicy::SecurityCritical => "security-critical",
        StackUpdatePolicy::Manual => "manual",
    }
}
