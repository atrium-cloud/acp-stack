use std::io::{self, IsTerminal, Write};

use clap::{Args, Subcommand};

use crate::config::Config;
use crate::error::{Result, StackError};
use crate::fs_util::home_dir;
use crate::runtime::dependencies::deps_apply::{
    DepApplyOutcome, apply_dependencies, apply_dependencies_with_progress, candidate_summary_line,
    candidates_for, summarize_candidates,
};
use crate::state::{StateStore, default_state_path};

use super::core::{OutputFormat, print_json};

#[derive(Debug, Subcommand)]
pub enum DepsCommand {
    /// Print declared dependency status.
    Check,
    /// Run the declared install action for missing dependencies.
    Apply(DepsApplyArgs),
}

#[derive(Debug, Args)]
pub struct DepsApplyArgs {
    /// Skip the confirmation prompt. Required for non-interactive
    /// runs that have any actionable dep.
    #[arg(long)]
    yes: bool,
    /// Apply only deps whose `feature` matches this string.
    #[arg(long, value_name = "FEATURE")]
    feature: Option<String>,
}

pub(super) fn run_deps_command(command: DepsCommand, output: OutputFormat) -> Result<()> {
    match command {
        DepsCommand::Check => run_check(output),
        DepsCommand::Apply(args) => run_apply(args, output),
    }
}

fn run_check(output: OutputFormat) -> Result<()> {
    let config = Config::load_from_default_path()?;
    let report = crate::runtime::dependencies::deps::check_dependencies(&config);
    if output.is_json() {
        print_json(
            &serde_json::to_value(&report).map_err(|source| StackError::ServeIo {
                source: std::io::Error::other(format!("serialize deps report: {source}")),
            })?,
        )?;
        return Ok(());
    }
    if report.dependencies.is_empty() {
        println!("no dependencies declared in [dependencies]");
        return Ok(());
    }
    for entry in &report.dependencies {
        let status = if entry.available {
            if let Some(path) = &entry.path {
                format!("OK  {path}")
            } else {
                "OK".to_owned()
            }
        } else {
            let reason = entry.reason.as_deref().unwrap_or("unavailable");
            format!("MISS {reason}")
        };
        let required = if entry.required { "*" } else { " " };
        println!(
            "{required}{kind:<8} {name:<24} {status}",
            kind = format!("{:?}", entry.kind).to_lowercase(),
            name = entry.name,
        );
    }
    Ok(())
}

fn run_apply(args: DepsApplyArgs, output: OutputFormat) -> Result<()> {
    let config = Config::load_from_default_path()?;
    let candidates = candidates_for(&config, args.feature.as_deref());
    if candidates.is_empty() {
        if output.is_json() {
            print_json(&serde_json::json!({
                "candidates": [],
                "results": [],
                "feature": args.feature,
            }))?;
        } else if args.feature.is_some() {
            println!(
                "no actionable dependencies match --feature {filter:?}",
                filter = args.feature.as_deref().unwrap_or(""),
            );
        } else {
            println!(
                "no dependencies declare an [install] block — declare one to make a dep actionable"
            );
        }
        return Ok(());
    }

    let (count, any_system) = summarize_candidates(&candidates);
    if !output.is_json() {
        println!("`acps deps apply` will run {count} install action(s):");
        for candidate in &candidates {
            println!("  - {}", candidate_summary_line(candidate));
        }
        if any_system {
            println!(
                "WARNING: one or more actions declare scope=system; they require root privilege."
            );
        }
    }

    if !confirm(args.yes)? {
        if output.is_json() {
            print_json(&serde_json::json!({ "aborted": true, "confirmed": false }))?;
        } else {
            println!("aborted (no confirmation)");
        }
        return Ok(());
    }

    let home = home_dir()?;
    let state_path = default_state_path(&home);
    if !state_path.exists() {
        // The runner runs operator install scripts and persists per-
        // action `installer_runs` rows for audit. Silently downgrading
        // to "audit off" when the state DB is missing would let
        // side-effectful installs run without a trail; fail fast with
        // a clear pointer to `acps init` instead.
        return Err(StackError::InvalidParam {
            field: "state",
            reason: format!(
                "state DB missing at `{}`; run `acps init` first so `acps deps apply` can record per-action audit rows",
                state_path.display(),
            ),
        });
    }
    let store = StateStore::open(&state_path)?;
    // Migrate before any install snippet runs. If the on-disk schema
    // is older than the binary's, the first `append_installer_run`
    // would fail mid-apply — by then a side-effectful install would
    // already have executed without an audit row. Failing fast here
    // keeps "no audit row recorded" from coexisting with "side
    // effects committed".
    store.migrate()?;
    let shell = &config.workspace.default_shell;
    let report = if output.is_json() {
        apply_dependencies(&config, args.feature.as_deref(), Some(&store), shell)?
    } else {
        let mut stdout = std::io::stdout();
        apply_dependencies_with_progress(
            &config,
            args.feature.as_deref(),
            Some(&store),
            shell,
            |current, total, name| {
                writeln!(
                    stdout,
                    "progress: applying dependency {current}/{total}: {name}"
                )
                .map_err(|source| StackError::AgentInitializeFailed {
                    reason: format!("write dependency apply progress failed: {source}"),
                })?;
                stdout
                    .flush()
                    .map_err(|source| StackError::AgentInitializeFailed {
                        reason: format!("flush dependency apply progress failed: {source}"),
                    })?;
                Ok(())
            },
        )?
    };
    if output.is_json() {
        print_json(&deps_apply_report_json(&report)?)?;
    } else {
        println!("---");
        print_apply_status_section("before", &report.before);
        println!("---");
        println!("results:");
        for result in &report.results {
            let line = match &result.outcome {
                DepApplyOutcome::Installed => format!("installed   {}", result.name),
                DepApplyOutcome::AlreadyPresent => format!("already     {}", result.name),
                DepApplyOutcome::PrivilegeRequired { uid } => {
                    format!("privreq     {} (uid={uid}; needs root)", result.name)
                }
                DepApplyOutcome::Failed {
                    exit_code,
                    stderr_tail,
                } => {
                    let code = exit_code
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "?".to_owned());
                    format!(
                        "failed      {} (exit={code}, stderr_tail={tail:?})",
                        result.name,
                        tail = stderr_tail,
                    )
                }
            };
            println!("  {line}");
        }
        println!("---");
        print_apply_status_section("after", &report.after);
        println!("audit run: {}", report.apply_run_id);
    }

    // Surface any non-success as a non-zero exit so automation can
    // gate on it. Without this, `acps deps apply --yes` in a CI
    // script would report success even when a required install
    // failed or was blocked on privilege. Use the worst-case across
    // every result.
    let mut bad: Vec<String> = Vec::new();
    for result in &report.results {
        match &result.outcome {
            DepApplyOutcome::Failed { exit_code, .. } => {
                let code = exit_code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "?".into());
                bad.push(format!("{} failed (exit={code})", result.name));
            }
            DepApplyOutcome::PrivilegeRequired { uid } => {
                bad.push(format!("{} needs root privilege (uid={uid})", result.name,));
            }
            DepApplyOutcome::Installed | DepApplyOutcome::AlreadyPresent => {}
        }
    }
    if !bad.is_empty() {
        return Err(StackError::InvalidParam {
            field: "deps",
            reason: format!(
                "deps apply produced non-success outcomes: {}; inspect audit rows with `acps installer history --agent deps_apply` (apply_run_id={})",
                bad.join("; "),
                report.apply_run_id,
            ),
        });
    }
    Ok(())
}

fn deps_apply_report_json(
    report: &crate::runtime::dependencies::deps_apply::DepsApplyReport,
) -> Result<serde_json::Value> {
    let before = serde_json::to_value(&report.before).map_err(|source| StackError::ServeIo {
        source: std::io::Error::other(format!("serialize deps apply before status: {source}")),
    })?;
    let after = serde_json::to_value(&report.after).map_err(|source| StackError::ServeIo {
        source: std::io::Error::other(format!("serialize deps apply after status: {source}")),
    })?;
    let results = report
        .results
        .iter()
        .map(|result| {
            let outcome = match &result.outcome {
                DepApplyOutcome::Installed => serde_json::json!({ "kind": "installed" }),
                DepApplyOutcome::AlreadyPresent => {
                    serde_json::json!({ "kind": "alreadypresent" })
                }
                DepApplyOutcome::PrivilegeRequired { uid } => {
                    serde_json::json!({ "kind": "privilegerequired", "uid": uid })
                }
                DepApplyOutcome::Failed { exit_code, .. } => {
                    serde_json::json!({
                        "kind": "failed",
                        "exit_code": exit_code,
                        "stderr_tail_omitted": true,
                    })
                }
            };
            serde_json::json!({
                "name": &result.name,
                "outcome": outcome,
                "post_status": &result.post_status,
            })
        })
        .collect::<Vec<_>>();

    Ok(serde_json::json!({
        "apply_run_id": &report.apply_run_id,
        "before": before,
        "after": after,
        "results": results,
    }))
}

fn print_apply_status_section(
    label: &str,
    entries: &[crate::runtime::dependencies::deps::DepStatus],
) {
    println!("{label}:");
    for entry in entries {
        let status = if entry.available { "OK  " } else { "MISS" };
        println!("  {status} {}", entry.name);
    }
}

fn confirm(yes: bool) -> Result<bool> {
    if yes {
        return Ok(true);
    }
    if !io::stdin().is_terminal() {
        return Err(StackError::InvalidParam {
            field: "yes",
            reason: "non-interactive run; pass --yes to confirm".to_owned(),
        });
    }
    print!("apply these dependency actions now? [y/N]: ");
    io::stdout()
        .flush()
        .map_err(|source| StackError::ConfigWrite {
            path: std::path::PathBuf::from("stdout"),
            source,
        })?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .map_err(|source| StackError::ConfigRead {
            path: std::path::PathBuf::from("stdin"),
            source,
        })?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "YES" | "Yes"))
}
