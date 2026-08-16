use super::*;

pub(crate) fn local_socket_path(config: &Config) -> Result<PathBuf> {
    if let Some(path) = config.local.socket_path.as_deref() {
        return Ok(PathBuf::from(path));
    }
    crate::local_listener::default_socket_path()
}

pub(crate) struct LocalHttpResponse {
    pub(crate) status: u16,
    pub(crate) body: Vec<u8>,
}

pub(crate) async fn local_http_request(
    socket: &Path,
    method: &str,
    path: &str,
    body: Option<Vec<u8>>,
) -> Result<LocalHttpResponse> {
    let mut stream = UnixStream::connect(socket)
        .await
        .map_err(|source| StackError::ServeIo { source })?;
    let body_bytes = body.unwrap_or_default();
    let mut request =
        format!("{method} {path} HTTP/1.1\r\nHost: acps.local\r\nConnection: close\r\n");
    if !body_bytes.is_empty() {
        request.push_str("Content-Type: application/json\r\n");
    }
    request.push_str(&format!("Content-Length: {}\r\n\r\n", body_bytes.len()));
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|source| StackError::ServeIo { source })?;
    if !body_bytes.is_empty() {
        stream
            .write_all(&body_bytes)
            .await
            .map_err(|source| StackError::ServeIo { source })?;
    }
    let mut raw = Vec::with_capacity(4096);
    stream
        .read_to_end(&mut raw)
        .await
        .map_err(|source| StackError::ServeIo { source })?;
    parse_local_http_response(&raw)
}

fn parse_local_http_response(raw: &[u8]) -> Result<LocalHttpResponse> {
    let header_end = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| StackError::AgentInitializeFailed {
            reason: "local daemon response missing header terminator".to_owned(),
        })?;
    let header_text = std::str::from_utf8(&raw[..header_end]).map_err(|source| {
        StackError::AgentInitializeFailed {
            reason: format!("local daemon response headers were not UTF-8: {source}"),
        }
    })?;
    let mut lines = header_text.split("\r\n");
    let status_line = lines
        .next()
        .ok_or_else(|| StackError::AgentInitializeFailed {
            reason: "local daemon response missing status line".to_owned(),
        })?;
    let status = status_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| StackError::AgentInitializeFailed {
            reason: "local daemon response status code missing".to_owned(),
        })?
        .parse::<u16>()
        .map_err(|source| StackError::AgentInitializeFailed {
            reason: format!("local daemon response status code was invalid: {source}"),
        })?;
    let mut content_length: Option<usize> = None;
    for line in lines {
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            content_length = Some(value.trim().parse::<usize>().map_err(|source| {
                StackError::AgentInitializeFailed {
                    reason: format!("local daemon response Content-Length was invalid: {source}"),
                }
            })?);
        }
    }
    let body_start = header_end + 4;
    let body = match content_length {
        Some(length) => {
            let end = body_start + length;
            if raw.len() < end {
                return Err(StackError::AgentInitializeFailed {
                    reason: format!(
                        "local daemon response truncated: Content-Length={length}, available={}",
                        raw.len().saturating_sub(body_start)
                    ),
                });
            }
            raw[body_start..end].to_vec()
        }
        None => raw[body_start..].to_vec(),
    };
    Ok(LocalHttpResponse { status, body })
}
