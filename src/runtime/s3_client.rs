//! Minimal AWS S3 client for Phase 4 workspace data ingestion.
//!
//! Implements just the two operations the workspace materializer needs —
//! `ListObjectsV2` and `GetObject` — with SigV4 signing built on
//! `reqwest`, `sha2`, `hmac`, and `chrono`. Pulling the full
//! `aws-sdk-s3` crate would more than double the compile surface for two
//! call sites; this module stays under 400 lines of focused code.
//!
//! Scope limits:
//!
//! * Path-style and virtual-hosted bucket URLs only.
//! * AWS standard regions (us-east-1, us-west-2, eu-…). The endpoint
//!   pattern is `https://s3.<region>.amazonaws.com/<bucket>`. Other
//!   providers can be plugged in later via an explicit endpoint override.
//! * No multipart, no presigning, no streaming uploads, no session-token
//!   credentials. Only static `(access_key, secret_key)` pairs.
//! * GET responses are buffered in memory up to a per-object cap. The
//!   workspace materializer enforces the cumulative cap.
//!
//! The SigV4 implementation is the standard "AWS Signature Version 4"
//! algorithm described in
//! https://docs.aws.amazon.com/IAM/latest/UserGuide/reference_aws-signing.html.

use std::io::Read;
use std::time::Duration;

use chrono::Utc;
use hmac::{Hmac, Mac};
use reqwest::blocking::Client;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use roxmltree::Document;
use sha2::{Digest, Sha256};

use crate::error::{Result, StackError};

const SERVICE: &str = "s3";
const ALGO: &str = "AWS4-HMAC-SHA256";
const EMPTY_BODY_SHA: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
const USER_AGENT: &str = concat!("acp-stack/", env!("CARGO_PKG_VERSION"), " s3-ingest");
const DEFAULT_TIMEOUT_SECS: u64 = 600;
const DEFAULT_MAX_OBJECTS_PER_PAGE: usize = 1000;
const STREAM_CHUNK_BYTES: usize = 32 * 1024;

type HmacSha256 = Hmac<Sha256>;

/// Static credentials. We intentionally do not implement the full AWS
/// credential-provider chain — operators declare the access/secret refs
/// in config and resolve them through the encrypted secret store.
#[derive(Debug, Clone)]
pub struct Credentials {
    pub access_key: String,
    pub secret_key: String,
}

#[derive(Debug, Clone)]
pub struct S3Client {
    http: Client,
    region: String,
    credentials: Credentials,
    /// Optional override for testing against a local mock. None ⇒
    /// the real AWS endpoint pattern.
    endpoint_base: Option<String>,
}

impl S3Client {
    pub fn new(region: String, credentials: Credentials) -> Result<Self> {
        let http = Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(DEFAULT_TIMEOUT_SECS))
            .build()
            .map_err(|source| StackError::SafeDownloadFailed {
                url: String::new(),
                reason: format!("build s3 reqwest client: {source}"),
            })?;
        Ok(Self {
            http,
            region,
            credentials,
            endpoint_base: None,
        })
    }

    /// Override the endpoint base, e.g. `http://127.0.0.1:9000` for a
    /// localhost MinIO-style mock in tests. The override carries the
    /// scheme and authority only; we still build the path ourselves.
    pub fn with_endpoint_base(mut self, base: impl Into<String>) -> Self {
        self.endpoint_base = Some(base.into());
        self
    }
}

/// One object in a `ListObjectsV2` response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3Object {
    pub key: String,
    pub size: u64,
    pub etag: String,
}

#[derive(Debug, Clone, Default)]
pub struct ListObjectsPage {
    pub objects: Vec<S3Object>,
    pub next_continuation_token: Option<String>,
    pub is_truncated: bool,
}

impl S3Client {
    pub fn list_objects_v2(
        &self,
        bucket: &str,
        prefix: Option<&str>,
        continuation_token: Option<&str>,
    ) -> Result<ListObjectsPage> {
        let mut query: Vec<(String, String)> = vec![
            ("list-type".to_owned(), "2".to_owned()),
            (
                "max-keys".to_owned(),
                DEFAULT_MAX_OBJECTS_PER_PAGE.to_string(),
            ),
        ];
        if let Some(p) = prefix
            && !p.is_empty()
        {
            query.push(("prefix".to_owned(), p.to_owned()));
        }
        if let Some(t) = continuation_token {
            query.push(("continuation-token".to_owned(), t.to_owned()));
        }

        let body = self.signed_get(bucket, "", &query)?;
        parse_list_objects_v2(&body)
    }

    /// Fetch `bucket/key`. The full body is buffered into memory because
    /// the workspace materializer wants a single `bytes` slice it can
    /// write atomically; callers are responsible for enforcing the
    /// per-object size before requesting (the response cap below is
    /// defensive backstop).
    pub fn get_object(&self, bucket: &str, key: &str, max_bytes: u64) -> Result<Vec<u8>> {
        // Pass the raw key — `canonical_uri` will percent-encode it
        // exactly once. Pre-encoding here used to double-encode
        // (`% → %25`) and break SigV4 for keys with spaces, `+`, or
        // non-ASCII characters.
        self.signed_get_with_path(bucket, key, &[], max_bytes)
    }

    fn signed_get(
        &self,
        bucket: &str,
        path_raw: &str,
        query: &[(String, String)],
    ) -> Result<Vec<u8>> {
        self.signed_get_with_path(bucket, path_raw, query, u64::MAX)
    }

    fn signed_get_with_path(
        &self,
        bucket: &str,
        path_raw: &str,
        query: &[(String, String)],
        max_bytes: u64,
    ) -> Result<Vec<u8>> {
        let now = Utc::now();
        let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
        let date_stamp = now.format("%Y%m%d").to_string();
        let host = host_for(&self.endpoint_base, &self.region);

        // Canonical request inputs. `canonical_uri` percent-encodes the
        // raw path once; we then build the HTTP URL from the same
        // encoded form so the request line matches what we signed.
        let canonical_uri = canonical_uri(bucket, path_raw);
        let canonical_query = canonical_query_string(query);

        let mut signed_headers: Vec<(&'static str, String)> = vec![
            ("host", host.clone()),
            ("x-amz-content-sha256", EMPTY_BODY_SHA.to_owned()),
            ("x-amz-date", amz_date.clone()),
        ];
        signed_headers.sort_by(|a, b| a.0.cmp(b.0));

        let canonical_headers = signed_headers
            .iter()
            .map(|(name, value)| format!("{name}:{}\n", value.trim()))
            .collect::<String>();
        let signed_header_names = signed_headers
            .iter()
            .map(|(name, _)| *name)
            .collect::<Vec<_>>()
            .join(";");

        let canonical_request = format!(
            "GET\n{canonical_uri}\n{canonical_query}\n{canonical_headers}\n{signed_header_names}\n{EMPTY_BODY_SHA}"
        );
        let canonical_request_hash = hex_sha256(canonical_request.as_bytes());

        // String-to-sign.
        let credential_scope = format!("{date_stamp}/{}/{SERVICE}/aws4_request", self.region);
        let string_to_sign =
            format!("{ALGO}\n{amz_date}\n{credential_scope}\n{canonical_request_hash}");

        // Signing key derivation.
        let signing_key = derive_signing_key(
            &self.credentials.secret_key,
            &date_stamp,
            &self.region,
            SERVICE,
        )?;
        let signature = hex_hmac(&signing_key, string_to_sign.as_bytes())?;

        // Authorization header.
        let authorization = format!(
            "{ALGO} Credential={}/{credential_scope}, SignedHeaders={signed_header_names}, Signature={signature}",
            self.credentials.access_key
        );

        // Build and dispatch the request from the encoded canonical_uri
        // so the on-the-wire path matches the signed canonical request
        // byte-for-byte.
        let host_base = match &self.endpoint_base {
            Some(base) => base.trim_end_matches('/').to_owned(),
            None => format!("https://s3.{}.amazonaws.com", self.region),
        };
        let mut url = format!("{host_base}{canonical_uri}");
        if !canonical_query.is_empty() {
            url.push('?');
            url.push_str(&canonical_query);
        }

        let mut headers = HeaderMap::new();
        for (name, value) in &signed_headers {
            headers.insert(
                HeaderName::from_static(name),
                HeaderValue::from_str(value).map_err(|source| StackError::SafeDownloadFailed {
                    url: url.clone(),
                    reason: format!("invalid header `{name}`: {source}"),
                })?,
            );
        }
        headers.insert(
            HeaderName::from_static("authorization"),
            HeaderValue::from_str(&authorization).map_err(|source| {
                StackError::SafeDownloadFailed {
                    url: url.clone(),
                    reason: format!("invalid authorization header: {source}"),
                }
            })?,
        );

        let response = self
            .http
            .get(&url)
            .headers(headers)
            .send()
            .map_err(|source| StackError::SafeDownloadFailed {
                url: url.clone(),
                reason: source.to_string(),
            })?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            return Err(StackError::SafeDownloadHttpStatus { url, status });
        }
        read_response_capped(response, &url, max_bytes)
    }
}

fn read_response_capped(
    mut response: reqwest::blocking::Response,
    url: &str,
    max_bytes: u64,
) -> Result<Vec<u8>> {
    if let Some(content_length) = response.content_length()
        && content_length > max_bytes
    {
        return Err(StackError::SafeDownloadTooLarge { limit: max_bytes });
    }

    let mut out = match response.content_length() {
        Some(length) if length <= usize::MAX as u64 => Vec::with_capacity(length as usize),
        _ => Vec::new(),
    };
    let mut buf = vec![0u8; STREAM_CHUNK_BYTES];
    let mut total: u64 = 0;
    loop {
        let read = response
            .read(&mut buf)
            .map_err(|source| StackError::SafeDownloadFailed {
                url: url.to_owned(),
                reason: format!("read response: {source}"),
            })?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .ok_or(StackError::SafeDownloadTooLarge { limit: max_bytes })?;
        if total > max_bytes {
            return Err(StackError::SafeDownloadTooLarge { limit: max_bytes });
        }
        out.extend_from_slice(&buf[..read]);
    }
    Ok(out)
}

fn host_for(endpoint_base: &Option<String>, region: &str) -> String {
    if let Some(base) = endpoint_base {
        // Strip the scheme to extract authority for the `Host` header.
        let authority = base
            .trim_start_matches("http://")
            .trim_start_matches("https://")
            .trim_end_matches('/');
        return authority.to_owned();
    }
    format!("s3.{region}.amazonaws.com")
}

fn canonical_uri(bucket: &str, path: &str) -> String {
    let trimmed_path = path.trim_start_matches('/');
    if trimmed_path.is_empty() {
        format!("/{}", uri_encode(bucket, false))
    } else {
        // bucket is already a single segment; the object key may have
        // embedded `/` which we preserve unescaped (S3 expects that).
        let mut out = String::with_capacity(2 + bucket.len() + trimmed_path.len());
        out.push('/');
        out.push_str(&uri_encode(bucket, false));
        out.push('/');
        out.push_str(&uri_encode(trimmed_path, true));
        out
    }
}

fn canonical_query_string(query: &[(String, String)]) -> String {
    let mut pairs: Vec<(String, String)> = query
        .iter()
        .map(|(k, v)| (uri_encode(k, false), uri_encode(v, false)))
        .collect();
    pairs.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    pairs
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// Percent-encode per RFC 3986 with the AWS-specific rule that `/` is
/// preserved when `preserve_slash` is true (object keys may contain `/`).
fn uri_encode(input: &str, preserve_slash: bool) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.as_bytes() {
        let unreserved = byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'_' | b'.' | b'~')
            || (preserve_slash && *byte == b'/');
        if unreserved {
            out.push(*byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn derive_signing_key(
    secret_key: &str,
    date_stamp: &str,
    region: &str,
    service: &str,
) -> Result<Vec<u8>> {
    let initial = format!("AWS4{secret_key}");
    let date_key = mac(initial.as_bytes(), date_stamp.as_bytes())?;
    let region_key = mac(&date_key, region.as_bytes())?;
    let service_key = mac(&region_key, service.as_bytes())?;
    let signing_key = mac(&service_key, b"aws4_request")?;
    Ok(signing_key)
}

fn mac(key: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    let mut mac =
        HmacSha256::new_from_slice(key).map_err(|source| StackError::SafeDownloadFailed {
            url: String::new(),
            reason: format!("invalid HMAC key length: {source}"),
        })?;
    mac.update(data);
    Ok(mac.finalize().into_bytes().to_vec())
}

fn hex_hmac(key: &[u8], data: &[u8]) -> Result<String> {
    Ok(hex_lower(&mac(key, data)?))
}

fn hex_sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex_lower(&hasher.finalize())
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn parse_list_objects_v2(body: &[u8]) -> Result<ListObjectsPage> {
    let text = std::str::from_utf8(body).map_err(|source| StackError::SafeDownloadFailed {
        url: "<s3 list response>".to_owned(),
        reason: format!("response is not valid UTF-8: {source}"),
    })?;
    let doc = Document::parse(text).map_err(|source| StackError::SafeDownloadFailed {
        url: "<s3 list response>".to_owned(),
        reason: format!("malformed XML: {source}"),
    })?;
    let root = doc.root_element();
    if root.tag_name().name() != "ListBucketResult" {
        return Err(StackError::SafeDownloadFailed {
            url: "<s3 list response>".to_owned(),
            reason: format!("unexpected root element `{}`", root.tag_name().name()),
        });
    }
    let mut page = ListObjectsPage::default();
    for child in root.children().filter(|c| c.is_element()) {
        match child.tag_name().name() {
            "Contents" => {
                let mut key = String::new();
                let mut size: u64 = 0;
                let mut etag = String::new();
                for entry in child.children().filter(|c| c.is_element()) {
                    let value = entry.text().unwrap_or("").trim();
                    match entry.tag_name().name() {
                        "Key" => key = value.to_owned(),
                        "Size" => size = value.parse().unwrap_or(0),
                        "ETag" => etag = value.trim_matches('"').to_owned(),
                        _ => {}
                    }
                }
                if !key.is_empty() {
                    page.objects.push(S3Object { key, size, etag });
                }
            }
            "IsTruncated" => {
                page.is_truncated = child
                    .text()
                    .unwrap_or("")
                    .trim()
                    .eq_ignore_ascii_case("true");
            }
            "NextContinuationToken" => {
                let value = child.text().unwrap_or("").trim();
                if !value.is_empty() {
                    page.next_continuation_token = Some(value.to_owned());
                }
            }
            _ => {}
        }
    }
    Ok(page)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::StatusCode as AxumStatus;
    use axum::response::Response;
    use axum::routing::get;
    use std::net::SocketAddr;
    use std::sync::mpsc;
    use tokio::sync::oneshot;

    #[test]
    fn uri_encode_preserves_slash_when_requested() {
        assert_eq!(uri_encode("a/b c", false), "a%2Fb%20c");
        assert_eq!(uri_encode("a/b c", true), "a/b%20c");
        assert_eq!(uri_encode("plain.txt", false), "plain.txt");
        assert_eq!(uri_encode("plain.txt", true), "plain.txt");
        assert_eq!(uri_encode("héllo", false), "h%C3%A9llo");
    }

    #[test]
    fn canonical_query_sorts_and_encodes_pairs() {
        let pairs = vec![
            ("list-type".to_owned(), "2".to_owned()),
            ("prefix".to_owned(), "a/b/".to_owned()),
            ("max-keys".to_owned(), "1000".to_owned()),
        ];
        let canonical = canonical_query_string(&pairs);
        assert_eq!(canonical, "list-type=2&max-keys=1000&prefix=a%2Fb%2F");
    }

    #[test]
    fn signing_key_derivation_matches_aws_test_vector() {
        // From the AWS SigV4 test suite — example secret/date/region/service.
        let key = derive_signing_key(
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            "20150830",
            "us-east-1",
            "iam",
        )
        .expect("derive");
        let hex = hex_lower(&key);
        assert_eq!(
            hex,
            "c4afb1cc5771d871763a393e44b703571b55cc28424d1a5e86da6ed3c154a4b9"
        );
    }

    #[test]
    fn parse_list_objects_v2_extracts_contents_and_pagination() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Name>example</Name>
  <Prefix>data/</Prefix>
  <KeyCount>2</KeyCount>
  <MaxKeys>1000</MaxKeys>
  <IsTruncated>true</IsTruncated>
  <NextContinuationToken>abc123</NextContinuationToken>
  <Contents>
    <Key>data/one.txt</Key>
    <LastModified>2026-01-01T00:00:00.000Z</LastModified>
    <ETag>"d41d8cd98f00b204e9800998ecf8427e"</ETag>
    <Size>42</Size>
    <StorageClass>STANDARD</StorageClass>
  </Contents>
  <Contents>
    <Key>data/two.bin</Key>
    <LastModified>2026-01-02T00:00:00.000Z</LastModified>
    <ETag>"abc"</ETag>
    <Size>1024</Size>
    <StorageClass>STANDARD</StorageClass>
  </Contents>
</ListBucketResult>"#;
        let page = parse_list_objects_v2(xml).expect("parse");
        assert_eq!(page.objects.len(), 2);
        assert_eq!(page.objects[0].key, "data/one.txt");
        assert_eq!(page.objects[0].size, 42);
        assert_eq!(page.objects[1].key, "data/two.bin");
        assert_eq!(page.objects[1].size, 1024);
        assert!(page.is_truncated);
        assert_eq!(page.next_continuation_token.as_deref(), Some("abc123"));
    }

    #[test]
    fn canonical_uri_single_encodes_object_keys() {
        // Object keys with spaces, `+`, and non-ASCII characters must
        // appear percent-encoded **exactly once** in the canonical URI;
        // double-encoding would break SigV4 against real S3.
        assert_eq!(
            canonical_uri("example", "data/has spaces+and%signs/héllo"),
            "/example/data/has%20spaces%2Band%25signs/h%C3%A9llo"
        );
        // Leading slash on the key is trimmed.
        assert_eq!(
            canonical_uri("example", "/leading/slash"),
            "/example/leading/slash"
        );
    }

    #[test]
    fn parse_list_objects_v2_rejects_wrong_root() {
        let xml = br#"<?xml version="1.0"?><Other/>"#;
        let err = parse_list_objects_v2(xml).expect_err("rejected");
        match err {
            StackError::SafeDownloadFailed { reason, .. } => {
                assert!(
                    reason.contains("unexpected root element"),
                    "reason: {reason}"
                );
            }
            other => panic!("unexpected: {other}"),
        }
    }

    struct TestServer {
        addr: SocketAddr,
        shutdown: Option<oneshot::Sender<()>>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl TestServer {
        fn endpoint(&self) -> String {
            format!("http://{}", self.addr)
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
        let (addr_tx, addr_rx) = mpsc::sync_channel(1);
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

    fn test_client(endpoint: String) -> S3Client {
        S3Client::new(
            "us-east-1".to_owned(),
            Credentials {
                access_key: "AKIAIOSFODNN7EXAMPLE".to_owned(),
                secret_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".to_owned(),
            },
        )
        .expect("client")
        .with_endpoint_base(endpoint)
    }

    #[test]
    fn get_object_allows_exact_size_limit() {
        let router = Router::new().route(
            "/example/data.bin",
            get(|| async {
                Response::builder()
                    .status(AxumStatus::OK)
                    .body(Body::from(vec![7u8; 16 * 1024]))
                    .unwrap()
            }),
        );
        let server = spawn_test_server(router);
        let client = test_client(server.endpoint());
        let body = client
            .get_object("example", "data.bin", 16 * 1024)
            .expect("body");
        assert_eq!(body, vec![7u8; 16 * 1024]);
    }

    #[test]
    fn get_object_rejects_body_over_size_limit_while_streaming() {
        let router = Router::new().route(
            "/example/big.bin",
            get(|| async {
                let chunks = futures::stream::iter([Ok::<_, std::io::Error>(vec![
                    3u8;
                    STREAM_CHUNK_BYTES
                        + 1
                ])]);
                Response::builder()
                    .status(AxumStatus::OK)
                    .body(Body::from_stream(chunks))
                    .unwrap()
            }),
        );
        let server = spawn_test_server(router);
        let client = test_client(server.endpoint());
        let err = client
            .get_object("example", "big.bin", STREAM_CHUNK_BYTES as u64)
            .expect_err("too large");
        assert!(matches!(err, StackError::SafeDownloadTooLarge { .. }));
    }
}
