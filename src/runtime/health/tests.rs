use super::*;

#[test]
fn node_health_maps_each_outcome_and_only_failed_is_failing() {
    let home = tempfile::tempdir().expect("home");
    let cases = [
        (NodeRuntimeOutcome::Unmanaged, NODE_STATUS_UNMANAGED),
        (NodeRuntimeOutcome::Pending, NODE_STATUS_PENDING),
        (
            NodeRuntimeOutcome::Settled(Ok(NodeRuntimeStatus::Ready {
                version: "v26.1.0".to_owned(),
            })),
            NODE_STATUS_READY,
        ),
        (
            NodeRuntimeOutcome::Settled(Ok(NodeRuntimeStatus::Unsupported {
                reason: "no managed Node.js 26 build for macos/aarch64".to_owned(),
            })),
            NODE_STATUS_UNSUPPORTED,
        ),
        (
            NodeRuntimeOutcome::Settled(Ok(NodeRuntimeStatus::NotReady)),
            NODE_STATUS_FAILED,
        ),
        (
            NodeRuntimeOutcome::Settled(Err("managed Node.js runtime install failed".to_owned())),
            NODE_STATUS_FAILED,
        ),
    ];
    for (outcome, expected) in cases {
        assert_eq!(collect_node(outcome, home.path()).status, expected);
    }
}

#[cfg(unix)]
#[test]
fn node_health_reads_ready_when_a_later_install_healed_a_failed_startup() {
    let home = tempfile::tempdir().expect("home");
    let root = node_runtime::managed_root(home.path());
    let bin = root.join("releases/node-v26.1.0-linux-x64/bin");
    std::fs::create_dir_all(&bin).expect("release bin");
    std::fs::write(bin.join("node"), "#!/bin/sh\n").expect("node");
    std::os::unix::fs::symlink("releases/node-v26.1.0-linux-x64", root.join("current"))
        .expect("current");

    let health = collect_node(
        NodeRuntimeOutcome::Settled(Err("managed Node.js runtime install failed".to_owned())),
        home.path(),
    );

    assert_eq!(health.status, NODE_STATUS_READY);
    assert_eq!(health.version.as_deref(), Some("v26.1.0"));
    assert_eq!(health.reason, None);
}

#[test]
fn orphan_probe_without_started_processes_is_empty() {
    let probe = AgentProcessProbe::default();
    assert!(orphaned_agent_process_pids(&probe, &std::collections::BTreeSet::new()).is_empty());
}

#[test]
fn orphan_probe_ignores_current_supervised_pid() {
    let probe = AgentProcessProbe {
        started_processes: vec![AgentStartedProcess {
            created_at: "2026-05-28T00:00:00.000000000Z".to_owned(),
            agent_id: Some("opencode".to_owned()),
            pid: std::process::id(),
        }],
        probe_error: None,
    };
    let supervised = std::collections::BTreeSet::from([std::process::id()]);
    assert!(orphaned_agent_process_pids(&probe, &supervised).is_empty());
}

#[test]
fn orphan_probe_excludes_every_supervised_target_pid() {
    // A secondary target's live pid must read as supervised, not orphaned.
    let primary_pid = std::process::id();
    let secondary_pid = primary_pid.wrapping_add(1).max(2);
    let probe = AgentProcessProbe {
        started_processes: vec![
            AgentStartedProcess {
                created_at: "2026-05-28T00:00:00.000000000Z".to_owned(),
                agent_id: Some("opencode".to_owned()),
                pid: primary_pid,
            },
            AgentStartedProcess {
                created_at: "2026-05-28T00:00:00.000000000Z".to_owned(),
                agent_id: Some("codex".to_owned()),
                pid: secondary_pid,
            },
        ],
        probe_error: None,
    };
    let supervised = std::collections::BTreeSet::from([primary_pid, secondary_pid]);
    assert!(orphaned_agent_process_pids(&probe, &supervised).is_empty());
}

#[test]
fn collect_sink_disabled_returns_empty_health() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = StateStore::open(dir.path().join("state.sqlite")).expect("open");
    store.migrate().expect("migrate");
    let sink = collect_sink(&store, false);
    assert!(!sink.enabled);
    assert_eq!(sink.open_failure_count, 0);
    assert!(sink.probe_error.is_none());
}

#[test]
fn collect_sink_enabled_with_no_rows_reports_zero_failures() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = StateStore::open(dir.path().join("state.sqlite")).expect("open");
    store.migrate().expect("migrate");
    let sink = collect_sink(&store, true);
    assert!(sink.enabled);
    assert_eq!(sink.open_failure_count, 0);
    assert!(sink.latest_failure_at.is_none());
    assert!(sink.probe_error.is_none());
}

#[test]
fn collect_sink_surfaces_probe_error_when_table_missing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = StateStore::open(dir.path().join("state.sqlite")).expect("open");
    // No migrate(), so the sink tables are absent: the probe must surface
    // `probe_error` rather than swallowing it.
    let sink = collect_sink(&store, true);
    assert!(sink.enabled);
    assert!(
        sink.probe_error.is_some(),
        "expected probe_error when sink tables are missing, got {sink:?}"
    );
}
