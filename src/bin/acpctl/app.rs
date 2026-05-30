//! `acpctl` command dispatch.

use std::io::Read;
use std::process::ExitCode;

use base64::Engine;
use clap::Parser;
use serde_json::Value;

use crate::cli_defs::{
    Cli, Command, CommandCommand, ConfigCommand, DepsCommand, LogsCommand, McpCommand,
    PermissionsCommand, SecurityCommand, WorkspaceCommand, WsCommand,
};
use crate::client::request;
use crate::formatters::{
    format_command, format_command_output, format_command_submitted, format_commands_list,
    format_config_export, format_deps, format_file_mutation, format_files_list, format_logs,
    format_permissions, format_security, format_status, format_ws_connections, format_ws_sessions,
    print_response, write_workspace_read,
};
use crate::helpers::{build_logs_path, resolve_socket_path, url_encode};

pub(crate) async fn run_cli() -> ExitCode {
    let cli = Cli::parse();
    let socket = match cli.socket.clone().or_else(resolve_socket_path) {
        Some(path) => path,
        None => {
            eprintln!("acpctl: could not resolve socket path (set HOME or pass --socket)");
            return ExitCode::from(2);
        }
    };
    match run(cli, &socket).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("acpctl: {err}");
            ExitCode::from(1)
        }
    }
}

async fn run(cli: Cli, socket: &std::path::Path) -> Result<(), String> {
    let json_mode = cli.json;
    match cli.command {
        Command::Status => {
            let resp = request(socket, "GET", "/v1/status", &[], None).await?;
            print_response(&resp, json_mode, format_status)
        }
        Command::Security {
            action: SecurityCommand::Check,
        } => {
            let resp = request(socket, "GET", "/v1/security/check", &[], None).await?;
            print_response(&resp, json_mode, format_security)
        }
        Command::Deps {
            action: DepsCommand::Check,
        } => {
            let resp = request(
                socket,
                "POST",
                "/v1/deps/check",
                &[("content-type", "application/json")],
                Some(b"{}".to_vec()),
            )
            .await?;
            print_response(&resp, json_mode, format_deps)
        }
        Command::Logs {
            action: LogsCommand::Query(args),
        } => {
            let path = build_logs_path(&args)?;
            let resp = request(socket, "GET", &path, &[], None).await?;
            print_response(&resp, json_mode, format_logs)
        }
        Command::Workspace {
            action: WorkspaceCommand::List { path },
        } => {
            let query = format!("/v1/files?path={}", url_encode(&path));
            let resp = request(socket, "GET", &query, &[], None).await?;
            print_response(&resp, json_mode, format_files_list)
        }
        Command::Workspace {
            action: WorkspaceCommand::Read { path },
        } => {
            let query = format!("/v1/files/content?path={}", url_encode(&path));
            let resp = request(socket, "GET", &query, &[], None).await?;
            // `workspace read` writes the *content* to stdout; route it
            // through a path that propagates partial-write / broken-pipe
            // errors as a non-zero exit instead of swallowing them inside a
            // formatter.
            write_workspace_read(&resp, json_mode)
        }
        Command::Workspace {
            action: WorkspaceCommand::Write { path },
        } => {
            let mut bytes = Vec::new();
            std::io::stdin()
                .read_to_end(&mut bytes)
                .map_err(|e| format!("read stdin: {e}"))?;
            let (encoding, content) = match std::str::from_utf8(&bytes) {
                Ok(text) => ("utf8", text.to_owned()),
                Err(_) => (
                    "base64",
                    base64::engine::general_purpose::STANDARD.encode(&bytes),
                ),
            };
            let body = serde_json::json!({
                "path": path,
                "encoding": encoding,
                "content": content,
            })
            .to_string();
            let resp = request(
                socket,
                "PUT",
                "/v1/files/content",
                &[("content-type", "application/json")],
                Some(body.into_bytes()),
            )
            .await?;
            print_response(&resp, json_mode, format_file_mutation)
        }
        Command::Command {
            action:
                CommandCommand::Run {
                    command,
                    cwd,
                    timeout,
                },
        } => {
            let mut body = serde_json::Map::new();
            body.insert("command".to_owned(), Value::String(command));
            if let Some(cwd) = cwd {
                body.insert("cwd".to_owned(), Value::String(cwd));
            }
            if let Some(timeout) = timeout {
                body.insert("timeout".to_owned(), Value::String(timeout));
            }
            let body_text = Value::Object(body).to_string();
            let resp = request(
                socket,
                "POST",
                "/v1/commands",
                &[("content-type", "application/json")],
                Some(body_text.into_bytes()),
            )
            .await?;
            print_response(&resp, json_mode, format_command_submitted)
        }
        Command::Command {
            action: CommandCommand::List { limit },
        } => {
            let path = format!("/v1/commands?limit={limit}");
            let resp = request(socket, "GET", &path, &[], None).await?;
            print_response(&resp, json_mode, format_commands_list)
        }
        Command::Command {
            action: CommandCommand::Get { id },
        } => {
            let path = format!("/v1/commands/{}", url_encode(&id));
            let resp = request(socket, "GET", &path, &[], None).await?;
            print_response(&resp, json_mode, format_command)
        }
        Command::Command {
            action:
                CommandCommand::Output {
                    id,
                    limit,
                    after,
                    order,
                },
        } => {
            let mut path = format!(
                "/v1/commands/{}/output?limit={limit}&order={}",
                url_encode(&id),
                url_encode(&order)
            );
            if let Some(after) = after {
                path.push_str("&after=");
                path.push_str(&url_encode(&after));
            }
            let resp = request(socket, "GET", &path, &[], None).await?;
            print_response(&resp, json_mode, format_command_output)
        }
        Command::Command {
            action: CommandCommand::Cancel { id },
        } => {
            let path = format!("/v1/commands/{}/cancel", url_encode(&id));
            let resp = request(socket, "POST", &path, &[], None).await?;
            print_response(&resp, json_mode, format_command)
        }
        Command::Config {
            action: ConfigCommand::Export,
        } => {
            let resp = request(socket, "GET", "/v1/config/export", &[], None).await?;
            print_response(&resp, json_mode, format_config_export)
        }
        Command::Permissions {
            action: PermissionsCommand::Pending { limit },
        } => {
            let path = format!("/v1/permissions/pending?limit={limit}");
            let resp = request(socket, "GET", &path, &[], None).await?;
            print_response(&resp, json_mode, format_permissions)
        }
        Command::Ws {
            action: WsCommand::Connections,
        } => {
            let resp = request(socket, "GET", "/v1/ws/connections", &[], None).await?;
            print_response(&resp, json_mode, format_ws_connections)
        }
        Command::Ws {
            action: WsCommand::Sessions,
        } => {
            let resp = request(socket, "GET", "/v1/ws/sessions", &[], None).await?;
            print_response(&resp, json_mode, format_ws_sessions)
        }
        Command::Mcp {
            action: McpCommand::Serve(args),
        } => crate::mcp::run_serve(socket, args).await,
    }
}
