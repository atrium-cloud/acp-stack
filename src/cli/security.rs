use crate::config::Config;
use crate::error::{Result, StackError};
use crate::fs_util::home_dir;
use clap::Subcommand;
use serde_json::Value;

use super::core::{CliKey, CliMethod, daemon_base_url, daemon_request, open_cli_key};

#[derive(Debug, Subcommand)]
pub enum SecurityCommand {
    /// Print findings from the runtime security self-check.
    Check,
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
        }
    })
}

fn format_security(data: &Value) {
    let ok = data.get("ok").and_then(Value::as_bool).unwrap_or(false);
    let count = data
        .get("auth_failure_count")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    println!("ok: {ok}");
    println!("auth_failures_total: {count}");
    if let Some(findings) = data.get("findings").and_then(Value::as_array) {
        if findings.is_empty() {
            println!("findings: (none)");
        } else {
            println!("findings:");
            for finding in findings {
                let severity = finding
                    .get("severity")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let code = finding.get("code").and_then(Value::as_str).unwrap_or("");
                let message = finding.get("message").and_then(Value::as_str).unwrap_or("");
                println!("- {severity} {code}: {message}");
            }
        }
    }
}
