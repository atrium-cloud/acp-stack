use crate::config::Config;
use crate::error::{Result, StackError};
use clap::{Args, Subcommand};

use super::core::{CliMethod, OutputFormat, local_daemon_request, print_json};

#[derive(Debug, Subcommand)]
pub enum MetricsCommand {
    /// Print the metrics summary (counts, durations, percentiles).
    Summary(MetricsSummaryArgs),
}

#[derive(Debug, Args)]
pub struct MetricsSummaryArgs {
    /// Window start. Accepts `1h`/`30m`/`2d` or an RFC3339 timestamp.
    /// Defaults to 24h ago.
    #[arg(long)]
    since: Option<String>,
    /// Window end. Same format as `--since`. Defaults to now.
    #[arg(long)]
    until: Option<String>,
}

pub(super) fn run_metrics_command(command: MetricsCommand, output: OutputFormat) -> Result<()> {
    let config = Config::load_from_default_path()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|source| StackError::ServeIo { source })?;
    runtime.block_on(async move {
        match command {
            MetricsCommand::Summary(args) => {
                let mut query = String::new();
                if let Some(since) = &args.since {
                    query.push_str(&format!("since={}", encode_query_value(since)));
                }
                if let Some(until) = &args.until {
                    if !query.is_empty() {
                        query.push('&');
                    }
                    query.push_str(&format!("until={}", encode_query_value(until)));
                }
                let path = if query.is_empty() {
                    "/v1/metrics/summary".to_owned()
                } else {
                    format!("/v1/metrics/summary?{query}")
                };
                let body = local_daemon_request(&config, CliMethod::Get, &path, None).await?;
                if let Some(data) = body.get("data") {
                    if output.is_json() {
                        print_json(data)?;
                    } else {
                        let rendered =
                            serde_json::to_string_pretty(data).unwrap_or_else(|_| data.to_string());
                        println!("{rendered}");
                    }
                } else {
                    println!("{body}");
                }
                Ok(())
            }
        }
    })
}

/// Minimal percent-encoder for query-string values. Encodes the small set of
/// characters that can appear in our metrics bounds (`:` and `+` from RFC3339,
/// `&` defensively). Anything outside the safe set turns into `%XX`.
fn encode_query_value(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        let safe = matches!(byte,
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~'
        );
        if safe {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}
