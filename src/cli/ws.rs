use crate::config::Config;
use crate::error::{Result, StackError};
use crate::fs_util::home_dir;
use clap::{Args, Subcommand};
use serde_json::Value;

use super::core::{
    CliKey, CliMethod, OutputFormat, daemon_base_url, daemon_request, open_cli_key, print_json,
};

#[derive(Debug, Subcommand)]
pub enum WsCommand {
    /// List live WebSocket connections.
    Connections,
    /// List unique subscribed session IDs.
    Sessions,
    /// Disconnect live WebSocket clients by connection or session.
    Disconnect(WsDisconnectArgs),
}

#[derive(Debug, Args)]
pub struct WsDisconnectArgs {
    /// Disconnect these WebSocket connection IDs.
    #[arg(long, conflicts_with = "session_id")]
    connection_id: Vec<String>,
    /// Disconnect every WebSocket subscribed to these session IDs.
    #[arg(long, conflicts_with = "connection_id")]
    session_id: Vec<String>,
}

pub(super) fn run_ws_command(command: WsCommand, output: OutputFormat) -> Result<()> {
    let home = home_dir()?;
    let config = Config::load_from_default_path()?;
    let key_kind = match command {
        WsCommand::Connections | WsCommand::Sessions => CliKey::Session,
        WsCommand::Disconnect(_) => CliKey::Admin,
    };
    let key = open_cli_key(&config, &home, key_kind)?;
    let base_url = daemon_base_url(config.api.public_url.as_deref(), &config.api.bind)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|source| StackError::ServeIo { source })?;
    runtime.block_on(async move {
        match command {
            WsCommand::Connections => {
                let body =
                    daemon_request(&base_url, CliMethod::Get, "/v1/ws/connections", &key, None)
                        .await?;
                let data = body.get("data").unwrap_or(&body);
                if output.is_json() {
                    print_json(data)?;
                } else {
                    print_connections(data);
                }
            }
            WsCommand::Sessions => {
                let body = daemon_request(&base_url, CliMethod::Get, "/v1/ws/sessions", &key, None)
                    .await?;
                let data = body.get("data").unwrap_or(&body);
                if output.is_json() {
                    print_json(data)?;
                } else {
                    print_sessions(data);
                }
            }
            WsCommand::Disconnect(args) => {
                if args.connection_id.is_empty() && args.session_id.is_empty() {
                    return Err(StackError::MissingField {
                        field: "--connection-id or --session-id",
                    });
                }
                let (path, body) = if !args.connection_id.is_empty() {
                    (
                        "/v1/ws/connections/disconnect",
                        serde_json::json!({"connection_ids": args.connection_id, "reason": "operator-request"}),
                    )
                } else {
                    (
                        "/v1/ws/sessions/disconnect",
                        serde_json::json!({"session_ids": args.session_id, "reason": "operator-request"}),
                    )
                };
                let body = daemon_request(&base_url, CliMethod::Post, path, &key, Some(&body))
                    .await?;
                let requested = body
                    .get("data")
                    .and_then(|data| data.get("requested"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                if output.is_json() {
                    print_json(body.get("data").unwrap_or(&body))?;
                } else {
                    println!("disconnect_requested: {requested}");
                }
            }
        }
        Ok(())
    })
}

fn print_connections(data: &Value) {
    let Some(connections) = data.get("connections").and_then(Value::as_array) else {
        println!("{data}");
        return;
    };
    if connections.is_empty() {
        println!("connections: (none)");
        return;
    }
    for connection in connections {
        let id = connection
            .get("connection_id")
            .and_then(Value::as_str)
            .unwrap_or("");
        let topics = connection
            .get("topics")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .unwrap_or_default();
        let origin_kind = connection
            .get("origin")
            .and_then(|origin| origin.get("origin_kind"))
            .and_then(Value::as_str)
            .unwrap_or("");
        println!("{id} origin={origin_kind} topics={topics}");
    }
}

fn print_sessions(data: &Value) {
    let Some(sessions) = data.get("sessions").and_then(Value::as_array) else {
        println!("{data}");
        return;
    };
    if sessions.is_empty() {
        println!("sessions: (none)");
        return;
    }
    for session in sessions {
        let id = session
            .get("session_id")
            .and_then(Value::as_str)
            .unwrap_or("");
        let count = session
            .get("connection_count")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        println!("{id} connections={count}");
    }
}
