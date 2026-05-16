use std::collections::HashMap;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

pub(crate) struct HttpResponse {
    pub(crate) status: u16,
    pub(crate) body: Vec<u8>,
}

pub(crate) async fn request(
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
