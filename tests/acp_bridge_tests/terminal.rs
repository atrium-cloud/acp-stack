//! terminal/* round-trips, read back out of the sink as the placebo's
//! `terminal-report:{json}` message chunk.

use crate::support::{
    INVALID_PARAMS_CODE, RESOURCE_NOT_FOUND_CODE, open_test_state, run_terminal_probe,
};

#[tokio::test]
async fn terminal_create_returns_output_and_records_acp_command() {
    let (_tempdir, state) = open_test_state();
    let event_hub = acp_stack::events::EventHub::new();
    // The hub is a broadcast channel, so subscribe before the probe runs.
    let mut live_events = event_hub.subscribe();
    let (report, bridge, _sink) = run_terminal_probe(
        &[
            "--terminal-command",
            "printf",
            "--terminal-arg",
            "hi-terminal",
        ],
        Some(acp_stack::runtime::agent::acp_bridge::TerminalCommandLog {
            state: state.clone(),
            event_hub: event_hub.clone(),
        }),
    )
    .await;
    assert_eq!(report["exit_code"], 0);
    assert_eq!(report["signal"], serde_json::Value::Null);
    assert_eq!(report["output"], "hi-terminal");
    assert_eq!(report["truncated"], false);
    assert_eq!(report["post_release_error_code"], RESOURCE_NOT_FOUND_CODE);
    bridge.shutdown().await.expect("shutdown ok");

    let commands = state
        .lock()
        .await
        .query_commands(acp_stack::state::CommandFilter {
            limit: 10,
            ..Default::default()
        })
        .expect("query commands");
    let row = commands
        .iter()
        .find(|row| row.origin == "acp")
        .expect("acp-origin command row recorded");
    assert_eq!(row.command, "printf hi-terminal");
    assert_eq!(row.status, "exited");
    assert!(row.session_id.is_some());

    let topic = format!("commands.{}", row.id);
    let mut saw_output_chunk = false;
    let mut saw_exited = false;
    while let Ok(event) = live_events.try_recv() {
        if event.topic != topic {
            continue;
        }
        let kind = event.payload["kind"].as_str().unwrap_or_default();
        if kind.ends_with(".stdout")
            && event.payload["data"]["data"]
                .as_str()
                .is_some_and(|data| data.contains("hi-terminal"))
        {
            saw_output_chunk = true;
        }
        if kind == "command.exited" {
            saw_exited = true;
        }
    }
    assert!(saw_output_chunk, "missing live stdout chunk on {topic}");
    assert!(saw_exited, "missing live command.exited on {topic}");
}

#[tokio::test]
async fn terminal_create_confines_cwd_outside_workspace() {
    let (report, bridge, _sink) = run_terminal_probe(
        &[
            "--terminal-command",
            "printf",
            "--terminal-arg",
            "never-runs",
            "--terminal-cwd",
            "/",
        ],
        None,
    )
    .await;
    assert_eq!(report["create_error_code"], INVALID_PARAMS_CODE);
    bridge.shutdown().await.expect("shutdown ok");
}

#[tokio::test]
async fn terminal_output_truncates_to_newest_bytes_at_byte_limit() {
    let (report, bridge, _sink) = run_terminal_probe(
        &[
            "--terminal-command",
            "sh",
            "--terminal-arg=-c",
            "--terminal-arg=printf aaaaabbbbb",
            "--terminal-byte-limit",
            "5",
        ],
        None,
    )
    .await;
    assert_eq!(report["exit_code"], 0);
    // Spec direction: truncation drops the OLDEST bytes and keeps the newest.
    assert_eq!(report["output"], "bbbbb");
    assert_eq!(report["truncated"], true);
    bridge.shutdown().await.expect("shutdown ok");
}

#[tokio::test]
async fn terminal_wait_for_exit_returns_exit_code() {
    let (report, bridge, _sink) = run_terminal_probe(
        &[
            "--terminal-command",
            "sh",
            "--terminal-arg=-c",
            "--terminal-arg=exit 7",
        ],
        None,
    )
    .await;
    assert_eq!(report["exit_code"], 7);
    assert_eq!(report["signal"], serde_json::Value::Null);
    bridge.shutdown().await.expect("shutdown ok");
}

#[tokio::test]
async fn terminal_kill_terminates_but_output_remains_readable() {
    let (report, bridge, _sink) = run_terminal_probe(
        &[
            "--terminal-command",
            "sh",
            "--terminal-arg=-c",
            "--terminal-arg=printf started; sleep 30",
            "--terminal-kill",
        ],
        None,
    )
    .await;
    assert_eq!(report["exit_code"], serde_json::Value::Null);
    let signal = report["signal"].as_str().expect("signal name");
    assert!(
        signal == "SIGTERM" || signal == "SIGKILL",
        "unexpected signal {signal}"
    );
    assert_eq!(report["output"], "started");
    bridge.shutdown().await.expect("shutdown ok");
}

#[tokio::test]
async fn terminal_wait_cancellation_preserves_the_terminal() {
    let (report, bridge, _sink) = run_terminal_probe(
        &[
            "--terminal-command",
            "sh",
            "--terminal-arg=-c",
            "--terminal-arg=printf started; sleep 30",
            "--terminal-cancel-wait",
        ],
        None,
    )
    .await;
    assert_eq!(report["cancelled_wait_error_code"], -32800);
    assert_eq!(report["output_after_cancel_ok"], true);
    assert_eq!(report["output"], "started");
    assert!(report["signal"].is_string());
    assert_eq!(report["post_release_error_code"], RESOURCE_NOT_FOUND_CODE);
    bridge.shutdown().await.expect("shutdown ok");
}

// The strict placebo only touches terminal/* when the client advertised
// `terminal: true`, so a non-skipped lifecycle proves the capability is on the
// wire.
#[tokio::test]
async fn terminal_full_lifecycle_under_advertised_capability() {
    let (report, bridge, _sink) = run_terminal_probe(
        &[
            "--require-terminal",
            "--terminal-command",
            "sh",
            "--terminal-arg=-c",
            "--terminal-arg=printf started; sleep 30",
            "--terminal-byte-limit",
            "1024",
            "--terminal-kill",
            "--terminal-release-unknown",
        ],
        None,
    )
    .await;
    assert_eq!(
        report.get("skipped"),
        None,
        "strict agent skipped the terminal probe; capability not advertised"
    );
    assert_eq!(
        report["release_unknown_error_code"],
        RESOURCE_NOT_FOUND_CODE
    );
    assert_eq!(report["exit_code"], serde_json::Value::Null);
    assert!(report["signal"].is_string());
    assert_eq!(report["output"], "started");
    assert_eq!(report["post_release_error_code"], RESOURCE_NOT_FOUND_CODE);
    bridge.shutdown().await.expect("shutdown ok");
}

#[tokio::test]
async fn terminal_release_of_unknown_id_errors() {
    let (report, bridge, _sink) = run_terminal_probe(&["--terminal-release-unknown"], None).await;
    assert_eq!(
        report["release_unknown_error_code"],
        RESOURCE_NOT_FOUND_CODE
    );
    bridge.shutdown().await.expect("shutdown ok");
}

#[tokio::test]
async fn shutdown_kills_live_terminals() {
    let pid_dir = tempfile::tempdir().expect("tempdir");
    let pid_file = pid_dir.path().join("terminal.pid");
    let script = format!("echo $$ > {}; sleep 30", pid_file.to_string_lossy());
    let (report, bridge, _sink) = run_terminal_probe(
        &[
            "--terminal-command",
            "sh",
            "--terminal-arg=-c",
            &format!("--terminal-arg={script}"),
            "--terminal-orphan",
        ],
        None,
    )
    .await;
    assert_eq!(report["orphaned"], true);

    let mut pid: Option<i32> = None;
    for _ in 0..100 {
        if let Ok(text) = std::fs::read_to_string(&pid_file)
            && let Ok(parsed) = text.trim().parse::<i32>()
        {
            pid = Some(parsed);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let pid = pid.expect("terminal child wrote its pid");

    bridge.shutdown().await.expect("shutdown ok");

    // The terminal child is in its own process group, so only the registry
    // drain can have killed it. kill(pid, 0) refuses for a dead pid.
    #[cfg(unix)]
    {
        let mut alive = true;
        for _ in 0..50 {
            // SAFETY: signal 0 is the standard existence probe.
            if unsafe { libc::kill(pid, 0) } != 0 {
                alive = false;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(!alive, "terminal child {pid} survived bridge shutdown");
    }
}
