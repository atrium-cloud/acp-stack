use super::*;

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
    // A secondary Array target's live pid must be treated as supervised,
    // not orphaned: both supervised pids are excluded before the liveness
    // probe runs, so a multi-target fleet reports zero orphans.
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
    // Deliberately skip `migrate()` so `sink_outbox` / `sink_failures_summary`
    // do not exist. The probe must surface this as `probe_error`
    // (regression test for the silent-swallow finding from Codex audit).
    let sink = collect_sink(&store, true);
    assert!(sink.enabled);
    assert!(
        sink.probe_error.is_some(),
        "expected probe_error when sink tables are missing, got {sink:?}"
    );
}
