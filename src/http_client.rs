//! Single construction point for outbound HTTP clients. Fixture builds refuse every non-loopback
//! host unless the host is disposable, so the test suite can never egress from a developer
//! machine; release builds get plain builders.

// === CONSTANTS ===
/// Nothing listens here; routing a request through it fails at connect before any packet leaves
/// the loopback interface.
const DEAD_PROXY_URL: &str = "http://127.0.0.1:1";

pub fn client_builder() -> reqwest::ClientBuilder {
    let builder = reqwest::Client::builder();
    if egress_guard_active() {
        builder.proxy(loopback_only_proxy())
    } else {
        builder
    }
}

pub fn blocking_client_builder() -> reqwest::blocking::ClientBuilder {
    let builder = reqwest::blocking::Client::builder();
    if egress_guard_active() {
        builder.proxy(loopback_only_proxy())
    } else {
        builder
    }
}

#[cfg(feature = "test-fixtures")]
fn egress_guard_active() -> bool {
    crate::dev_gates::fixture_guards_active()
}

#[cfg(not(feature = "test-fixtures"))]
fn egress_guard_active() -> bool {
    false
}

/// Non-reqwest egress channels (the `acps logs --follow` websocket) ask here before dialing:
/// fixture builds refuse non-loopback URLs so a remote `api.public_url` cannot smuggle egress
/// past the proxy guard.
pub fn ensure_url_allowed(url_text: &str) -> crate::error::Result<()> {
    if !egress_guard_active() {
        return Ok(());
    }
    refuse_non_loopback(url_text)
}

/// Split from the env-dependent wrapper so unit tests exercise the refusal directly; CI exports
/// the disposable-host opt-out, so env-dependent assertions would behave differently per host.
#[cfg(feature = "test-fixtures")]
fn refuse_non_loopback(url_text: &str) -> crate::error::Result<()> {
    let url = reqwest::Url::parse(url_text).map_err(|_| {
        crate::error::StackError::FixtureEgressRefused {
            url: url_text.to_owned(),
        }
    })?;
    if url_is_loopback(&url) {
        return Ok(());
    }
    Err(crate::error::StackError::FixtureEgressRefused {
        url: url_text.to_owned(),
    })
}

#[cfg(not(feature = "test-fixtures"))]
fn refuse_non_loopback(_url_text: &str) -> crate::error::Result<()> {
    Ok(())
}

fn loopback_only_proxy() -> reqwest::Proxy {
    reqwest::Proxy::custom(|url: &reqwest::Url| {
        if url_is_loopback(url) {
            None
        } else {
            Some(DEAD_PROXY_URL)
        }
    })
}

fn url_is_loopback(url: &reqwest::Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    // IPv6 literals keep their brackets in `host_str`.
    let literal = host.trim_start_matches('[').trim_end_matches(']');
    match literal.parse::<std::net::IpAddr>() {
        Ok(address) => address.is_loopback(),
        Err(_) => host.eq_ignore_ascii_case("localhost"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};

    fn guarded_blocking_client() -> reqwest::blocking::Client {
        reqwest::blocking::Client::builder()
            .proxy(loopback_only_proxy())
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .expect("client")
    }

    #[test]
    fn loopback_hosts_are_recognized() {
        for raw in [
            "http://127.0.0.1:8080/x",
            "http://[::1]:8080/x",
            "http://localhost/x",
            "http://LOCALHOST:1/x",
        ] {
            let url = reqwest::Url::parse(raw).expect("url");
            assert!(url_is_loopback(&url), "{raw}");
        }
        for raw in ["https://openrouter.ai/api/v1/models", "http://203.0.113.1/"] {
            let url = reqwest::Url::parse(raw).expect("url");
            assert!(!url_is_loopback(&url), "{raw}");
        }
    }

    #[cfg(feature = "test-fixtures")]
    #[test]
    fn non_loopback_urls_are_refused() {
        for allowed in ["ws://127.0.0.1:9000/v1/ws", "ws://[::1]:9000/v1/ws"] {
            assert!(refuse_non_loopback(allowed).is_ok(), "{allowed}");
        }
        for refused in ["wss://logs.example.com/v1/ws", "not a url"] {
            assert!(refuse_non_loopback(refused).is_err(), "{refused}");
        }
    }

    #[test]
    fn guarded_client_fails_non_loopback_at_connect_without_dns() {
        // `.invalid` never resolves; a DNS error here would mean the request left the guard.
        let error = guarded_blocking_client()
            .get("http://egress-guard.invalid/models")
            .send()
            .expect_err("non-loopback request must fail");
        assert!(error.is_connect(), "expected connect failure, got {error}");
    }

    #[test]
    fn guarded_client_reaches_loopback_directly() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("addr");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buffer = [0u8; 1024];
            let _request_bytes = stream.read(&mut buffer).expect("read request");
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .expect("write response");
        });
        let body = guarded_blocking_client()
            .get(format!("http://{address}/models"))
            .send()
            .expect("loopback request")
            .text()
            .expect("body");
        assert_eq!(body, "ok");
        server.join().expect("server thread");
    }
}
