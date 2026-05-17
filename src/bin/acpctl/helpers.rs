use std::path::PathBuf;

use chrono::SecondsFormat;

use crate::cli_defs::LogsQueryArgs;
use acp_stack::time_util::parse_duration_suffix;

/// Resolve the socket path the daemon is listening on. Precedence:
/// 1. `[acpctl] socket_path` in `~/.config/acp-stack/acp-stack.toml`, if the
///    file is readable and well-formed — this keeps a CLI invocation in sync
///    with a TOML override an operator already configured.
/// 2. The documented default `~/.local/share/acp-stack/acpctl.sock`.
///
/// Config-read failures are non-fatal: they fall through to the default path
/// so `acpctl` keeps working when the config is absent or being edited.
pub(crate) fn resolve_socket_path() -> Option<PathBuf> {
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

pub(crate) fn build_logs_path(args: &LogsQueryArgs) -> Result<String, String> {
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
pub(crate) fn resolve_time_bound(raw: &str, field: &str) -> Result<String, String> {
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

pub(crate) fn url_encode(input: &str) -> String {
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
