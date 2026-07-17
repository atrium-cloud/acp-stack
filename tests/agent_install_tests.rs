#![cfg(feature = "test-fixtures")]

//! End-to-end install flow against a mocked GitHub Releases API.
//!
//! Phase 4 / Test Hardening: the `install_resolved_capture` two-step
//! flow (harness install + adapter install for adapter-backed entries)
//! has unit tests for each install path in isolation, but no integration
//! test that exercises the production GitHub Release driver against a
//! deterministic mock. This file plugs that gap.
//!
//! The mock is an axum server bound to `127.0.0.1:0` serving:
//!
//! - `GET /repos/test-owner/<repo>/releases/latest` — release JSON whose
//!   asset URLs point back at the same mock so the asset download stays
//!   in-process.
//! - `GET /assets/<filename>` — raw asset bytes (a tiny executable so
//!   the `creates` postcheck resolves a real file on PATH).
//!
//! `ACP_STACK_GITHUB_API_BASE` redirects `github_release::fetch_release`
//! at the mock; the rest of the install path (asset matching, byte
//! download, raw-binary write, sha256 hashing, final `creates` probe)
//! runs unchanged.

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::sync::{Mutex, mpsc};
use std::thread;

use acp_stack::config::AgentConfig;
use acp_stack::runtime::install::agent_installer::install_resolved_capture;
use acp_stack::runtime::install::agent_registry::{
    AdapterSpec, ArchMap, ArchiveKind, GithubInstall, HarnessSpec, InstallSet, RegistryEntry,
    RegistryKind, RegistryStdioFraming,
};
use axum::Router;
use axum::extract::{Path as AxPath, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use serde_json::json;

/// Cargo runs `#[test]` functions in parallel inside one binary, and
/// `ACP_STACK_GITHUB_API_BASE` is a process-wide env var that the
/// installer threads read. A naïve `set_var` per test would let two
/// tests step on each other (test A sees test B's mock URL, or the
/// var disappears mid-flight when B's RAII guard drops). Hold this
/// mutex for the duration of any test that mutates env so the env
/// state is per-test serialized.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// RAII guard that sets a single env var on construction and restores
/// the prior value on drop. Holds `ENV_LOCK` for its lifetime, which
/// guarantees only one test at a time mutates the var even if a panic
/// inside the test unwinds.
struct EnvGuard<'a> {
    _lock: std::sync::MutexGuard<'a, ()>,
    key: &'static str,
    previous: Option<std::ffi::OsString>,
}

impl<'a> EnvGuard<'a> {
    fn new(key: &'static str, value: &str) -> Self {
        let lock = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = std::env::var_os(key);
        // SAFETY: `std::env::set_var` is unsafe in the Rust 2024
        // edition because it mutates process-wide environment state
        // that other threads may concurrently read. Two threads exist
        // in this test binary:
        //   1. Other `#[test]` threads: `ENV_LOCK` (held for the full
        //      lifetime of this guard) serializes them. While we hold
        //      the lock, no other test is inside an `EnvGuard`.
        //   2. Per-test mock-server threads: these are pure axum
        //      handlers that never read this env var or any GitHub
        //      env var. They cannot observe the mutation.
        // Production code reads the env var once at the entry of
        // `github_release::install`/`latest_release_tag`, then uses
        // the resolved value for both the token decision and the URL,
        // so a concurrent flip between reads cannot break invariants.
        unsafe { std::env::set_var(key, value) };
        Self {
            _lock: lock,
            key,
            previous,
        }
    }
}

impl Drop for EnvGuard<'_> {
    fn drop(&mut self) {
        // SAFETY: ENV_LOCK still held (drop fires before _lock
        // releases). Same threading argument as `new`. Restore the
        // prior value, or remove the var if it was unset coming in,
        // so the next test sees a clean slate.
        unsafe {
            match &self.previous {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

const HARNESS_REPO: &str = "test-owner/harness-repo";
const ADAPTER_REPO: &str = "test-owner/adapter-repo";
const HARNESS_BIN: &str = "fake-harness";
const ADAPTER_BIN: &str = "fake-adapter";
const HARNESS_ASSET: &str = "fake-harness";
const ADAPTER_ASSET: &str = "fake-adapter";
const HARNESS_TAG: &str = "v0.4.2";
const ADAPTER_TAG: &str = "v1.0.0";

/// Mock GitHub releases server. `addr` is the bind address; routes serve
/// release JSON and asset bytes. Stays bound for the lifetime of the
/// test process — the OS reclaims the socket when the test binary exits.
struct MockGithub {
    addr: SocketAddr,
}

#[derive(Clone)]
struct MockState {
    base_url: String,
}

fn binary_bytes(label: &str) -> Vec<u8> {
    // Minimal shell-script "binary" — enough to satisfy a `creates`
    // probe that just checks `is_file()`. We don't actually exec the
    // result, so a script with the correct shebang is fine.
    format!("#!/bin/sh\necho {label}\n").into_bytes()
}

async fn release_handler(
    State(state): State<MockState>,
    AxPath((owner, repo)): AxPath<(String, String)>,
) -> impl IntoResponse {
    let full = format!("{owner}/{repo}");
    let (tag, asset_name, asset_bytes) = match full.as_str() {
        HARNESS_REPO => (HARNESS_TAG, HARNESS_ASSET, binary_bytes("harness")),
        ADAPTER_REPO => (ADAPTER_TAG, ADAPTER_ASSET, binary_bytes("adapter")),
        _ => return (StatusCode::NOT_FOUND, "unknown repo").into_response(),
    };
    let body = json!({
        "tag_name": tag,
        "assets": [
            {
                "name": asset_name,
                "browser_download_url": format!("{}/assets/{}", state.base_url, asset_name),
                "size": asset_bytes.len() as u64,
            }
        ]
    });
    axum::Json(body).into_response()
}

async fn asset_handler(AxPath(filename): AxPath<String>) -> impl IntoResponse {
    let bytes = match filename.as_str() {
        HARNESS_ASSET => binary_bytes("harness"),
        ADAPTER_ASSET => binary_bytes("adapter"),
        _ => return (StatusCode::NOT_FOUND, "unknown asset").into_response(),
    };
    (
        StatusCode::OK,
        [("content-type", "application/octet-stream")],
        bytes,
    )
        .into_response()
}

fn start_mock_github() -> MockGithub {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build runtime");
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            let addr = listener.local_addr().expect("addr");
            let base_url = format!("http://{addr}");
            let state = MockState {
                base_url: base_url.clone(),
            };
            let router = Router::new()
                .route(
                    "/repos/{owner}/{repo}/releases/latest",
                    get(release_handler),
                )
                .route("/assets/{filename}", get(asset_handler))
                .with_state(state);
            tx.send(addr).expect("send addr");
            axum::serve(listener, router).await.expect("serve");
        });
    });
    let addr = rx.recv().expect("recv addr");
    MockGithub { addr }
}

fn adapter_kind_entry() -> RegistryEntry {
    RegistryEntry {
        id: "fake-agent".to_owned(),
        name: "Fake Agent".to_owned(),
        kind: RegistryKind::Adapter,
        headless_compatible: true,
        set_provider: false,
        multiple_active_providers: false,
        set_model: false,
        allow_custom_provider: false,
        allow_custom_model: false,
        set_mode: false,
        supports_mcp: true,
        supports_agent_skills: true,
        agent_skills_install_dir: Some("~/.agents/skills".to_owned()),
        subagents: false,
        subagent_alias: None,
        subagent_free_models: Vec::new(),
        stdio_framing: RegistryStdioFraming::JsonLines,
        website: None,
        github: Some(format!("https://github.com/{HARNESS_REPO}")),
        support_doc: Some("docs/agents/fake-agent.md".to_owned()),
        testflight_prompt: None,
        testflight_expect_fs: None,
        harness: Some(HarnessSpec {
            id: HARNESS_BIN.to_owned(),
            install: InstallSet {
                github: Some(GithubInstall {
                    asset_pattern: HARNESS_ASSET.to_owned(),
                    archive: ArchiveKind::None,
                    archive_binary_name: None,
                    binary_name: HARNESS_BIN.to_owned(),
                    checksums_asset: None,
                    arch: ArchMap {
                        x86_64: Some("x86_64".to_owned()),
                        aarch64: Some("aarch64".to_owned()),
                    },
                }),
                ..InstallSet::default()
            },
            update: Default::default(),
        }),
        adapter: Some(AdapterSpec {
            id: ADAPTER_BIN.to_owned(),
            sync_id: None,
            github: Some(format!("https://github.com/{ADAPTER_REPO}")),
            install: InstallSet {
                github: Some(GithubInstall {
                    asset_pattern: ADAPTER_ASSET.to_owned(),
                    archive: ArchiveKind::None,
                    archive_binary_name: None,
                    binary_name: ADAPTER_BIN.to_owned(),
                    checksums_asset: None,
                    arch: ArchMap {
                        x86_64: Some("x86_64".to_owned()),
                        aarch64: Some("aarch64".to_owned()),
                    },
                }),
                ..InstallSet::default()
            },
            update: Default::default(),
        }),
    }
}

fn agent_config(command: &str) -> AgentConfig {
    AgentConfig {
        id: "fake-agent".to_owned(),
        name: "Fake Agent".to_owned(),
        command: command.to_owned(),
        args: Vec::new(),
        cwd: None,
        env: Vec::new(),
        expected_sha256: None,
        restart: "on-crash".to_owned(),
        mode: None,
        model: None,
        harness_version: None,
        adapter: None,
        provider: None,
        providers: None,
        subagent: None,
        auto_update: None,
        install: None,
    }
}

#[test]
fn install_resolved_two_step_flow_against_mocked_github_api() {
    let mock = start_mock_github();
    let dest_dir = tempfile::tempdir().expect("dest tempdir");

    let _env = EnvGuard::new(
        "ACP_STACK_GITHUB_API_BASE",
        &format!("http://{}", mock.addr),
    );
    let entry = adapter_kind_entry();
    // The final verification step needs `agent.command` to resolve. The
    // adapter is what speaks ACP from the runtime's POV, so it's the
    // command. install_resolved_capture writes both binaries into
    // dest_dir.
    let result = install_resolved_capture(
        &agent_config(ADAPTER_BIN),
        &entry,
        std::collections::HashMap::new(),
        dest_dir.path(),
        dest_dir.path(),
    );

    let outcome = result
        .outcome
        .expect("two-step install against the mock should succeed");
    match outcome {
        acp_stack::runtime::install::agent_installer::InstallerOutcome::Installed {
            path, ..
        }
        | acp_stack::runtime::install::agent_installer::InstallerOutcome::AlreadyPresent {
            path,
            ..
        } => {
            assert_eq!(
                path.file_name().and_then(|n| n.to_str()),
                Some(ADAPTER_BIN),
                "final outcome path should point at the adapter binary",
            );
        }
    }

    // Two `installer_runs` drafts: one harness row, one adapter row.
    assert_eq!(
        result.rows.len(),
        2,
        "adapter-kind installs must record both harness AND adapter rows",
    );
    let harness_row = result
        .rows
        .iter()
        .find(|r| r.step == "harness")
        .expect("harness row missing");
    let adapter_row = result
        .rows
        .iter()
        .find(|r| r.step == "adapter")
        .expect("adapter row missing");

    assert_eq!(harness_row.status, "ran");
    assert_eq!(adapter_row.status, "ran");
    assert_eq!(
        harness_row.version.as_deref(),
        Some(HARNESS_TAG),
        "harness row should record the release tag returned by the mock",
    );
    assert_eq!(
        adapter_row.version.as_deref(),
        Some(ADAPTER_TAG),
        "adapter row should record the release tag returned by the mock",
    );

    // Both binaries landed at dest_dir with executable bit.
    let harness_path = dest_dir.path().join(HARNESS_BIN);
    let adapter_path = dest_dir.path().join(ADAPTER_BIN);
    assert!(harness_path.is_file(), "harness binary missing");
    assert!(adapter_path.is_file(), "adapter binary missing");
    let harness_mode = std::fs::metadata(&harness_path)
        .expect("stat harness")
        .permissions()
        .mode()
        & 0o111;
    assert!(
        harness_mode != 0,
        "harness binary should be executable, got mode {harness_mode:o}",
    );
    let adapter_mode = std::fs::metadata(&adapter_path)
        .expect("stat adapter")
        .permissions()
        .mode()
        & 0o111;
    assert!(
        adapter_mode != 0,
        "adapter binary should be executable, got mode {adapter_mode:o}",
    );
}

#[test]
fn install_resolved_records_failure_when_release_endpoint_missing() {
    // Mock declares only the adapter repo; the harness repo's release
    // endpoint returns 404. The harness step fails, the adapter step
    // never completes a successful install (because both run
    // concurrently and either failure aborts the overall outcome), and
    // both rows are persisted for audit.
    let mock = start_mock_github();
    let dest_dir = tempfile::tempdir().expect("dest tempdir");

    let _env = EnvGuard::new(
        "ACP_STACK_GITHUB_API_BASE",
        &format!("http://{}", mock.addr),
    );
    // Point the harness at a repo the mock does NOT know about.
    let mut entry = adapter_kind_entry();
    entry.github = Some("https://github.com/test-owner/missing-repo".to_owned());
    let result = install_resolved_capture(
        &agent_config(ADAPTER_BIN),
        &entry,
        std::collections::HashMap::new(),
        dest_dir.path(),
        dest_dir.path(),
    );

    assert!(
        result.outcome.is_err(),
        "harness 404 must surface as a failed outcome, got {:?}",
        result.outcome,
    );
    // Both rows persisted — the audit log must show what each side did.
    let harness_row = result
        .rows
        .iter()
        .find(|r| r.step == "harness")
        .expect("harness row should be persisted even on failure");
    // Status sentinel for non-zero installer outcomes is `error`
    // (see InstallerRowDraft::config_error / the install machinery's
    // failure branches); `failed` is the init_steps sentinel, not the
    // installer_runs one.
    assert!(
        matches!(harness_row.status.as_str(), "error" | "failed"),
        "harness row should record a non-success status, got `{}`",
        harness_row.status,
    );
    // The adapter side runs concurrently against the still-reachable
    // adapter repo and may either succeed or be cut short by the
    // harness failure; either way an `installer_runs` row for it must
    // exist so the operator can see what happened on both halves.
    let adapter_row = result
        .rows
        .iter()
        .find(|r| r.step == "adapter")
        .expect("adapter row should be persisted even when the sibling step fails");
    assert!(
        matches!(adapter_row.status.as_str(), "ran" | "error" | "failed"),
        "adapter row should carry a known status, got `{}`",
        adapter_row.status,
    );
}
