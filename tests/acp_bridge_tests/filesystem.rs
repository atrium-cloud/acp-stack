//! fs/* round-trips. The probe workspace root is `std::env::temp_dir()`, so per-test tempdirs created under it are inside the workspace.

use crate::support::{
    INVALID_PARAMS_CODE, RESOURCE_NOT_FOUND_CODE, open_test_state, run_terminal_probe,
};

#[tokio::test]
async fn fs_write_persists_to_disk_and_records_audit_event() {
    let dir = tempfile::tempdir().expect("tempdir");
    let target = dir.path().join("fs-probe.txt");
    let (_state_dir, state) = open_test_state();
    let (report, bridge, _sink) = run_terminal_probe(
        &[
            "--fs-write-path",
            &target.to_string_lossy(),
            "--fs-write-content",
            "hello fs",
            "--fs-read-path",
            &target.to_string_lossy(),
        ],
        Some(acp_stack::runtime::agent::acp_bridge::TerminalCommandLog {
            state: state.clone(),
            event_hub: acp_stack::events::EventHub::new(),
        }),
    )
    .await;
    assert_eq!(report["fs_write_ok"], true);
    assert_eq!(report["fs_read_content"], "hello fs");
    let on_disk = std::fs::read_to_string(&target).expect("file exists");
    assert_eq!(on_disk, "hello fs");
    bridge.shutdown().await.expect("shutdown ok");

    let events = state
        .lock()
        .await
        .query_events(acp_stack::state::LogFilter {
            limit: 50,
            kind: Some("fs.write"),
            source: Some("acp"),
            ..Default::default()
        })
        .expect("query events");
    assert_eq!(events.len(), 1, "expected one acp fs.write audit event");
    assert!(events[0].payload_json.contains("fs-probe.txt"));
}

#[tokio::test]
async fn fs_read_honors_line_and_limit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let target = dir.path().join("lines.txt");
    std::fs::write(&target, "one\ntwo\nthree\nfour\n").expect("seed file");
    let (report, bridge, _sink) = run_terminal_probe(
        &[
            "--fs-read-path",
            &target.to_string_lossy(),
            "--fs-read-line",
            "2",
            "--fs-read-limit",
            "2",
        ],
        None,
    )
    .await;
    assert_eq!(report["fs_read_content"], "two\nthree");
    bridge.shutdown().await.expect("shutdown ok");
}

#[tokio::test]
async fn fs_rejects_out_of_workspace_write_and_missing_file_read() {
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("does-not-exist.txt");
    let (report, bridge, _sink) = run_terminal_probe(
        &[
            "--fs-write-path",
            "/etc/acp-stack-escape-attempt",
            "--fs-read-path",
            &missing.to_string_lossy(),
        ],
        None,
    )
    .await;
    assert_eq!(report["fs_write_error_code"], INVALID_PARAMS_CODE);
    assert_eq!(report["fs_read_error_code"], RESOURCE_NOT_FOUND_CODE);
    bridge.shutdown().await.expect("shutdown ok");
}

#[cfg(unix)]
#[tokio::test]
async fn fs_write_rejects_symlink_target() {
    let dir = tempfile::tempdir().expect("tempdir");
    let outside = tempfile::NamedTempFile::new().expect("outside file");
    let link = dir.path().join("sneaky-link");
    std::os::unix::fs::symlink(outside.path(), &link).expect("symlink");
    let (report, bridge, _sink) =
        run_terminal_probe(&["--fs-write-path", &link.to_string_lossy()], None).await;
    assert_eq!(report["fs_write_error_code"], INVALID_PARAMS_CODE);
    bridge.shutdown().await.expect("shutdown ok");
}

// The strict placebo only touches fs/* when the client advertised both fs.readTextFile and
// fs.writeTextFile, so a non-skipped round-trip proves the capability is on the wire.
#[tokio::test]
async fn fs_round_trip_under_advertised_capability() {
    let dir = tempfile::tempdir().expect("tempdir");
    let target = dir.path().join("strict.txt");
    let (report, bridge, _sink) = run_terminal_probe(
        &[
            "--require-fs",
            "--fs-write-path",
            &target.to_string_lossy(),
            "--fs-write-content",
            "strict fs round trip",
            "--fs-read-path",
            &target.to_string_lossy(),
        ],
        None,
    )
    .await;
    assert_eq!(
        report.get("fs_skipped"),
        None,
        "strict agent skipped the fs probe; capability not advertised"
    );
    assert_eq!(report["fs_write_ok"], true);
    assert_eq!(report["fs_read_content"], "strict fs round trip");
    bridge.shutdown().await.expect("shutdown ok");
}
