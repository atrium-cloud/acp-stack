use super::*;

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};

use sha2::{Digest, Sha256};

const LINUX_X64: NodePlatform<'static> = NodePlatform {
    os: "linux",
    arch: "x86_64",
};
const UNREACHABLE_DIST_BASE: &str = "http://127.0.0.1:1";
const RELEASE: &str = "node-v26.1.0-linux-x64";
const OLDER_RELEASE: &str = "node-v26.0.0-linux-x64";

struct Fixture {
    base: String,
    requests: Arc<AtomicUsize>,
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// A Node-shaped release: `bin/node` is a shell script reporting `version`, and `bin/npm` is a
/// relative symlink into the bundled npm, as in the upstream tarballs.
fn release_archive(release: &str, version: &str) -> Vec<u8> {
    let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
        Vec::new(),
        flate2::Compression::default(),
    ));
    let files: [(String, Vec<u8>, u32); 2] = [
        (
            format!("{release}/bin/node"),
            format!("#!/bin/sh\necho {version}\n").into_bytes(),
            0o755,
        ),
        (
            format!("{release}/lib/node_modules/npm/bin/npm-cli.js"),
            b"// npm\n".to_vec(),
            0o644,
        ),
    ];
    for (path, body, mode) in &files {
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(*mode);
        header.set_cksum();
        builder
            .append_data(&mut header, path, body.as_slice())
            .expect("append file");
    }
    let mut link = tar::Header::new_gnu();
    link.set_entry_type(tar::EntryType::Symlink);
    link.set_size(0);
    link.set_mode(0o777);
    builder
        .append_link(
            &mut link,
            format!("{release}/bin/npm"),
            "../lib/node_modules/npm/bin/npm-cli.js",
        )
        .expect("append npm symlink");
    builder
        .into_inner()
        .expect("finish tar")
        .finish()
        .expect("finish gzip")
}

fn channel_path(name: &str) -> String {
    format!("/latest-v{NODE_MAJOR}.x/{name}")
}

/// Serve `SHASUMS256.txt` plus the archive for `release`; `listed_sha256` overrides the listed
/// checksum to simulate a tampered archive.
fn serve_release(release: &str, version: &str, listed_sha256: Option<&str>) -> Fixture {
    let archive = release_archive(release, version);
    let archive_name = format!("{release}{ARCHIVE_SUFFIX}");
    let sha256 = listed_sha256
        .map(str::to_owned)
        .unwrap_or_else(|| sha256_hex(&archive));
    let checksums = format!(
        "{other}  node-v26.1.0-linux-arm64.tar.gz\n{other}  {release}.tar.xz\n{sha256}  {archive_name}\n",
        other = "0".repeat(SHA256_HEX_LEN),
    );
    let routes = HashMap::from([
        (channel_path(CHECKSUMS_FILE_NAME), checksums.into_bytes()),
        (channel_path(&archive_name), archive),
    ]);
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture");
    let base = format!("http://{}", listener.local_addr().expect("fixture addr"));
    let requests = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&requests);
    std::thread::spawn(move || serve(listener, routes, counter));
    Fixture { base, requests }
}

// Minimal blocking HTTP/1.1 fixture: one request per connection, routed by path.
fn serve(listener: TcpListener, routes: HashMap<String, Vec<u8>>, requests: Arc<AtomicUsize>) {
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        requests.fetch_add(1, Ordering::SeqCst);
        let mut reader = BufReader::new(stream.try_clone().expect("clone fixture stream"));
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).is_err() {
            continue;
        }
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) if line == "\r\n" || line == "\n" => break,
                Ok(_) => continue,
                Err(_) => break,
            }
        }
        let path = request_line.split_whitespace().nth(1).unwrap_or("/");
        let response = match routes.get(path) {
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
        if let Err(error) = stream.write_all(&response) {
            eprintln!("fixture server write failed: {error}");
        }
    }
}

/// Seed an installed release on disk, as a completed earlier ensure leaves it.
fn seed_release(home: &Path, release: &str, version: &str) {
    let bin = managed_root(home)
        .join(RELEASES_DIR_NAME)
        .join(release)
        .join("bin");
    std::fs::create_dir_all(&bin).expect("seed bin");
    std::fs::write(bin.join("node"), format!("#!/bin/sh\necho {version}\n")).expect("seed node");
    replace_symlink_atomically(
        &Path::new(RELEASES_DIR_NAME).join(release),
        &managed_root(home).join(CURRENT_LINK_NAME),
    )
    .expect("seed current");
}

fn current_release(home: &Path) -> Option<String> {
    std::fs::read_link(managed_root(home).join(CURRENT_LINK_NAME))
        .ok()
        .and_then(|target| {
            target
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
}

#[test]
fn fresh_install_swaps_current_writes_npm_prefix_and_links_tools() {
    let home = tempfile::tempdir().expect("home");
    let fixture = serve_release(RELEASE, "v26.1.0", None);

    let status = ensure_for_platform(home.path(), LINUX_X64, &fixture.base, None).expect("ensure");

    assert_eq!(
        status,
        NodeRuntimeStatus::Ready {
            version: "v26.1.0".to_owned()
        }
    );
    assert_eq!(current_release(home.path()).as_deref(), Some(RELEASE));
    let npmrc =
        std::fs::read_to_string(managed_bin_dir(home.path()).join("../lib/node_modules/npm/npmrc"))
            .expect("npm builtin config");
    assert_eq!(
        npmrc,
        format!("prefix={}\n", home.path().join(".local").display())
    );
    let local_bin = crate::runtime::install::local_bin_dir(home.path());
    for tool in ["node", "npm"] {
        assert_eq!(
            std::fs::read_link(local_bin.join(tool)).expect("tool link"),
            managed_bin_dir(home.path()).join(tool)
        );
    }
    assert!(
        std::fs::symlink_metadata(local_bin.join("npx")).is_err(),
        "a tool the release lacks is not linked"
    );
    assert_eq!(installed_version(home.path()).as_deref(), Some("v26.1.0"));
}

#[test]
fn installed_release_takes_the_offline_fast_path() {
    let home = tempfile::tempdir().expect("home");
    seed_release(home.path(), OLDER_RELEASE, "v26.0.0");

    let status =
        ensure_for_platform(home.path(), LINUX_X64, UNREACHABLE_DIST_BASE, None).expect("ensure");

    assert_eq!(
        status,
        NodeRuntimeStatus::Ready {
            version: "v26.0.0".to_owned()
        }
    );
}

#[test]
fn checksum_mismatch_is_typed_and_leaves_no_current() {
    let home = tempfile::tempdir().expect("home");
    let wrong = "f".repeat(SHA256_HEX_LEN);
    let fixture = serve_release(RELEASE, "v26.1.0", Some(&wrong));

    let error = ensure_for_platform(home.path(), LINUX_X64, &fixture.base, None)
        .expect_err("tampered archive must fail");

    match error {
        StackError::NodeRuntimeChecksumMismatch {
            archive, expected, ..
        } => {
            assert_eq!(archive, format!("{RELEASE}{ARCHIVE_SUFFIX}"));
            assert_eq!(expected, wrong);
        }
        other => panic!("expected a checksum mismatch, got {other:?}"),
    }
    assert_eq!(current_release(home.path()), None);
    let releases = std::fs::read_dir(managed_root(home.path()).join(RELEASES_DIR_NAME))
        .expect("releases dir")
        .count();
    assert_eq!(releases, 0, "the staging dir is cleaned up");
}

#[test]
fn checksum_mismatch_keeps_an_existing_current() {
    let home = tempfile::tempdir().expect("home");
    seed_release(home.path(), "node-v25.9.0-linux-x64", "v25.9.0");
    let fixture = serve_release(RELEASE, "v26.1.0", Some(&"f".repeat(SHA256_HEX_LEN)));

    ensure_for_platform(home.path(), LINUX_X64, &fixture.base, None)
        .expect_err("tampered archive must fail");

    assert_eq!(
        current_release(home.path()).as_deref(),
        Some("node-v25.9.0-linux-x64")
    );
}

#[test]
fn older_major_is_replaced_keeping_the_previous_release_and_pruning_the_rest() {
    let home = tempfile::tempdir().expect("home");
    seed_release(home.path(), "node-v22.23.3-linux-x64", "v22.23.3");
    seed_release(home.path(), "node-v24.21.0-linux-x64", "v24.21.0");
    let fixture = serve_release(RELEASE, "v26.1.0", None);

    ensure_for_platform(home.path(), LINUX_X64, &fixture.base, None).expect("ensure");

    let mut remaining: Vec<String> =
        std::fs::read_dir(managed_root(home.path()).join(RELEASES_DIR_NAME))
            .expect("releases dir")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
    remaining.sort();
    assert_eq!(remaining, ["node-v24.21.0-linux-x64", RELEASE]);
    assert_eq!(current_release(home.path()).as_deref(), Some(RELEASE));
}

#[test]
fn legacy_links_are_repointed_and_the_legacy_root_kept() {
    let home = tempfile::tempdir().expect("home");
    // The recipe-installed Node 22 root; `npm -g` packages installed under it must survive.
    let legacy_root = crate::secrets::state_dir(home.path()).join("node");
    let legacy_bin = legacy_root.join("current").join("bin");
    std::fs::create_dir_all(&legacy_bin).expect("legacy root");
    let local_bin = crate::runtime::install::local_bin_dir(home.path());
    std::fs::create_dir_all(&local_bin).expect("local bin");
    replace_symlink_atomically(&legacy_bin.join("node"), &local_bin.join("node"))
        .expect("legacy link");
    seed_release(home.path(), RELEASE, "v26.1.0");

    ensure_for_platform(home.path(), LINUX_X64, UNREACHABLE_DIST_BASE, None).expect("ensure");

    assert_eq!(
        std::fs::read_link(local_bin.join("node")).expect("node link"),
        managed_bin_dir(home.path()).join("node")
    );
    assert!(legacy_bin.is_dir());
}

#[test]
fn a_failed_install_is_not_retried_inside_the_cooldown() {
    let home = tempfile::tempdir().expect("home");
    let tampered = serve_release(RELEASE, "v26.1.0", Some(&"f".repeat(SHA256_HEX_LEN)));
    ensure_for_platform(home.path(), LINUX_X64, &tampered.base, None)
        .expect_err("tampered archive must fail");
    let healthy = serve_release(RELEASE, "v26.1.0", None);

    let error = ensure_for_platform(home.path(), LINUX_X64, &healthy.base, None)
        .expect_err("a retry inside the cooldown must be skipped");

    assert!(
        matches!(error, StackError::NodeRuntimeInstallFailed { ref reason } if reason.contains("sha256")),
        "{error:?}"
    );
    assert_eq!(healthy.requests.load(Ordering::SeqCst), 0);

    std::fs::remove_file(managed_root(home.path()).join(FAILED_INSTALL_MARKER_NAME))
        .expect("expire the cooldown");
    ensure_for_platform(home.path(), LINUX_X64, &healthy.base, None).expect("retry installs");
    assert!(
        !managed_root(home.path())
            .join(FAILED_INSTALL_MARKER_NAME)
            .exists(),
        "a successful install clears the failure"
    );
}

#[test]
fn a_regular_file_at_the_link_path_is_left_alone() {
    let home = tempfile::tempdir().expect("home");
    let local_bin = crate::runtime::install::local_bin_dir(home.path());
    std::fs::create_dir_all(&local_bin).expect("local bin");
    std::fs::write(local_bin.join("node"), "operator-owned").expect("operator node");
    seed_release(home.path(), RELEASE, "v26.1.0");

    ensure_for_platform(home.path(), LINUX_X64, UNREACHABLE_DIST_BASE, None).expect("ensure");

    assert_eq!(
        std::fs::read_to_string(local_bin.join("node")).expect("operator node"),
        "operator-owned"
    );
}

#[test]
fn unsupported_platform_touches_nothing() {
    let home = tempfile::tempdir().expect("home");
    for platform in [
        NodePlatform {
            os: "macos",
            arch: "aarch64",
        },
        NodePlatform {
            os: "linux",
            arch: "riscv64",
        },
    ] {
        let status = ensure_for_platform(home.path(), platform, UNREACHABLE_DIST_BASE, None)
            .expect("ensure");
        assert!(matches!(status, NodeRuntimeStatus::Unsupported { .. }));
    }
    assert!(!managed_root(home.path()).exists());
}

#[test]
fn wait_ready_never_downloads_or_creates_the_root() {
    let home = tempfile::tempdir().expect("home");
    let fixture = serve_release(RELEASE, "v26.1.0", None);

    assert_eq!(
        wait_ready_for_platform(home.path(), LINUX_X64),
        NodeRuntimeStatus::NotReady
    );
    assert!(!managed_root(home.path()).exists());

    ensure_for_platform(home.path(), LINUX_X64, &fixture.base, None).expect("ensure");
    let requests_after_install = fixture.requests.load(Ordering::SeqCst);
    assert_eq!(
        wait_ready_for_platform(home.path(), LINUX_X64),
        NodeRuntimeStatus::Ready {
            version: "v26.1.0".to_owned()
        }
    );
    assert_eq!(
        fixture.requests.load(Ordering::SeqCst),
        requests_after_install
    );
}

#[test]
fn startup_lock_blocks_a_second_holder() {
    let home = tempfile::tempdir().expect("home");
    let root = managed_root(home.path());
    std::fs::create_dir_all(&root).expect("root");
    let lock_path = root.join(LOCK_FILE_NAME);

    let held = try_acquire_exclusive_lock_file(&lock_path)
        .expect("lock")
        .expect("first holder gets the lock");
    assert!(
        try_acquire_exclusive_lock_file(&lock_path)
            .expect("second try")
            .is_none()
    );
    drop(held);
    assert!(
        try_acquire_exclusive_lock_file(&lock_path)
            .expect("third try")
            .is_some()
    );
}

#[test]
fn archive_selection_matches_only_the_major_and_arch() {
    let sha = "a".repeat(SHA256_HEX_LEN);
    let listing = format!(
        "{sha}  node-v26.1.0-linux-arm64.tar.gz\n\
         {sha}  node-v26.1.0-linux-x64.tar.xz\n\
         {sha}  node-v24.0.0-linux-x64.tar.gz\n\
         {sha}  ../node-v26.1.0-linux-x64.tar.gz\n\
         short  node-v26.1.0-linux-x64.tar.gz\n\
         {sha} *node-v26.1.0-linux-x64.tar.gz\n"
    );

    assert_eq!(
        select_archive(&listing, "x64"),
        Some(("node-v26.1.0-linux-x64.tar.gz".to_owned(), sha.clone()))
    );
    assert_eq!(
        select_archive(&listing, "arm64"),
        Some(("node-v26.1.0-linux-arm64.tar.gz".to_owned(), sha))
    );
    assert_eq!(select_archive("", "x64"), None);
}

#[test]
fn installed_version_ignores_another_major_and_a_missing_node() {
    let home = tempfile::tempdir().expect("home");
    assert_eq!(installed_version(home.path()), None);

    seed_release(home.path(), "node-v24.21.0-linux-x64", "v24.21.0");
    assert_eq!(installed_version(home.path()), None);

    seed_release(home.path(), RELEASE, "v26.1.0");
    std::fs::remove_file(managed_bin_dir(home.path()).join("node")).expect("remove node");
    assert_eq!(installed_version(home.path()), None);
}

#[test]
fn outcome_state_defaults_to_unmanaged_and_round_trips() {
    let state = NodeRuntimeState::default();
    assert!(matches!(state.get(), NodeRuntimeOutcome::Unmanaged));

    state.set(NodeRuntimeOutcome::Pending);
    assert!(matches!(state.clone().get(), NodeRuntimeOutcome::Pending));
}
