//! `acps installer` subcommands: operator-facing view of `installer_runs`.
//!
//! The installer module (`src/runtime/install/agent_installer.rs`) writes one row per
//! step into SQLite each time an install runs. Until now the only consumers
//! were the durable-event log and the HTTP `POST /v1/agent/install` response.
//! `acps installer history` surfaces those rows to the operator without going
//! through the daemon, so a failed install can be diagnosed even when the
//! daemon hasn't been started yet.

use clap::{Args, Subcommand};

use crate::error::{Result, StackError};
use crate::fs_util::{create_dir_owner_only, home_dir, parent_dir, pre_create_owner_only};
use crate::state::{InstallerRun, StateStore, default_state_path};

use super::core::{OutputFormat, print_json};

const DEFAULT_HISTORY_LIMIT: u32 = 20;
const MAX_HISTORY_LIMIT: u32 = 500;

#[derive(Debug, Subcommand)]
pub enum InstallerCommand {
    /// Show recent installer steps with status, duration, and recorded version.
    History(InstallerHistoryArgs),
}

#[derive(Debug, Args)]
pub struct InstallerHistoryArgs {
    /// Filter rows to one configured agent id.
    #[arg(long)]
    agent: Option<String>,
    /// Maximum number of rows to print. Newest first.
    #[arg(long, default_value_t = DEFAULT_HISTORY_LIMIT)]
    limit: u32,
}

pub(super) fn run_installer_command(command: InstallerCommand, output: OutputFormat) -> Result<()> {
    match command {
        InstallerCommand::History(args) => run_installer_history(args, output),
    }
}

fn run_installer_history(args: InstallerHistoryArgs, output: OutputFormat) -> Result<()> {
    if args.limit == 0 || args.limit > MAX_HISTORY_LIMIT {
        return Err(StackError::InvalidParam {
            field: "limit",
            reason: format!("limit must be 1..={MAX_HISTORY_LIMIT}, got {}", args.limit),
        });
    }
    let home = home_dir()?;
    let state_path = default_state_path(&home);
    let state_dir = parent_dir(&state_path)?;
    create_dir_owner_only(state_dir)?;
    pre_create_owner_only(&state_path)?;
    let store = StateStore::open(&state_path)?;
    store.migrate()?;

    let rows = store.query_installer_runs_filtered(args.agent.as_deref(), args.limit)?;
    if output.is_json() {
        let rows_json = rows
            .iter()
            .map(|row| {
                serde_json::json!({
                    "id": &row.id,
                    "agent_id": &row.agent_id,
                    "started_at": &row.started_at,
                    "finished_at": &row.finished_at,
                    "status": &row.status,
                    "exit_status": row.exit_status,
                    "step": &row.step,
                    "version": &row.version,
                    "log_dir": &row.log_dir,
                    "apply_run_id": &row.apply_run_id,
                    "duration_ms": duration_ms(row),
                })
            })
            .collect::<Vec<_>>();
        print_json(&serde_json::json!({ "runs": rows_json }))?;
        return Ok(());
    }
    if rows.is_empty() {
        if let Some(agent) = args.agent.as_deref() {
            println!("no installer runs recorded for agent `{agent}`");
        } else {
            println!("no installer runs recorded");
        }
        return Ok(());
    }
    print_history_table(&rows);
    Ok(())
}

/// Render the installer-history table. Columns sized to common content
/// (started_at is the RFC3339 nano timestamp; durations and exit codes fit
/// in single digits in practice). Truncation is preferred over wrapping so
/// the output stays grep-able.
fn print_history_table(rows: &[InstallerRun]) {
    let header = format!(
        "{started:<32}  {agent:<14}  {step:<8}  {status:<10}  {duration:>8}  {exit:>5}  {version}",
        started = "started_at",
        agent = "agent",
        step = "step",
        status = "status",
        duration = "duration",
        exit = "exit",
        version = "version",
    );
    println!("{header}");
    for row in rows {
        let duration = format_duration_ms(duration_ms(row));
        let exit = row
            .exit_status
            .map(|value| value.to_string())
            .unwrap_or_else(|| "-".to_owned());
        let version = row.version.as_deref().unwrap_or("-");
        let agent = row.agent_id.as_deref().unwrap_or("-");
        println!(
            "{started:<32}  {agent:<14}  {step:<8}  {status:<10}  {duration:>8}  {exit:>5}  {version}",
            started = row.started_at,
            agent = agent,
            step = row.step,
            status = row.status,
            duration = duration,
            exit = exit,
            version = version,
        );
        // log_dir is rendered as a continuation line because the full path
        // would otherwise blow out the fixed-width table. The leading
        // indentation matches the data columns so a grep on `log_dir:` still
        // picks it up. Rows that never wrote logs (config_error, legacy)
        // suppress the line entirely.
        if let Some(dir) = row.log_dir.as_deref() {
            println!("  log_dir: {dir}");
        }
    }
}

/// Compute the row's elapsed time as `finished_at - started_at`. Returns
/// `None` when finished_at is unset (e.g. a config_error row that never ran).
fn duration_ms(row: &InstallerRun) -> Option<i64> {
    let started = chrono::DateTime::parse_from_rfc3339(&row.started_at).ok()?;
    let finished = chrono::DateTime::parse_from_rfc3339(row.finished_at.as_deref()?).ok()?;
    Some((finished - started).num_milliseconds())
}

fn format_duration_ms(value: Option<i64>) -> String {
    match value {
        Some(ms) if ms < 1_000 => format!("{ms}ms"),
        Some(ms) => format!("{}s", ms / 1_000),
        None => "-".to_owned(),
    }
}
