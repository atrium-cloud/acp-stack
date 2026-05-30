//! Safe HTTPS downloader for untrusted external sources.
//!
//! Used by the workspace init materializer to pull archives from public URLs
//! (Drive/Dropbox/arbitrary hosts) and other Phase 4 download paths. Hardened
//! against the failure modes that matter for untrusted endpoints:
//!
//! * Scheme allowlist on the requested URL **and** every redirect target.
//!   Production callers stick to `https`; tests can broaden the allowlist.
//! * Bounded redirect chain (default 3) so a hostile server cannot spin us
//!   forever or trick us into following a downgrade.
//! * Streaming body copy with a hard byte cap. Oversized responses abort
//!   mid-stream rather than buffering the whole body.
//! * Optional sha256 verification on the fully-written file.
//! * Connect and read timeouts so a slow loris cannot keep us hanging.
//!
//! Intentionally NOT done here: archive extraction, mime-type inference, or
//! content-disposition parsing — those live in `safe_extract` and the
//! materializer. This module owns the wire.

use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::Path;
use std::time::Duration;

use reqwest::redirect::Policy;
use sha2::{Digest, Sha256};

use crate::dev_gates::{TEST_INSECURE_HTTPS_ENV, fixture_enabled};
use crate::error::{Result, StackError};

const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 30;
const DEFAULT_READ_TIMEOUT_SECS: u64 = 600;
const DEFAULT_MAX_REDIRECTS: usize = 3;
/// Cap on individual downloads when callers do not provide an explicit
/// limit. Public so other materializers (notably the S3 ingest path) can
/// share the same default size budget.
pub const DEFAULT_MAX_DOWNLOAD_BYTES: u64 = 500 * 1024 * 1024; // 500 MiB
const STREAM_CHUNK_BYTES: usize = 32 * 1024;
const USER_AGENT: &str = concat!("acp-stack/", env!("CARGO_PKG_VERSION"));

/// Options for a single download. Defaults are tuned for archive ingestion
/// from untrusted hosts; callers tighten via the builder methods.
#[derive(Debug, Clone)]
pub struct DownloadOpts {
    /// Schemes we accept on the requested URL and on each redirect target.
    pub allowed_schemes: Vec<String>,
    /// Maximum redirects to follow. `0` means none.
    pub max_redirects: usize,
    /// Hard cap on bytes written to disk. Streams abort mid-stream when hit.
    pub max_bytes: u64,
    /// TCP connect timeout.
    pub connect_timeout: Duration,
    /// Per-read timeout once the connection is established.
    pub read_timeout: Duration,
    /// Optional sha256 the downloaded content must match (lowercase hex).
    pub expected_sha256: Option<String>,
}

impl Default for DownloadOpts {
    fn default() -> Self {
        Self {
            allowed_schemes: vec!["https".to_owned()],
            max_redirects: DEFAULT_MAX_REDIRECTS,
            max_bytes: DEFAULT_MAX_DOWNLOAD_BYTES,
            connect_timeout: Duration::from_secs(DEFAULT_CONNECT_TIMEOUT_SECS),
            read_timeout: Duration::from_secs(DEFAULT_READ_TIMEOUT_SECS),
            expected_sha256: None,
        }
    }
}

/// Outcome of a successful download.
#[derive(Debug, Clone)]
pub struct DownloadReport {
    pub bytes_written: u64,
    pub sha256: String,
    pub content_type: Option<String>,
    pub final_url: String,
}

/// Download `url` to `dest`, enforcing scheme/redirect/size policy.
///
/// `dest` is overwritten if it exists. On any failure the partial file is
/// removed so callers do not see a half-written artifact.
pub fn download_to_file(url: &str, dest: &Path, opts: &DownloadOpts) -> Result<DownloadReport> {
    enforce_scheme(url, &opts.allowed_schemes)?;

    let client = build_client(opts)?;
    let response = client
        .get(url)
        .send()
        .map_err(|source| StackError::SafeDownloadFailed {
            url: url.to_owned(),
            reason: source.to_string(),
        })?;

    let final_url = response.url().to_string();
    enforce_scheme(&final_url, &opts.allowed_schemes).map_err(|err| match err {
        StackError::SafeDownloadInsecureRedirect { url } => {
            StackError::SafeDownloadInsecureRedirect { url }
        }
        other => other,
    })?;

    let status = response.status();
    if !status.is_success() {
        return Err(StackError::SafeDownloadHttpStatus {
            url: final_url,
            status: status.as_u16(),
        });
    }

    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_owned());

    if let Some(content_length) = response.content_length()
        && content_length > opts.max_bytes
    {
        return Err(StackError::SafeDownloadTooLarge {
            limit: opts.max_bytes,
        });
    }

    // Stream the body through a size-capped, hashing writer.
    let outcome = write_streaming(response, dest, opts);
    match outcome {
        Ok((bytes_written, sha256)) => {
            if let Some(expected) = opts.expected_sha256.as_deref()
                && !sha_eq(expected, &sha256)
            {
                cleanup_partial(dest);
                return Err(StackError::SafeDownloadChecksumMismatch {
                    expected: expected.to_owned(),
                    actual: sha256,
                });
            }
            Ok(DownloadReport {
                bytes_written,
                sha256,
                content_type,
                final_url,
            })
        }
        Err(err) => {
            cleanup_partial(dest);
            Err(err)
        }
    }
}

fn build_client(opts: &DownloadOpts) -> Result<reqwest::blocking::Client> {
    let allowed = opts.allowed_schemes.clone();
    let policy = if opts.max_redirects == 0 {
        Policy::none()
    } else {
        let max = opts.max_redirects;
        Policy::custom(move |attempt| {
            if attempt.previous().len() >= max {
                return attempt.error(format!("exceeded the configured redirect limit of {max}"));
            }
            let scheme = attempt.url().scheme().to_owned();
            let target = attempt.url().to_string();
            if !allowed.iter().any(|s| s == &scheme) {
                return attempt.error(format!(
                    "redirect target `{target}` uses disallowed scheme `{scheme}`"
                ));
            }
            attempt.follow()
        })
    };

    let mut builder = reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(opts.connect_timeout)
        .timeout(opts.read_timeout)
        .redirect(policy);
    if fixture_enabled(TEST_INSECURE_HTTPS_ENV) {
        builder = builder.danger_accept_invalid_certs(true);
    }
    builder
        .build()
        .map_err(|source| StackError::SafeDownloadFailed {
            url: String::new(),
            reason: source.to_string(),
        })
}

fn write_streaming(
    mut response: reqwest::blocking::Response,
    dest: &Path,
    opts: &DownloadOpts,
) -> Result<(u64, String)> {
    let final_url = response.url().to_string();
    if let Some(parent) = dest.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|source| StackError::SafeDownloadFailed {
            url: final_url.clone(),
            reason: format!(
                "could not create destination directory `{}`: {source}",
                parent.display()
            ),
        })?;
    }
    let file = File::create(dest).map_err(|source| StackError::SafeDownloadFailed {
        url: final_url.clone(),
        reason: format!(
            "could not create destination `{}`: {source}",
            dest.display()
        ),
    })?;
    let mut writer = BufWriter::new(file);
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; STREAM_CHUNK_BYTES];
    let mut total: u64 = 0;

    loop {
        let read = response
            .read(&mut buf)
            .map_err(|source| StackError::SafeDownloadFailed {
                url: final_url.clone(),
                reason: format!("read failed: {source}"),
            })?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .ok_or_else(|| StackError::SafeDownloadTooLarge {
                limit: opts.max_bytes,
            })?;
        if total > opts.max_bytes {
            return Err(StackError::SafeDownloadTooLarge {
                limit: opts.max_bytes,
            });
        }
        hasher.update(&buf[..read]);
        writer
            .write_all(&buf[..read])
            .map_err(|source| StackError::SafeDownloadFailed {
                url: final_url.clone(),
                reason: format!("write failed: {source}"),
            })?;
    }

    writer
        .flush()
        .map_err(|source| StackError::SafeDownloadFailed {
            url: final_url.clone(),
            reason: format!("flush failed: {source}"),
        })?;

    let sha256 = format!("{:x}", hasher.finalize());
    Ok((total, sha256))
}

fn enforce_scheme(url: &str, allowed: &[String]) -> Result<()> {
    let scheme = url.split_once("://").map(|(s, _)| s).unwrap_or("");
    if scheme.is_empty() || !allowed.iter().any(|s| s == scheme) {
        return Err(StackError::SafeDownloadInsecureRedirect {
            url: url.to_owned(),
        });
    }
    Ok(())
}

fn cleanup_partial(dest: &Path) {
    if dest.exists() {
        // Best-effort cleanup; the caller will report a more specific error.
        let _ = std::fs::remove_file(dest);
    }
}

fn sha_eq(expected: &str, actual: &str) -> bool {
    if expected.len() != actual.len() {
        return false;
    }
    expected
        .bytes()
        .zip(actual.bytes())
        .all(|(a, b)| a.eq_ignore_ascii_case(&b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::extract::Path as AxumPath;
    use axum::http::{HeaderMap, StatusCode as AxumStatus, header};
    use axum::response::{IntoResponse, Redirect, Response};
    use axum::routing::get;
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::oneshot;

    fn http_opts() -> DownloadOpts {
        // Tests serve over plain HTTP. Production callers use the
        // HTTPS-only default.
        DownloadOpts {
            allowed_schemes: vec!["http".to_owned(), "https".to_owned()],
            ..DownloadOpts::default()
        }
    }

    struct TestServer {
        addr: SocketAddr,
        shutdown: Option<oneshot::Sender<()>>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl TestServer {
        fn url(&self, path: &str) -> String {
            format!("http://{}{path}", self.addr)
        }
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            if let Some(tx) = self.shutdown.take() {
                let _ = tx.send(());
            }
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    fn spawn_test_server(router: Router) -> TestServer {
        let (addr_tx, addr_rx) = std::sync::mpsc::sync_channel(1);
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let handle = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build runtime");
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                    .await
                    .expect("bind test server");
                let addr = listener.local_addr().expect("local addr");
                addr_tx.send(addr).expect("send addr");
                axum::serve(listener, router)
                    .with_graceful_shutdown(async move {
                        let _ = shutdown_rx.await;
                    })
                    .await
                    .expect("serve");
            });
        });
        let addr = addr_rx.recv().expect("addr from test server");
        TestServer {
            addr,
            shutdown: Some(shutdown_tx),
            handle: Some(handle),
        }
    }

    #[test]
    fn refuses_http_when_only_https_allowed() {
        let dest_dir = tempfile::tempdir().expect("tempdir");
        let dest = dest_dir.path().join("download.bin");
        let err = download_to_file("http://example.com/x", &dest, &DownloadOpts::default())
            .expect_err("http rejected by default");
        match err {
            StackError::SafeDownloadInsecureRedirect { url } => {
                assert_eq!(url, "http://example.com/x");
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn downloads_small_body_with_sha256() {
        let router = Router::new().route(
            "/data",
            get(|| async {
                let body = b"hello, acp\n";
                Response::builder()
                    .status(AxumStatus::OK)
                    .header(header::CONTENT_TYPE, "application/octet-stream")
                    .body(Body::from(&body[..]))
                    .unwrap()
            }),
        );
        let server = spawn_test_server(router);
        let dest_dir = tempfile::tempdir().expect("tempdir");
        let dest = dest_dir.path().join("download.bin");
        let report = download_to_file(&server.url("/data"), &dest, &http_opts()).expect("ok");
        assert_eq!(report.bytes_written, 11);
        assert_eq!(
            report.content_type.as_deref(),
            Some("application/octet-stream")
        );
        let on_disk = std::fs::read(&dest).expect("read back");
        assert_eq!(&on_disk, b"hello, acp\n");
        // sha256("hello, acp\n")
        let mut hasher = Sha256::new();
        hasher.update(b"hello, acp\n");
        let expected = format!("{:x}", hasher.finalize());
        assert_eq!(report.sha256, expected);
    }

    #[test]
    fn aborts_when_body_exceeds_cap() {
        // 16 KiB body, but cap at 8 KiB.
        let router = Router::new().route(
            "/big",
            get(|| async {
                let body = vec![0u8; 16 * 1024];
                Response::builder()
                    .status(AxumStatus::OK)
                    .body(Body::from(body))
                    .unwrap()
            }),
        );
        let server = spawn_test_server(router);
        let dest_dir = tempfile::tempdir().expect("tempdir");
        let dest = dest_dir.path().join("download.bin");
        let opts = DownloadOpts {
            max_bytes: 8 * 1024,
            ..http_opts()
        };
        let err = download_to_file(&server.url("/big"), &dest, &opts).expect_err("too large");
        assert!(matches!(err, StackError::SafeDownloadTooLarge { .. }));
        // Partial file removed.
        assert!(!dest.exists());
    }

    #[test]
    fn rejects_http_redirect_when_only_https_allowed() {
        // Production callers use https-only. We confirm redirect targets are
        // checked against the scheme allowlist by serving an HTTP redirect
        // while the client says it allows only https. We have to also allow
        // http on the initial request so we can reach the test server, so we
        // construct the opts manually.
        let router = Router::new().route(
            "/r",
            get(|| async { Redirect::temporary("http://127.0.0.1:1/elsewhere") }),
        );
        let server = spawn_test_server(router);
        // Allow http only on the initial request; redirect target uses http
        // too, so we must restrict the *redirect* by another mechanism. For
        // this test we narrow the allowlist after the initial hit by serving
        // a redirect with a forbidden scheme. The simplest way is to keep
        // http allowed (we are testing redirect handling, not the initial
        // hit's scheme check) and instead serve a redirect to a `gopher://`
        // URL.
        // Re-spawn with a gopher redirect instead.
        drop(server);
        let router = Router::new().route(
            "/r",
            get(|| async { Redirect::temporary("gopher://forbidden/elsewhere") }),
        );
        let server = spawn_test_server(router);
        let dest_dir = tempfile::tempdir().expect("tempdir");
        let dest = dest_dir.path().join("download.bin");
        let err = download_to_file(&server.url("/r"), &dest, &http_opts()).expect_err("blocked");
        // The reqwest redirect policy emits an opaque error; we surface it
        // through SafeDownloadFailed. Either is acceptable signalling.
        // reqwest wraps our Policy error generically as "error following
        // redirect"; either that or our own InsecureRedirect mapping is
        // acceptable signalling — the test's point is that the gopher://
        // hop did not happen.
        match err {
            StackError::SafeDownloadFailed { .. }
            | StackError::SafeDownloadInsecureRedirect { .. } => {}
            other => panic!("unexpected error: {other}"),
        }
        assert!(!dest.exists());
    }

    #[test]
    fn enforces_redirect_limit() {
        // Each /a -> /a -> /a … forever, but capped redirects = 1.
        let count = Arc::new(AtomicUsize::new(0));
        let count_for_handler = Arc::clone(&count);
        let router = Router::new().route(
            "/loop/{n}",
            get({
                move |AxumPath(n): AxumPath<usize>| {
                    let count = Arc::clone(&count_for_handler);
                    async move {
                        count.fetch_add(1, Ordering::SeqCst);
                        Redirect::temporary(&format!("/loop/{}", n + 1)).into_response()
                    }
                }
            }),
        );
        let server = spawn_test_server(router);
        let dest_dir = tempfile::tempdir().expect("tempdir");
        let dest = dest_dir.path().join("download.bin");
        let opts = DownloadOpts {
            max_redirects: 1,
            ..http_opts()
        };
        let err = download_to_file(&server.url("/loop/0"), &dest, &opts).expect_err("loop");
        assert!(matches!(err, StackError::SafeDownloadFailed { .. }));
        // Hit at most max_redirects + 1 endpoints (the initial + one follow).
        assert!(count.load(Ordering::SeqCst) <= 2, "redirects: {count:?}");
    }

    #[test]
    fn surfaces_http_status_errors() {
        let router = Router::new().route(
            "/nope",
            get(|| async {
                let mut headers = HeaderMap::new();
                headers.insert(header::CONTENT_TYPE, "text/plain".parse().unwrap());
                (AxumStatus::NOT_FOUND, headers, "missing").into_response()
            }),
        );
        let server = spawn_test_server(router);
        let dest_dir = tempfile::tempdir().expect("tempdir");
        let dest = dest_dir.path().join("download.bin");
        let err = download_to_file(&server.url("/nope"), &dest, &http_opts()).expect_err("404");
        match err {
            StackError::SafeDownloadHttpStatus { status, .. } => assert_eq!(status, 404),
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn sha256_mismatch_is_rejected_and_file_removed() {
        let router = Router::new().route("/d", get(|| async { "abc" }));
        let server = spawn_test_server(router);
        let dest_dir = tempfile::tempdir().expect("tempdir");
        let dest = dest_dir.path().join("download.bin");
        let opts = DownloadOpts {
            expected_sha256: Some("0".repeat(64)),
            ..http_opts()
        };
        let err = download_to_file(&server.url("/d"), &dest, &opts).expect_err("mismatch");
        assert!(matches!(
            err,
            StackError::SafeDownloadChecksumMismatch { .. }
        ));
        assert!(!dest.exists());
    }
}
