//! `acpctl` — local agent CLI for `acp-stack`.
//!
//! Speaks HTTP/1.1 over a Unix-domain socket against the daemon's local
//! listener (see `acp_stack::local_listener`). Maps each subcommand to one of
//! the 10 allowlisted local routes and prints a human-readable summary by
//! default; pass `--json` to emit the raw response envelope.
//!
//! Access control is filesystem-permission based: the daemon binds the socket
//! at mode 0600 inside a 0700 directory, so any process able to connect is
//! already running as the runtime user.

use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;

use acp_stack::time_util::parse_duration_suffix;
use base64::Engine;
use chrono::SecondsFormat;
use clap::{Args, Parser, Subcommand};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

#[derive(Parser, Debug)]
#[command(
    name = "acpctl",
    version,
    about = "Local agent control CLI for the acp-stack runtime."
)]
struct Cli {
    /// Override the Unix-domain socket path. Defaults to
    /// `~/.local/share/acp-stack/acpctl.sock`.
    #[arg(long, global = true)]
    socket: Option<PathBuf>,
    /// Emit the raw JSON response envelope rather than human-readable text.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
#[allow(clippy::enum_variant_names)]
enum Command {
    /// Print runtime status (schema version, latest event).
    Status,
    /// Run the daemon's security self-check.
    Security {
        #[command(subcommand)]
        action: SecurityCommand,
    },
    /// Inspect or refresh dependency status.
    Deps {
        #[command(subcommand)]
        action: DepsCommand,
    },
    /// Query recent log events.
    Logs {
        #[command(subcommand)]
        action: LogsCommand,
    },
    /// Workspace file operations.
    Workspace {
        #[command(subcommand)]
        action: WorkspaceCommand,
    },
    /// Run a mediated shell command through the command gateway.
    Command {
        #[command(subcommand)]
        action: CommandCommand,
    },
    /// Config-related actions.
    Config {
        #[command(subcommand)]
        action: ConfigCommand,
    },
    /// Permission queue introspection.
    Permissions {
        #[command(subcommand)]
        action: PermissionsCommand,
    },
}

#[derive(Subcommand, Debug)]
enum SecurityCommand {
    /// Print findings from the runtime security self-check.
    Check,
}

#[derive(Subcommand, Debug)]
enum DepsCommand {
    /// Run the dependency check and print the latest report.
    Check,
}

#[derive(Subcommand, Debug)]
enum LogsCommand {
    /// Query events between optional time bounds.
    Query(LogsQueryArgs),
}

#[derive(Args, Debug)]
struct LogsQueryArgs {
    /// Restrict to events on or after this time. Accepts duration suffixes
    /// (`30m`, `1h`, `2d`) or RFC3339 timestamps.
    #[arg(long)]
    since: Option<String>,
    /// Restrict to events strictly before this time.
    #[arg(long)]
    until: Option<String>,
    /// Filter by event kind. A trailing `.` matches as a prefix.
    #[arg(long)]
    kind: Option<String>,
    /// Filter by log level.
    #[arg(long)]
    level: Option<String>,
    /// Filter by session ID.
    #[arg(long)]
    session: Option<String>,
    /// Maximum number of rows to return.
    #[arg(long, default_value_t = 200)]
    limit: u32,
    /// Cursor for pagination; pass the last seen event id.
    #[arg(long)]
    after: Option<String>,
}

#[derive(Subcommand, Debug)]
enum WorkspaceCommand {
    /// List a directory inside the workspace root.
    List { path: String },
    /// Print the contents of a workspace file to stdout.
    Read { path: String },
    /// Write stdin to the workspace file at the given path (atomic).
    Write { path: String },
}

#[derive(Subcommand, Debug)]
enum CommandCommand {
    /// Submit a shell command to the command gateway.
    Run {
        command: String,
        /// Optional working directory; must remain inside the workspace root.
        #[arg(long)]
        cwd: Option<String>,
        /// Optional timeout, e.g. `30s`, `5m`.
        #[arg(long)]
        timeout: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum ConfigCommand {
    /// Print the canonical TOML config with secret references only.
    Export,
}

#[derive(Subcommand, Debug)]
enum PermissionsCommand {
    /// List pending permission requests.
    Pending {
        #[arg(long, default_value_t = 200)]
        limit: u32,
    },
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
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

/// Resolve the socket path the daemon is listening on. Precedence:
/// 1. `[acpctl] socket_path` in `~/.config/acp-stack/acp-stack.toml`, if the
///    file is readable and well-formed — this keeps a CLI invocation in sync
///    with a TOML override an operator already configured.
/// 2. The documented default `~/.local/share/acp-stack/acpctl.sock`.
///
/// Config-read failures are non-fatal: they fall through to the default path
/// so `acpctl` keeps working when the config is absent or being edited.
fn resolve_socket_path() -> Option<PathBuf> {
    if let Ok(path) = acp_stack::config::default_config_path() {
        if let Ok(config) = acp_stack::config::Config::load_from_path(&path) {
            if let Some(socket) = config.acpctl.socket_path.as_deref() {
                return Some(PathBuf::from(socket));
            }
        }
    }
    default_socket_path()
}

fn default_socket_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    if home.is_empty() {
        return None;
    }
    Some(PathBuf::from(home).join(".local/share/acp-stack/acpctl.sock"))
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
    }
}

fn build_logs_path(args: &LogsQueryArgs) -> Result<String, String> {
    let mut path = String::from("/v1/logs/events?");
    let mut sep = "";
    let mut push = |k: &str, v: &str| {
        path.push_str(sep);
        path.push_str(k);
        path.push('=');
        path.push_str(&url_encode(v));
        sep = "&";
    };
    push("limit", &args.limit.to_string());
    if let Some(v) = &args.since {
        push("since", &resolve_time_bound(v, "since")?);
    }
    if let Some(v) = &args.until {
        push("until", &resolve_time_bound(v, "until")?);
    }
    if let Some(v) = &args.kind {
        push("kind", v);
    }
    if let Some(v) = &args.level {
        push("level", v);
    }
    if let Some(v) = &args.session {
        push("session_id", v);
    }
    if let Some(v) = &args.after {
        push("after", v);
    }
    Ok(path)
}

/// Accept either a duration suffix (`30m`, `1h`, `2d`) or an RFC3339
/// timestamp and return an RFC3339 string the server can compare lexically.
/// The server's `/v1/logs/events` route does no duration parsing of its own,
/// so passing `30m` straight through would be compared character-by-character
/// against real timestamps and silently mis-filter.
fn resolve_time_bound(raw: &str, field: &str) -> Result<String, String> {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(raw) {
        return Ok(dt
            .with_timezone(&chrono::Utc)
            .to_rfc3339_opts(SecondsFormat::Nanos, true));
    }
    let duration = parse_duration_suffix(raw)
        .ok_or_else(|| format!("--{field} must be RFC3339 or a duration suffix (got {raw:?})"))?;
    let absolute = chrono::Utc::now() - duration;
    Ok(absolute.to_rfc3339_opts(SecondsFormat::Nanos, true))
}

fn url_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.as_bytes() {
        let c = *byte;
        if c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.' | b'~') {
            out.push(c as char);
        } else {
            out.push('%');
            out.push_str(&format!("{c:02X}"));
        }
    }
    out
}

struct HttpResponse {
    status: u16,
    body: Vec<u8>,
}

async fn request(
    socket: &std::path::Path,
    method: &str,
    path: &str,
    extra_headers: &[(&str, &str)],
    body: Option<Vec<u8>>,
) -> Result<HttpResponse, String> {
    let mut stream = UnixStream::connect(socket)
        .await
        .map_err(|e| format!("connect {}: {e}", socket.display()))?;
    let body_bytes = body.unwrap_or_default();
    let mut request =
        format!("{method} {path} HTTP/1.1\r\nHost: acpctl.local\r\nConnection: close\r\n");
    for (name, value) in extra_headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str(&format!("Content-Length: {}\r\n\r\n", body_bytes.len()));
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|e| format!("write request: {e}"))?;
    if !body_bytes.is_empty() {
        stream
            .write_all(&body_bytes)
            .await
            .map_err(|e| format!("write body: {e}"))?;
    }
    // Do NOT half-close the write side: hyper's HTTP/1.1 server may interpret
    // the FIN as a client cancellation and abandon the response. We send
    // `Connection: close`, so the server closes its side after writing the
    // response, which is enough to terminate `read_to_end`.
    let mut raw = Vec::with_capacity(4096);
    stream
        .read_to_end(&mut raw)
        .await
        .map_err(|e| format!("read response: {e}"))?;
    parse_response(&raw)
}

fn parse_response(raw: &[u8]) -> Result<HttpResponse, String> {
    let header_end = find_header_end(raw).ok_or("response missing CRLF CRLF terminator")?;
    let header_text = std::str::from_utf8(&raw[..header_end])
        .map_err(|_| "response headers are not UTF-8".to_owned())?;
    let mut lines = header_text.split("\r\n");
    let status_line = lines.next().ok_or("response missing status line")?;
    let mut parts = status_line.splitn(3, ' ');
    let _http = parts.next().ok_or("malformed status line")?;
    let status: u16 = parts
        .next()
        .ok_or("status code missing")?
        .parse()
        .map_err(|_| "status code is not numeric".to_owned())?;
    let mut headers: HashMap<String, String> = HashMap::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
        }
    }
    let body_start = header_end + 4;
    let body = match headers.get("content-length") {
        Some(len_text) => {
            let want: usize = len_text
                .parse()
                .map_err(|_| "Content-Length is not a number".to_owned())?;
            if raw.len() < body_start + want {
                return Err(format!(
                    "response truncated: Content-Length={want} but {} bytes available",
                    raw.len().saturating_sub(body_start)
                ));
            }
            raw[body_start..body_start + want].to_vec()
        }
        None => raw[body_start..].to_vec(),
    };
    Ok(HttpResponse { status, body })
}

fn find_header_end(raw: &[u8]) -> Option<usize> {
    raw.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Returns `Err` when the server responded with `{ok:false}` (envelope error)
/// or when the HTTP status is not 2xx. The caller maps that into a non-zero
/// process exit code so failing acpctl operations are observable from
/// scripts and agents.
fn print_response<F>(resp: &HttpResponse, json_mode: bool, formatter: F) -> Result<(), String>
where
    F: FnOnce(&Value),
{
    let body_text = std::str::from_utf8(&resp.body).unwrap_or("");
    let parsed: Option<Value> = serde_json::from_str(body_text).ok();
    if json_mode {
        match &parsed {
            Some(value) => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(value).unwrap_or_default()
                );
            }
            None => println!("{body_text}"),
        }
    }
    let envelope_ok = parsed
        .as_ref()
        .and_then(|v| v.get("ok"))
        .and_then(Value::as_bool);
    let server_ok = (200..300).contains(&resp.status) && envelope_ok != Some(false);
    if !server_ok {
        let (code, message) = match parsed.as_ref() {
            Some(value) => {
                let code = value
                    .get("error")
                    .and_then(|e| e.get("code"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let message = value
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("(no message)");
                (code.to_owned(), message.to_owned())
            }
            None => (
                "http_error".to_owned(),
                format!("non-2xx response: {body_text}"),
            ),
        };
        return Err(format!("HTTP {} {code}: {message}", resp.status));
    }
    if json_mode {
        return Ok(());
    }
    let Some(value) = parsed else {
        println!("{body_text}");
        return Ok(());
    };
    let data = value.get("data").unwrap_or(&value);
    formatter(data);
    Ok(())
}

fn format_status(data: &Value) {
    print_kv(data, &["schema_version", "latest_event"]);
    if let Some(version) = data
        .get("server")
        .and_then(|s| s.get("version"))
        .and_then(Value::as_str)
    {
        println!("version: {version}");
    }
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
                println!("  - [{severity}] {code}: {message}");
            }
        }
    }
}

fn format_deps(data: &Value) {
    println!("{}", serde_json::to_string_pretty(data).unwrap_or_default());
}

fn format_logs(data: &Value) {
    let Some(events) = data.get("events").and_then(Value::as_array) else {
        println!("(no events)");
        return;
    };
    if events.is_empty() {
        println!("(no events)");
        return;
    }
    for event in events {
        let created = event
            .get("created_at")
            .and_then(Value::as_str)
            .unwrap_or("");
        let level = event.get("level").and_then(Value::as_str).unwrap_or("");
        let source = event.get("source").and_then(Value::as_str).unwrap_or("");
        let kind = event.get("kind").and_then(Value::as_str).unwrap_or("");
        let message = event.get("message").and_then(Value::as_str).unwrap_or("");
        println!("{created} {level} {source} {kind} {message}");
    }
}

fn format_files_list(data: &Value) {
    let path = data.get("path").and_then(Value::as_str).unwrap_or("");
    println!("path: {path}");
    let Some(entries) = data.get("entries").and_then(Value::as_array) else {
        return;
    };
    for entry in entries {
        let name = entry.get("name").and_then(Value::as_str).unwrap_or("");
        let kind = entry.get("kind").and_then(Value::as_str).unwrap_or("");
        let size = entry
            .get("size")
            .and_then(Value::as_u64)
            .map(|s| s.to_string())
            .unwrap_or_else(|| "-".to_owned());
        println!("{kind:9} {size:>10} {name}");
    }
}

fn write_workspace_read(resp: &HttpResponse, json_mode: bool) -> Result<(), String> {
    let body_text = std::str::from_utf8(&resp.body).unwrap_or("");
    let value: Value =
        serde_json::from_str(body_text).map_err(|e| format!("response is not JSON: {e}"))?;
    if !(200..300).contains(&resp.status) || value.get("ok").and_then(Value::as_bool) == Some(false)
    {
        let code = value
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let message = value
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("(no message)");
        return Err(format!("HTTP {} {code}: {message}", resp.status));
    }
    if json_mode {
        println!(
            "{}",
            serde_json::to_string_pretty(&value).unwrap_or_default()
        );
        return Ok(());
    }
    let data = value.get("data").unwrap_or(&value);
    let encoding = data.get("encoding").and_then(Value::as_str).unwrap_or("");
    let content = data.get("content").and_then(Value::as_str).unwrap_or("");
    use std::io::Write;
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    if encoding == "base64" {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(content)
            .map_err(|e| format!("decode base64 content: {e}"))?;
        handle
            .write_all(&bytes)
            .map_err(|e| format!("write stdout: {e}"))?;
    } else {
        handle
            .write_all(content.as_bytes())
            .map_err(|e| format!("write stdout: {e}"))?;
    }
    handle.flush().map_err(|e| format!("flush stdout: {e}"))?;
    Ok(())
}

fn format_file_mutation(data: &Value) {
    print_kv(data, &["path", "size", "modified"]);
}

fn format_command(data: &Value) {
    // `/v1/commands` returns the row at submission time only (status is
    // typically `pending` or `running`); stdout is streamed via WebSocket on
    // the public API, not via this REST submit. Poll `/v1/commands/{id}` from
    // a follow-up call to observe completion.
    print_kv(data, &["id", "status", "command", "exit_status"]);
}

fn format_config_export(data: &Value) {
    if let Some(toml) = data.get("toml").and_then(Value::as_str) {
        print!("{toml}");
    } else {
        println!("{}", serde_json::to_string_pretty(data).unwrap_or_default());
    }
}

fn format_permissions(data: &Value) {
    let Some(perms) = data.get("permissions").and_then(Value::as_array) else {
        println!("(none)");
        return;
    };
    if perms.is_empty() {
        println!("(none)");
        return;
    }
    for perm in perms {
        let id = perm.get("id").and_then(Value::as_str).unwrap_or("");
        let source = perm.get("source").and_then(Value::as_str).unwrap_or("");
        let requester = perm.get("requester").and_then(Value::as_str).unwrap_or("");
        let created = perm.get("created_at").and_then(Value::as_str).unwrap_or("");
        println!("{created} {id} src={source} requester={requester}");
    }
}

fn print_kv(data: &Value, keys: &[&str]) {
    for key in keys {
        if let Some(value) = data.get(*key) {
            let rendered = match value {
                Value::String(s) => s.clone(),
                _ => value.to_string(),
            };
            println!("{key}: {rendered}");
        }
    }
}
