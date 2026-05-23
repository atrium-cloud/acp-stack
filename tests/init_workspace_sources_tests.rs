//! End-to-end coverage for the Phase 4 workspace init materializer.
//!
//! Boundaries we exercise:
//!   * `acps init --code-from <local-bare-repo>` clones into
//!     `<root>/usr/code/<name>/` and writes the `.acp-stack-source.json`
//!     sentinel.
//!   * `acps init --data-from <local-path>` mirrors a local dataset into
//!     `<root>/usr/data/<name>/`.
//!   * Re-running `acps init` is idempotent when sentinels match.
//!   * `acps init --data-from https://...` downloads and extracts an
//!     archive without relying on an external network fixture.
//!   * `--data-from http://...` is rejected by the CLI parser before any
//!     network IO.

use acp_stack::runtime::init_runner::step_kind;
use acp_stack::state::{StateStore, default_state_path};
use assert_cmd::Command;
use base64::Engine;
use flate2::Compression;
use flate2::write::GzEncoder;
use std::io::Write;
use std::net::SocketAddr;
use std::path::Path;
use std::process::Command as StdCommand;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::oneshot;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

const TEST_CERT_PEM: &str = r#"-----BEGIN CERTIFICATE-----
MIIDJTCCAg2gAwIBAgIUD3qVKGnq2UkVRwq1dp4nR+gWFjswDQYJKoZIhvcNAQEL
BQAwFDESMBAGA1UEAwwJbG9jYWxob3N0MB4XDTI2MDUxOTE4MDA1NFoXDTM2MDUx
NjE4MDA1NFowFDESMBAGA1UEAwwJbG9jYWxob3N0MIIBIjANBgkqhkiG9w0BAQEF
AAOCAQ8AMIIBCgKCAQEAyjJOeGL1EpMPTEwpgq++d3SYYSJIgSvEU/IQPRF6ddBp
td2dlkh/TiZXPk+BRXjt8Exoa15R9DZwv7KHFLNkjC3qKn4RsGqolGIXRCWHCPO5
ww/OLj+gJlMdyYxtnwu0ZbhACLksUrKI/vBSR6llCXJvpHs/KV/7MUOgLXTWkrw+
/mdeSfhSot0Pn1onI4ERI0iXPzVT0Vfy+R/O0CkULLuv0HA30OtJ6YQ2TYAMlwcQ
2KWkkERGXIwv+8Pytewa8aoqYXuNmWyFbBXaO+amQdc56VjTrWuG9+JbOweGQsoo
Me45CB9sUhtGk9+JIcuRFCcBD7vXxp4xaa5/8AdD/wIDAQABo28wbTAdBgNVHQ4E
FgQU8NOwGsrxZcLSbsYRTtJLI98ZpG0wHwYDVR0jBBgwFoAU8NOwGsrxZcLSbsYR
TtJLI98ZpG0wDwYDVR0TAQH/BAUwAwEB/zAaBgNVHREEEzARgglsb2NhbGhvc3SH
BH8AAAEwDQYJKoZIhvcNAQELBQADggEBAAiqicmT/5YYPZoXlorq9ih6kXJJe/Nd
4+VUJ0mC2E6ahlZhVKKBxJVA5AeiinI74H6EWwdBMxxRjO0MYuLjPmg/27Fi1t+k
0j0LD9g5Qrtgking/pL7WGH0zHFeWVVVyzOHqPHsJulRDmTxjCOIMEtqgcfkH/DK
BedeO9r3qVKzAhlRDZHmy4oSMK5QCUVCI+ZWzBE5GcI2Ol+lqF5FSEDUjaMc76VN
T5CPc+7VET952HRTKSCRBonEvbanHGL0pxvhGGiVFpocZPUaP0xPdXn5i9hrsh3s
tUtnDtMcrvKvkGRnBzN9LBb1V3XbSIcc6qlqLAo5XSaBusVEA6g3WQI=
-----END CERTIFICATE-----"#;

const TEST_KEY_PEM: &str = r#"-----BEGIN PRIVATE KEY-----
MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQDKMk54YvUSkw9M
TCmCr753dJhhIkiBK8RT8hA9EXp10Gm13Z2WSH9OJlc+T4FFeO3wTGhrXlH0NnC/
socUs2SMLeoqfhGwaqiUYhdEJYcI87nDD84uP6AmUx3JjG2fC7RluEAIuSxSsoj+
8FJHqWUJcm+kez8pX/sxQ6AtdNaSvD7+Z15J+FKi3Q+fWicjgREjSJc/NVPRV/L5
H87QKRQsu6/QcDfQ60nphDZNgAyXBxDYpaSQREZcjC/7w/K17Brxqiphe42ZbIVs
Fdo75qZB1znpWNOta4b34ls7B4ZCyigx7jkIH2xSG0aT34khy5EUJwEPu9fGnjFp
rn/wB0P/AgMBAAECggEARq+FjUKTCHZOz86EaIKF5H7nUnXIwReK4AnssVyt4ggF
HKYoFESt9KUktMzYlW/sRqh/jKGBpw1tJycDYDJCwVq/1TETgAgZfR45ogI4jeGe
nFmnK7Xkh+FgtXgZTpOp8jGSeTo7C4IMsItVSGYowz+1VdwcPZunVhadJacF6G+1
Fqke1u/h9mAQGEkBK7S9RhzjF58KLSkX1H+3XVLsu+SGM2E5XeR585ydc6oO24yn
0n9A9MP0+00Np3USXz1ZqxiCq0DQAdab8bpgpQDeRXl4Dxo8ERlOe204K9fyN9hL
3qppOlEIrSlDk3RzYLzDa9ayO4niiIwrgocFlJWw0QKBgQD5r7hnPOjJRrxbvEzm
BbQC/AVh/MoIopjCObUlBSBHgTxK4OFQsVYRKE9kjQdw1H+4Bb+ZSFs/GYAO/xWK
4eK0hAkMZxyoNY+3nni+ci73S1Y6KQmSkreXLsKivrcrwHPA+7DO6vpaQgtKjB5e
0vvfu6ejyFuzdiLBcsk4hqoXiQKBgQDPTyw6PNeofKG1afBxi9zYCnoz22hVvU8H
egtdUe5OWoTm4B9kdQebuujCax70y8wPlwHCyTDMuFv/rwy2mWOjxEd5Lh8rcRXT
Wkw9kqi64aJnAC+qGX3wYX3OtZa8XtgcP4WnSvFZA2z5h9DSSm39B4giiUPwTqoH
6yhGa+iVRwKBgQCgvNzLqUx69syfidM/aYB/Q1r7v88YTARnVNsmn+wt3CbCVY73
cZJkrTyfEKMvob0u7JRxy0DimDn2bYSiydQ2PaHNmDu8le6VWLGGeN9/V8KE4pJF
B6qk6vdl6bzlr5lVLTKfomnU3RuDvDx48v4vBBFhwMQ3/d9WBd1ARIEPcQKBgQCz
gr4sa7Tee5VXTrXazJ5eIKxyrQGqhLOjKMYgd6qgBHfip53nUgNVkUKt3y4+ER7R
PRRUQ/zSuK1JOJ4Mm8QQ8gxl3U82QcUBb8Zm5tDeijUjdaDQbSSFexYCYXF2Cbae
t06XbyiPyIybgkSLY9Hb8aKrLSs8MhG/q5wDZrtOTwKBgEpVBE0tLqc2+FNnkBXx
7UgvW1XxhDq2N32k4zFBKY3Uc5vdMr2f4zbJdbVryENmhb80govtrDi55Sip4sf+
LKRBqeJuCvUNVyi2QOoAfx8ojYlEJAdsvEK3NAveQh2ogpdRDKQdp5kRI0DUlRUo
z7B2P/aVq7GlYKCC6g9aEJev
-----END PRIVATE KEY-----"#;

fn git_available() -> bool {
    StdCommand::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn run_git(repo: &Path, args: &[&str]) {
    let status = StdCommand::new("git")
        .args(args)
        .current_dir(repo)
        .status()
        .expect("git");
    assert!(status.success(), "git {args:?}");
}

fn prepare_upstream() -> tempfile::TempDir {
    let upstream = tempfile::tempdir().expect("upstream tempdir");
    run_git(upstream.path(), &["init", "-q"]);
    run_git(
        upstream.path(),
        &["config", "user.email", "test@example.com"],
    );
    run_git(upstream.path(), &["config", "user.name", "Test"]);
    run_git(upstream.path(), &["config", "commit.gpgsign", "false"]);
    std::fs::write(upstream.path().join("README.md"), "hello\n").expect("readme");
    run_git(upstream.path(), &["add", "README.md"]);
    run_git(upstream.path(), &["commit", "-q", "-m", "init"]);
    upstream
}

fn acps_init(home: &Path, workspace_root: &Path, extra: &[&str]) -> assert_cmd::Command {
    let mut cmd = Command::cargo_bin("acps").expect("acps");
    cmd.env("HOME", home)
        .arg("init")
        .arg("--workspace-root")
        .arg(workspace_root.as_os_str())
        .arg("--workspace-uploads")
        .arg(workspace_root.join("uploads").as_os_str())
        .arg("--no-install-agent");
    for value in extra {
        cmd.arg(value);
    }
    cmd
}

fn capture_names(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .expect("read capture dir")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect()
}

fn has_capture(names: &[String], tag: &str, extension: &str) -> bool {
    names
        .iter()
        .any(|name| name.starts_with(&format!("{tag}.")) && name.ends_with(extension))
}

struct HttpsArchiveServer {
    addr: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl HttpsArchiveServer {
    fn url(&self, path: &str) -> String {
        format!("https://localhost:{}{path}", self.addr.port())
    }
}

impl Drop for HttpsArchiveServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn pem_der(pem: &str, label: &str) -> Vec<u8> {
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let body = pem
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            !trimmed.is_empty() && trimmed != begin && trimmed != end
        })
        .collect::<String>();
    base64::engine::general_purpose::STANDARD
        .decode(body)
        .expect("decode pem")
}

fn archive_with_file(path: &str, contents: &[u8]) -> Vec<u8> {
    let mut tar_bytes = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut tar_bytes);
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, path, contents)
            .expect("append tar entry");
        builder.finish().expect("finish tar");
    }
    let mut archive = Vec::new();
    let mut encoder = GzEncoder::new(&mut archive, Compression::default());
    encoder.write_all(&tar_bytes).expect("gzip tar");
    encoder.finish().expect("finish gzip");
    archive
}

fn spawn_https_archive_server(body: Vec<u8>) -> HttpsArchiveServer {
    let cert = CertificateDer::from(pem_der(TEST_CERT_PEM, "CERTIFICATE"));
    let key = PrivateKeyDer::from(PrivatePkcs8KeyDer::from(pem_der(
        TEST_KEY_PEM,
        "PRIVATE KEY",
    )));
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .expect("tls config");
    let acceptor = TlsAcceptor::from(Arc::new(config));
    let (addr_tx, addr_rx) = std::sync::mpsc::sync_channel(1);
    let (shutdown_tx, shutdown_rx) = oneshot::channel();

    let handle = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind https test server");
            let addr = listener.local_addr().expect("addr");
            addr_tx.send(addr).expect("send addr");
            tokio::select! {
                accepted = listener.accept() => {
                    let (stream, _) = accepted.expect("accept");
                    let mut stream = acceptor.accept(stream).await.expect("tls accept");
                    let mut request = Vec::new();
                    let mut buf = [0u8; 1024];
                    loop {
                        let read = stream.read(&mut buf).await.expect("read request");
                        if read == 0 {
                            break;
                        }
                        request.extend_from_slice(&buf[..read]);
                        if request.windows(4).any(|window| window == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let headers = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/gzip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    stream.write_all(headers.as_bytes()).await.expect("write headers");
                    stream.write_all(&body).await.expect("write body");
                    stream.shutdown().await.expect("shutdown");
                }
                _ = shutdown_rx => {}
            }
        });
    });

    HttpsArchiveServer {
        addr: addr_rx.recv().expect("https server addr"),
        shutdown: Some(shutdown_tx),
        handle: Some(handle),
    }
}

#[test]
fn init_clones_git_code_source_into_usr_code() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let home = tempfile::tempdir().expect("home");
    let workspace = tempfile::tempdir().expect("workspace");
    let upstream = prepare_upstream();
    let upstream_url = upstream.path().display().to_string();
    let workspace_path = workspace.path().to_path_buf();

    acps_init(
        home.path(),
        &workspace_path,
        &["--code-from", &upstream_url],
    )
    .assert()
    .success();

    let dest_name = upstream.path().file_name().unwrap().to_str().unwrap();
    let dest = workspace_path.join("usr/code").join(dest_name);
    assert!(dest.join("README.md").is_file(), "clone landed");
    assert!(
        dest.join(".acp-stack-source.json").is_file(),
        "sentinel present"
    );
}

#[test]
fn init_materializes_local_data_source() {
    let home = tempfile::tempdir().expect("home");
    let workspace = tempfile::tempdir().expect("workspace");
    let source = tempfile::tempdir().expect("source");
    std::fs::write(source.path().join("dataset.txt"), b"alpha").expect("write");
    let source_path = source.path().display().to_string();
    let workspace_path = workspace.path().to_path_buf();

    acps_init(home.path(), &workspace_path, &["--data-from", &source_path])
        .assert()
        .success();

    let dest_name = source.path().file_name().unwrap().to_str().unwrap();
    let dest = workspace_path.join("usr/data").join(dest_name);
    assert_eq!(
        std::fs::read(dest.join("dataset.txt")).expect("dataset"),
        b"alpha"
    );
    assert!(dest.join(".acp-stack-source.json").is_file());
}

#[test]
fn init_downloads_and_extracts_https_archive_source() {
    let home = tempfile::tempdir().expect("home");
    let workspace = tempfile::tempdir().expect("workspace");
    let workspace_path = workspace.path().to_path_buf();
    let server = spawn_https_archive_server(archive_with_file("payload.txt", b"from https\n"));
    let url = server.url("/dataset.tar.gz");

    let mut first = acps_init(home.path(), &workspace_path, &["--data-from", &url]);
    first
        .env("ACP_STACK_TEST_INSECURE_HTTPS", "1")
        .assert()
        .success();

    let dest = workspace_path.join("usr/data").join("dataset");
    assert_eq!(
        std::fs::read(dest.join("payload.txt")).expect("payload"),
        b"from https\n"
    );
    assert!(dest.join(".acp-stack-source.json").is_file());

    let store = StateStore::open(default_state_path(home.path())).expect("state open");
    let run = store
        .latest_init_run()
        .expect("latest init run")
        .expect("init run exists");
    let steps = store.query_init_steps(&run.id).expect("init steps");
    let workspace_step = steps
        .iter()
        .find(|step| step.kind == step_kind::WORKSPACE_MATERIALIZE)
        .expect("workspace materialize step");
    let log_dir = workspace_step
        .log_dir
        .as_deref()
        .map(Path::new)
        .expect("workspace step log_dir");
    assert!(log_dir.is_dir(), "workspace log dir missing");
    let data_log_dir = log_dir.join("data-000");
    assert!(data_log_dir.is_dir(), "data source log dir missing");
    let captures = capture_names(&data_log_dir);
    assert!(
        has_capture(&captures, "download", ".stdout"),
        "download stdout missing under {}: {captures:?}",
        data_log_dir.display(),
    );
    assert!(
        has_capture(&captures, "download", ".stderr"),
        "download stderr missing under {}: {captures:?}",
        data_log_dir.display(),
    );
    assert!(
        has_capture(&captures, "extract", ".stdout"),
        "extract stdout missing under {}: {captures:?}",
        data_log_dir.display(),
    );
    assert!(
        has_capture(&captures, "extract", ".stderr"),
        "extract stderr missing under {}: {captures:?}",
        data_log_dir.display(),
    );

    let second = acps_init(home.path(), &workspace_path, &[])
        .env("ACP_STACK_TEST_INSECURE_HTTPS", "1")
        .assert()
        .success();
    let stdout = String::from_utf8(second.get_output().stdout.clone()).unwrap_or_default();
    assert!(
        stdout.contains("Verified"),
        "rerun did not verify existing HTTPS source: {stdout}"
    );
}

#[test]
fn init_rerun_is_idempotent_for_local_sources() {
    let home = tempfile::tempdir().expect("home");
    let workspace = tempfile::tempdir().expect("workspace");
    let source = tempfile::tempdir().expect("source");
    std::fs::write(source.path().join("dataset.txt"), b"x").expect("write");
    let source_path = source.path().display().to_string();
    let workspace_path = workspace.path().to_path_buf();

    acps_init(home.path(), &workspace_path, &["--data-from", &source_path])
        .assert()
        .success();
    // Second invocation must mark the source as Verified (sentinel match).
    // Accepting "Created" would let a regression slip through where the
    // sentinel path no-ops and a fresh materialization runs on every init.
    let second = acps_init(home.path(), &workspace_path, &[])
        .assert()
        .success();
    let stdout = String::from_utf8(second.get_output().stdout.clone()).unwrap_or_default();
    assert!(
        stdout.contains("Verified"),
        "stdout did not surface a Verified rerun outcome: {stdout}"
    );
    assert!(
        !stdout.contains("Created"),
        "rerun must not re-materialize the source: {stdout}"
    );
}

#[test]
fn init_rejects_http_data_from() {
    let home = tempfile::tempdir().expect("home");
    let workspace = tempfile::tempdir().expect("workspace");
    let workspace_path = workspace.path().to_path_buf();

    acps_init(
        home.path(),
        &workspace_path,
        &["--data-from", "http://example.com/dataset.tar.gz"],
    )
    .assert()
    .failure()
    .stderr(predicates::str::contains("https"));
}
