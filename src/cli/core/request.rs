use super::*;

// CONSTANTS

/// Paths whose label is the path itself. Matched before [`PATTERN_PATH_LABELS`].
const EXACT_PATH_LABELS: &[&str] = &[
    "/v1/status",
    "/v1/status/agent",
    "/v1/agent/status",
    "/v1/status/connections",
    "/v1/health/live",
    "/v1/health/ready",
    "/v1/config/export",
    "/v1/auth/session-key/regenerate",
    "/v1/auth/local-session-access",
    "/v1/config/validate",
    "/v1/agent/capabilities",
    "/v1/array/status",
    "/v1/agent/install",
    "/v1/agent/start",
    "/v1/agent/stop",
    "/v1/agent/restart",
    "/v1/agent/restart-blockers",
    "/v1/agent/switch",
    "/v1/agent/skills",
    "/v1/agent/skills/catalog",
    "/v1/agent/skills/add",
    "/v1/agent/skills/remove",
    "/v1/agent/skills/source",
    "/v1/agent/skills/sources/add",
    "/v1/agent/skills/sources/remove",
    "/v1/agent/config/native/inspect",
    "/v1/agent/config/native/import",
    "/v1/logs/events",
    "/v1/logs/commands",
    "/v1/logs/permissions",
    "/v1/logs/security",
    "/v1/logs/sessions",
    "/v1/metrics/summary",
    "/v1/workspace",
    "/v1/files",
    "/v1/files/content",
    "/v1/files/upload",
    "/v1/files/download",
    "/v1/commands",
    "/v1/deps",
    "/v1/deps/check",
    "/v1/providers",
    "/v1/models",
    "/v1/permissions/pending",
    "/v1/security/check",
    "/v1/security/history",
    "/v1/ws/connections",
    "/v1/ws/sessions",
    "/v1/ws/connections/disconnect",
    "/v1/ws/sessions/disconnect",
    "/v1/sessions",
    "/v1/sessions/-/status",
];

/// Parameterized routes, matched in order — the per-collection catch-all MUST stay last within
/// its prefix group.
const PATTERN_PATH_LABELS: &[(&str, PathTail, &str)] = &[
    (
        "/v1/agent/config/native/import/",
        PathTail::Suffix("/cancel"),
        "/v1/agent/config/native/import/{operation_id}/cancel",
    ),
    (
        "/v1/agent/config/native/import/",
        PathTail::Any,
        "/v1/agent/config/native/import/{operation_id}",
    ),
    (
        "/v1/array/targets/",
        PathTail::Suffix("/capabilities"),
        "/v1/array/targets/{target_id}/capabilities",
    ),
    (
        "/v1/array/targets/",
        PathTail::Suffix("/install"),
        "/v1/array/targets/{target_id}/install",
    ),
    (
        "/v1/array/targets/",
        PathTail::Suffix("/start"),
        "/v1/array/targets/{target_id}/start",
    ),
    (
        "/v1/array/targets/",
        PathTail::Suffix("/stop"),
        "/v1/array/targets/{target_id}/stop",
    ),
    (
        "/v1/array/targets/",
        PathTail::Suffix("/restart"),
        "/v1/array/targets/{target_id}/restart",
    ),
    (
        "/v1/commands/",
        PathTail::Suffix("/output"),
        "/v1/commands/{id}/output",
    ),
    (
        "/v1/commands/",
        PathTail::Suffix("/cancel"),
        "/v1/commands/{id}/cancel",
    ),
    ("/v1/commands/", PathTail::Any, "/v1/commands/{id}"),
    (
        "/v1/permissions/",
        PathTail::Suffix("/approve"),
        "/v1/permissions/{id}/approve",
    ),
    (
        "/v1/permissions/",
        PathTail::Suffix("/deny"),
        "/v1/permissions/{id}/deny",
    ),
    (
        "/v1/security/history/",
        PathTail::Any,
        "/v1/security/history/{run_id}",
    ),
    (
        "/v1/sessions/",
        PathTail::Suffix("/prompt"),
        "/v1/sessions/{id}/prompt",
    ),
    (
        "/v1/sessions/",
        PathTail::Suffix("/cancel"),
        "/v1/sessions/{id}/cancel",
    ),
    (
        "/v1/sessions/",
        PathTail::Suffix("/load"),
        "/v1/sessions/{id}/load",
    ),
    (
        "/v1/sessions/",
        PathTail::Suffix("/resume"),
        "/v1/sessions/{id}/resume",
    ),
    (
        "/v1/sessions/",
        PathTail::Suffix("/fork"),
        "/v1/sessions/{id}/fork",
    ),
    (
        "/v1/sessions/",
        PathTail::Contains("/prompts/"),
        "/v1/sessions/{id}/prompts/{prompt_id}",
    ),
    (
        "/v1/sessions/",
        PathTail::Suffix("/events"),
        "/v1/sessions/{id}/events",
    ),
    (
        "/v1/sessions/",
        PathTail::Suffix("/snapshot"),
        "/v1/sessions/{id}/snapshot",
    ),
    ("/v1/sessions/", PathTail::Any, "/v1/sessions/{id}"),
];

/// Fallback for paths not listed above.
const FALLBACK_PATH_LABEL: &str = "/v1/agent";

#[derive(Debug, Clone, Copy)]
pub(crate) enum CliMethod {
    Get,
    Post,
    Put,
    Delete,
}

impl CliMethod {
    fn as_str(self) -> &'static str {
        match self {
            CliMethod::Get => "GET",
            CliMethod::Post => "POST",
            CliMethod::Put => "PUT",
            CliMethod::Delete => "DELETE",
        }
    }
}

/// Generalized daemon-RPC helper. Callers supply the explicit bearer key.
pub(crate) async fn daemon_request(
    base_url: &str,
    method: CliMethod,
    path: &str,
    key: &str,
    body: Option<&serde_json::Value>,
) -> Result<serde_json::Value> {
    let url = format!("{}{}", base_url.trim_end_matches('/'), path);
    // Bucketing to a static label keeps path params out of logged errors.
    let path_label: &'static str = static_path_label(path);
    let client = reqwest::Client::new();
    let request = match method {
        CliMethod::Get => client.get(&url),
        CliMethod::Post => client.post(&url),
        CliMethod::Put => client.put(&url),
        CliMethod::Delete => client.delete(&url),
    }
    .bearer_auth(key);
    let request = if let Some(body) = body {
        request.json(body)
    } else {
        request
    };
    let response = request
        .send()
        .await
        .map_err(|source| StackError::AgentApiRequest {
            path: path_label,
            source,
        })?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|source| StackError::AgentApiRequest {
            path: path_label,
            source,
        })?;
    if !status.is_success() {
        return Err(StackError::AgentApiStatus {
            path: path_label,
            status,
            body,
        });
    }
    serde_json::from_str(&body).map_err(|err| StackError::AgentInitializeFailed {
        reason: format!("daemon response was not JSON: {err}"),
    })
}

pub(crate) async fn local_daemon_request(
    config: &Config,
    method: CliMethod,
    path: &str,
    body: Option<&serde_json::Value>,
) -> Result<serde_json::Value> {
    let (status, body) = local_daemon_json_response(config, method, path, body).await?;
    if !status.is_success() {
        return Err(StackError::AgentApiStatus {
            path: static_path_label(path),
            status,
            body: body.to_string(),
        });
    }
    Ok(body)
}

pub(crate) async fn local_daemon_json_response(
    config: &Config,
    method: CliMethod,
    path: &str,
    body: Option<&serde_json::Value>,
) -> Result<(::http::StatusCode, serde_json::Value)> {
    let socket_path = local_socket_path(config)?;
    let body_bytes = body.map(serde_json::to_vec).transpose().map_err(|source| {
        StackError::AgentInitializeFailed {
            reason: format!("serialize local daemon request body: {source}"),
        }
    })?;
    let response = local_http_request(&socket_path, method.as_str(), path, body_bytes).await?;
    let status = ::http::StatusCode::from_u16(response.status)
        .unwrap_or(::http::StatusCode::INTERNAL_SERVER_ERROR);
    let body_text =
        String::from_utf8(response.body).map_err(|source| StackError::AgentInitializeFailed {
            reason: format!("local daemon response was not UTF-8: {source}"),
        })?;
    let body =
        serde_json::from_str(&body_text).map_err(|err| StackError::AgentInitializeFailed {
            reason: format!("local daemon response was not JSON: {err}"),
        })?;
    Ok((status, body))
}

/// Tail condition applied to a path that already matched a prefix.
#[derive(Debug, Clone, Copy)]
enum PathTail {
    Any,
    Suffix(&'static str),
    Contains(&'static str),
}

pub(crate) fn static_path_label(path: &str) -> &'static str {
    // Strip the query string so callers passing `?limit=` still resolve to the canonical label.
    let bare = path.split('?').next().unwrap_or(path);
    if let Some(label) = EXACT_PATH_LABELS.iter().find(|label| **label == bare) {
        return label;
    }
    for (prefix, tail, label) in PATTERN_PATH_LABELS {
        let tail_matches = match tail {
            PathTail::Any => true,
            PathTail::Suffix(suffix) => bare.ends_with(suffix),
            PathTail::Contains(needle) => bare.contains(needle),
        };
        if bare.starts_with(prefix) && tail_matches {
            return label;
        }
    }
    FALLBACK_PATH_LABEL
}

/// Percent-encode a URL path segment against the RFC 3986 unreserved set. ACP session and prompt
/// IDs are opaque, so an id containing `/` is a path-injection vector unless it is encoded here.
pub(crate) fn encode_path_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.as_bytes() {
        let b = *byte;
        let is_unreserved =
            b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.' || b == b'~';
        if is_unreserved {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub(crate) fn daemon_base_url(public_url: Option<&str>, bind: &str) -> Result<String> {
    if let Some(public_url) = public_url.filter(|value| !value.trim().is_empty()) {
        return Ok(public_url.trim_end_matches('/').to_owned());
    }
    let socket: SocketAddr = bind
        .parse()
        .map_err(|_| StackError::InvalidSocketAddress { field: "api.bind" })?;
    let host = match socket.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST).to_string(),
        IpAddr::V6(ip) if ip.is_unspecified() => format!("[{}]", Ipv6Addr::LOCALHOST),
        IpAddr::V4(ip) => ip.to_string(),
        IpAddr::V6(ip) => format!("[{ip}]"),
    };
    Ok(format!("http://{host}:{}", socket.port()))
}
