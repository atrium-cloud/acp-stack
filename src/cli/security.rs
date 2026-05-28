use crate::config::Config;
use crate::error::{Result, StackError};
use crate::fs_util::home_dir;
use clap::{Args, Subcommand};
use serde_json::Value;

use super::core::{CliKey, CliMethod, daemon_base_url, daemon_request, open_cli_key};

const DEFAULT_HISTORY_LIMIT: u32 = 20;
const MAX_HISTORY_LIMIT: u32 = 500;

#[derive(Debug, Subcommand)]
pub enum SecurityCommand {
    /// Print findings from the runtime security self-check (also recorded to history).
    Check,
    /// List previously recorded self-check runs.
    History(SecurityHistoryArgs),
    /// Show a single recorded self-check run with its findings.
    Show(SecurityShowArgs),
}

#[derive(Debug, Args)]
pub struct SecurityHistoryArgs {
    /// Maximum number of rows to print. Newest first.
    #[arg(long, default_value_t = DEFAULT_HISTORY_LIMIT)]
    limit: u32,
    /// Continue after a previous page's last run id.
    #[arg(long)]
    after: Option<String>,
    /// Print the raw JSON response instead of the operator table.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
pub struct SecurityShowArgs {
    /// Run id from `acps security history` or the `run_id` field of `acps security check`.
    run_id: String,
    /// Print the raw JSON response instead of the formatted view.
    #[arg(long)]
    json: bool,
}

pub(super) fn run_security_command(command: SecurityCommand) -> Result<()> {
    let home = home_dir()?;
    let config = Config::load_from_default_path()?;
    let admin_key = open_cli_key(&config, &home, CliKey::Admin)?;
    let base_url = daemon_base_url(config.api.public_url.as_deref(), &config.api.bind)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|source| StackError::ServeIo { source })?;
    runtime.block_on(async move {
        match command {
            SecurityCommand::Check => {
                let body = daemon_request(
                    &base_url,
                    CliMethod::Get,
                    "/v1/security/check",
                    &admin_key,
                    None,
                )
                .await?;
                if let Some(data) = body.get("data") {
                    format_security(data);
                } else {
                    println!("{body}");
                }
                Ok(())
            }
            SecurityCommand::History(args) => run_history(&base_url, &admin_key, args).await,
            SecurityCommand::Show(args) => run_show(&base_url, &admin_key, args).await,
        }
    })
}

async fn run_history(base_url: &str, admin_key: &str, args: SecurityHistoryArgs) -> Result<()> {
    if args.limit == 0 || args.limit > MAX_HISTORY_LIMIT {
        return Err(StackError::InvalidParam {
            field: "limit",
            reason: format!("limit must be 1..={MAX_HISTORY_LIMIT}, got {}", args.limit),
        });
    }
    let mut path = format!("/v1/security/history?limit={}", args.limit);
    if let Some(after) = args.after.as_deref()
        && !after.is_empty()
    {
        validate_run_id(after, "after")?;
        path.push_str("&after=");
        path.push_str(after);
    }
    let body = daemon_request(base_url, CliMethod::Get, &path, admin_key, None).await?;
    let data = body.get("data").unwrap_or(&body);
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(data).unwrap_or_else(|_| data.to_string())
        );
        return Ok(());
    }
    print_history_table(data);
    Ok(())
}

async fn run_show(base_url: &str, admin_key: &str, args: SecurityShowArgs) -> Result<()> {
    validate_run_id(&args.run_id, "run_id")?;
    let path = format!("/v1/security/history/{}", args.run_id);
    let body = daemon_request(base_url, CliMethod::Get, &path, admin_key, None).await?;
    let data = body.get("data").unwrap_or(&body);
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(data).unwrap_or_else(|_| data.to_string())
        );
        return Ok(());
    }
    format_show(data);
    Ok(())
}

fn format_security(data: &Value) {
    let ok = data.get("ok").and_then(Value::as_bool).unwrap_or(false);
    let count = data
        .get("auth_failure_count")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    if let Some(run_id) = data.get("run_id").and_then(Value::as_str) {
        println!("run_id: {run_id}");
    }
    println!("ok: {ok}");
    println!("auth_failures_total: {count}");
    if let Some(findings) = data.get("findings").and_then(Value::as_array) {
        print_findings(findings);
    }
}

fn format_show(data: &Value) {
    if let Some(run) = data.get("run") {
        let id = run.get("id").and_then(Value::as_str).unwrap_or("");
        let started = run.get("started_at").and_then(Value::as_str).unwrap_or("");
        let finished = run.get("finished_at").and_then(Value::as_str).unwrap_or("");
        let status = run.get("status").and_then(Value::as_str).unwrap_or("");
        let ok = run.get("ok").and_then(Value::as_bool).unwrap_or(false);
        let critical = run
            .get("critical_count")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let warning = run
            .get("warning_count")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let auth_failures = run
            .get("auth_failure_count")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        println!("run_id: {id}");
        println!("started_at: {started}");
        println!("finished_at: {finished}");
        println!("status: {status} (ok: {ok})");
        println!("critical: {critical}");
        println!("warning: {warning}");
        println!("auth_failures_total: {auth_failures}");
    }
    if let Some(findings) = data.get("findings").and_then(Value::as_array) {
        print_findings(findings);
    }
}

fn print_findings(findings: &[Value]) {
    if findings.is_empty() {
        println!("findings: (none)");
        return;
    }
    println!("findings:");
    for finding in findings {
        let severity = finding
            .get("severity")
            .and_then(Value::as_str)
            .unwrap_or("");
        let code = finding.get("code").and_then(Value::as_str).unwrap_or("");
        let message = finding.get("message").and_then(Value::as_str).unwrap_or("");
        println!("- {severity} {code}: {message}");
        if let Some(remediation) = finding.get("remediation").and_then(Value::as_str)
            && !remediation.is_empty()
        {
            println!("    hint: {remediation}");
        }
        if let Some(details) = finding.get("details")
            && !details.is_null()
        {
            let rendered = serde_json::to_string(details).unwrap_or_else(|_| details.to_string());
            println!("    details: {rendered}");
        }
    }
}

/// Reject any run id that would need URL encoding. The runtime emits ids of
/// the form `srun_<digits>_<digits>_<digits>` so accepting `[A-Za-z0-9_]+`
/// covers every legitimate value and keeps the URL builder simple. A weird
/// value (spaces, slashes, query chars) is almost certainly a CLI typo, so
/// failing here gives a clearer error than a 404 from the daemon.
fn validate_run_id(value: &str, field: &'static str) -> Result<()> {
    if value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        Ok(())
    } else {
        Err(StackError::InvalidParam {
            field,
            reason: format!(
                "expected an alphanumeric run id (letters, digits, underscores), got {value:?}"
            ),
        })
    }
}

fn print_history_table(data: &Value) {
    let runs = data
        .get("runs")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    if runs.is_empty() {
        println!("no security self-check runs recorded");
        return;
    }
    let header = format!(
        "{id:<40}  {started:<32}  {status:<10}  {ok:<3}  {crit:>5}  {warn:>5}  {auth:>5}",
        id = "id",
        started = "started_at",
        status = "status",
        ok = "ok",
        crit = "crit",
        warn = "warn",
        auth = "auth",
    );
    println!("{header}");
    for run in runs {
        let id = run.get("id").and_then(Value::as_str).unwrap_or("");
        let started = run.get("started_at").and_then(Value::as_str).unwrap_or("");
        let status = run.get("status").and_then(Value::as_str).unwrap_or("");
        let ok = if run.get("ok").and_then(Value::as_bool).unwrap_or(false) {
            "y"
        } else {
            "n"
        };
        let critical = run
            .get("critical_count")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let warning = run
            .get("warning_count")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let auth = run
            .get("auth_failure_count")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        println!(
            "{id:<40}  {started:<32}  {status:<10}  {ok:<3}  {critical:>5}  {warning:>5}  {auth:>5}",
        );
    }
    if let Some(cursor) = data.get("next_cursor").and_then(Value::as_str) {
        println!();
        println!("next page: --after {cursor}");
    }
}
