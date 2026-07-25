use super::*;
use crate::config::load_config_from_str;
use crate::state::{LogFilter, STACK_UPDATE_OPERATION_INSTALL, STACK_UPDATE_STATUS_SUCCEEDED};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::thread;

// The fixture must advertise a version strictly newer than the running crate,
// otherwise the updater correctly reports `Skipped`. Deriving patch+1 from the
// crate version keeps the test valid across release version bumps.
fn target_version() -> String {
    let current = env!("CARGO_PKG_VERSION");
    let (rest, patch) = current
        .rsplit_once('.')
        .expect("crate version has major.minor.patch");
    let patch: u64 = patch.parse().expect("crate patch version is numeric");
    format!("{rest}.{}", patch + 1)
}

fn target_tag() -> String {
    format!("v{}", target_version())
}

const CONFIG: &str = r#"
[api]
bind = "127.0.0.1:7700"
public_url = "http://127.0.0.1:7700"
max_request_bytes = 1048576

[security.http]
max_request_bytes = 1048576
rate_limit_per_minute = 120
burst = 30
auth_failures_per_minute = 5
auth_block_duration = "15m"
allowed_origins = []
trust_proxy_headers = false
trusted_proxies = []

[workspace]
root = "/workspace"
uploads = "/workspace/uploads"
default_shell = "/bin/bash"
runtime_user = "acp"
max_file_bytes = 8388608

[logging]
level = "info"
local_retention_days = 30

[agent]
id = "placebo"
name = "Placebo"
command = "placebo-agent"
args = []
cwd = "/workspace"
env = []
restart = "on-crash"

[updates.acp_stack]
policy = "compatible"
frequency = "1d"
"#;

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn make_targz(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
        Vec::new(),
        flate2::Compression::default(),
    ));
    for (name, body) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(&mut header, name, *body)
            .expect("append archive entry");
    }
    builder
        .into_inner()
        .expect("finish tar")
        .finish()
        .expect("finish gzip")
}

// A minimal blocking HTTP/1.1 fixture: one request per connection, routed by
// path. Runs on a detached thread for the lifetime of the test process.
fn serve(listener: TcpListener, routes: HashMap<String, Vec<u8>>) {
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        let reader_stream = stream.try_clone().expect("clone fixture stream");
        let mut reader = BufReader::new(reader_stream);
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).is_err() {
            continue;
        }
        // Drain request headers up to the blank line; bodies are never sent.
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) if line == "\r\n" || line == "\n" => break,
                Ok(_) => continue,
                Err(_) => break,
            }
        }
        let path = request_line
            .split_whitespace()
            .nth(1)
            .unwrap_or("/")
            .to_owned();
        let response = match routes.get(&path) {
            Some(body) => {
                let mut out = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .into_bytes();
                out.extend_from_slice(body);
                out
            }
            None => {
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
            }
        };
        // Best-effort: a client that hung up early is fine for a fixture.
        if let Err(error) = stream.write_all(&response) {
            eprintln!("fixture server write failed: {error}");
        }
    }
}

#[test]
fn install_stack_update_downloads_and_swaps_binaries() {
    // Release binaries are Linux-only; `host_target` errors elsewhere, so
    // the swap path is only meaningful on Linux. Skip the body off-Linux
    // (the module still compiles everywhere so the test stays type-checked).
    if std::env::consts::OS != "linux" {
        eprintln!("skipping apply e2e test: stack release binaries are Linux-only");
        return;
    }
    // The updater refuses to install inside a container, which would defeat
    // the test. GitHub-hosted runners aren't containers, but guard anyway.
    if std::path::Path::new("/.dockerenv").exists() {
        eprintln!("skipping apply e2e test: running inside a container");
        return;
    }
    let target = match std::env::consts::ARCH {
        "x86_64" => "x86_64-unknown-linux-gnu",
        "aarch64" => "aarch64-unknown-linux-gnu",
        other => {
            eprintln!("skipping apply e2e test: unsupported arch {other}");
            return;
        }
    };
    // Mirror the real release artifact name (`acp-stack-<version>-<target>.tar.gz`,
    // built by scripts/build-release.sh) so the fixture exercises the
    // production naming contract rather than a stand-in.
    let target_version = target_version();
    let target_tag = target_tag();
    let tarball_name = format!("acp-stack-{target_version}-{target}.tar.gz");

    // Build the release artifacts the fixture serves.
    let tarball = make_targz(&[("acps", b"new-acps-binary")]);
    let tar_sha = sha256_hex(&tarball);

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture");
    let port = listener.local_addr().expect("fixture addr").port();
    let base = format!("http://127.0.0.1:{port}");

    let manifest = serde_json::json!({
        "schema_version": 1,
        "repository": REPOSITORY,
        "tag": target_tag,
        "version": target_version,
        "classification": "regular",
        "breaking": false,
        "artifacts": [{ "target": target, "archive": tarball_name, "sha256": tar_sha }],
    });
    let manifest_bytes = serde_json::to_vec(&manifest).expect("serialize manifest");
    let manifest_sha = sha256_hex(&manifest_bytes);
    let checksums = format!("{manifest_sha}  {MANIFEST_ASSET}\n").into_bytes();

    let release = serde_json::json!({
        "tag_name": target_tag,
        "prerelease": false,
        "assets": [
            { "name": MANIFEST_ASSET, "browser_download_url": format!("{base}/dl/{MANIFEST_ASSET}") },
            { "name": CHECKSUMS_ASSET, "browser_download_url": format!("{base}/dl/{CHECKSUMS_ASSET}") },
            { "name": tarball_name, "browser_download_url": format!("{base}/dl/{tarball_name}") },
        ],
    });
    let release_bytes = serde_json::to_vec(&release).expect("serialize release");

    let mut routes: HashMap<String, Vec<u8>> = HashMap::new();
    routes.insert(
        format!("/repos/{REPOSITORY}/releases/latest"),
        release_bytes,
    );
    routes.insert(format!("/dl/{MANIFEST_ASSET}"), manifest_bytes);
    routes.insert(format!("/dl/{CHECKSUMS_ASSET}"), checksums);
    routes.insert(format!("/dl/{tarball_name}"), tarball);

    // Pre-seed the install dir: the swap renames the existing files aside,
    // so they must already exist.
    let workdir = tempfile::tempdir().expect("tempdir");
    let bin_dir = workdir.path().join("bin");
    std::fs::create_dir(&bin_dir).expect("create bin dir");
    for binary in BINARIES {
        std::fs::write(bin_dir.join(binary), b"old").expect("seed old binary");
    }
    std::fs::write(bin_dir.join("acpctl"), b"old-acpctl").expect("seed old acpctl");

    // Activate the fixture seams. SAFETY: set before any other thread is
    // spawned in this test; removed before the test returns. No other test
    // reads these vars (only the install path does, which only this test
    // drives).
    unsafe {
        std::env::set_var(GITHUB_API_BASE_ENV, &base);
        std::env::set_var(
            INSTALL_BINARY_DIR_ENV,
            bin_dir.to_str().expect("utf-8 bin dir"),
        );
    }
    thread::spawn(move || serve(listener, routes));

    let config = load_config_from_str(CONFIG).expect("config parses");
    let store = StateStore::open(workdir.path().join("state.sqlite")).expect("state open");
    store.migrate().expect("migrate");

    let result = install_stack_update(
        &config,
        &store,
        StackUpdateOptions {
            target: StackUpdateTarget::Latest,
            version: None,
            allow_breaking: false,
            auto: false,
        },
    );

    // Clear the seams before asserting so a failed assert can't leak env.
    unsafe {
        std::env::remove_var(GITHUB_API_BASE_ENV);
        std::env::remove_var(INSTALL_BINARY_DIR_ENV);
    }

    let report = result.expect("install should succeed");
    assert_eq!(report.status, StackUpdateStatus::Installed);
    assert_eq!(
        report.target_version.as_deref(),
        Some(target_version.as_str())
    );
    assert_eq!(report.target_tag.as_deref(), Some(target_tag.as_str()));

    assert_eq!(
        std::fs::read(bin_dir.join("acps")).expect("read acps"),
        b"new-acps-binary"
    );
    assert!(
        !bin_dir.join("acpctl").exists(),
        "stale acpctl should be removed"
    );
    let runs = store.query_stack_update_runs(10).expect("runs");
    assert!(
        runs.iter()
            .any(|run| run.operation == STACK_UPDATE_OPERATION_INSTALL
                && run.status == STACK_UPDATE_STATUS_SUCCEEDED),
        "expected a succeeded install run, got {runs:?}"
    );

    let events = store
        .query_events(LogFilter {
            limit: 10,
            kind: Some("stack.update.installed"),
            ..LogFilter::default()
        })
        .expect("events");
    assert_eq!(events.len(), 1);
}
