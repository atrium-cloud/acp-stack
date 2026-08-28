use crate::common::cli::*;
use acp_stack::state::{
    DEPS_APPLY_ORIGIN_INIT_BACKGROUND, DEPS_APPLY_RUN_FAILED, DEPS_APPLY_RUN_RUNNING,
    DEPS_APPLY_RUN_SUCCEEDED, DepsApplyRunRecord, NewDepsApplyRun, StateStore, default_state_path,
};
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

/// Long enough that init's remaining lanes finish while the install is still running.
const SLOW_INSTALL_SECONDS: u64 = 8;
/// Upper bound on waiting for the detached worker to settle its run row.
const SETTLE_DEADLINE: Duration = Duration::from_secs(60);

fn open_state(home: &Path) -> StateStore {
    let store = StateStore::open(default_state_path(home)).expect("state store should open");
    store.migrate().expect("state store should migrate");
    store
}

fn latest_run(store: &StateStore) -> DepsApplyRunRecord {
    store
        .latest_deps_apply_run()
        .expect("deps apply runs should query")
        .expect("a deps apply run should be recorded")
}

fn wait_until_settled(store: &StateStore) -> DepsApplyRunRecord {
    let deadline = Instant::now() + SETTLE_DEADLINE;
    loop {
        let run = latest_run(store);
        if run.status != DEPS_APPLY_RUN_RUNNING {
            return run;
        }
        assert!(
            Instant::now() < deadline,
            "background deps apply never settled: {run:?}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn init_deps_apply_async_exits_while_the_install_keeps_running() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let bin_dir = tempdir.path().join("bg-bin");
    fs::create_dir_all(&bin_dir).expect("bin dir");
    let marker = bin_dir.join("acpstack-bg-tool");
    let marker_str = marker.to_string_lossy().into_owned();
    let host_path = std::env::var("PATH").expect("PATH should be set");
    let path_with_bin = format!("{}:{host_path}", bin_dir.to_string_lossy());
    let shell = format!(
        "sleep {SLOW_INSTALL_SECONDS}; printf '#!/bin/sh\\nexit 0\\n' > {marker_str} && chmod 755 {marker_str}"
    );

    acps_command(tempdir.path())
        .env("PATH", &path_with_bin)
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--dep",
            &format!("acpstack-bg-tool={shell}"),
            "--deps-apply",
            "--deps-apply-yes",
            "--deps-apply-async",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "dependency install started in background (apply_run_id=",
        ))
        .stdout(predicates::str::contains("initialized acp-stack"));

    let store = open_state(tempdir.path());
    let run = latest_run(&store);
    assert_eq!(run.status, DEPS_APPLY_RUN_RUNNING, "{run:?}");
    assert_eq!(run.origin, DEPS_APPLY_ORIGIN_INIT_BACKGROUND);
    assert!(run.pid.is_some(), "child pid must be stamped: {run:?}");
    assert!(
        !marker.exists(),
        "install artifact already exists; the apply did not outlive init"
    );

    let settled = wait_until_settled(&store);
    assert_eq!(settled.status, DEPS_APPLY_RUN_SUCCEEDED, "{settled:?}");
    assert_eq!(settled.installed, 1);
    assert!(
        marker.exists(),
        "install artifact should exist after settle"
    );

    // The init step recorded the launch, not the outcome.
    let init_run = store
        .latest_init_run()
        .expect("init runs should query")
        .expect("an init run should exist");
    assert_eq!(init_run.status, "succeeded");
    let deps_step = store
        .query_init_steps(&init_run.id)
        .expect("init steps should query")
        .into_iter()
        .find(|step| step.kind == "deps_apply")
        .expect("deps_apply step should be recorded");
    assert_eq!(deps_step.status, "succeeded");
    assert!(
        deps_step.payload_json.contains("\"background\":true"),
        "{}",
        deps_step.payload_json
    );
}

#[test]
fn init_deps_apply_async_failure_settles_retryable_without_failing_init() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    // The same action fails init synchronously; async moves the failure onto the run row.
    acps_command(tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--dep",
            "acpstack-bg-failtool=exit 3",
            "--deps-apply",
            "--deps-apply-yes",
            "--deps-apply-async",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("initialized acp-stack"));

    let store = open_state(tempdir.path());
    let settled = wait_until_settled(&store);
    assert_eq!(settled.status, DEPS_APPLY_RUN_FAILED, "{settled:?}");
    assert_eq!(settled.failed, 1);
    assert_eq!(settled.error_code.as_deref(), Some("deps.apply_failed"));

    let rows = store
        .query_installer_runs_filtered(Some("deps_apply"), 10)
        .expect("installer history should query");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].status, "failed");
    assert_eq!(rows[0].exit_status, Some(3));

    let init_run = store
        .latest_init_run()
        .expect("init runs should query")
        .expect("an init run should exist");
    assert_eq!(init_run.status, "succeeded");
}

#[test]
fn init_deps_apply_async_failure_handoff_carries_the_background_run_id() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    // Launch the async worker at the deps step, then fail a later step (the managed Cloudflare
    // step resolves a ref that was never stored) so the handoff frame must still name the run.
    let output = acps_command(tempdir.path())
        .args([
            "dev",
            "init",
            "--handoff-json",
            "--agent",
            "placebo",
            "--dep",
            "acpstack-bg-handoff-tool=exit 0",
            "--deps-apply",
            "--deps-apply-yes",
            "--deps-apply-async",
            "--skip-workspace-init",
            "--skip-testflight",
            "--edge",
            "cloudflare",
            "--exposure",
            "tunnel",
            "--hostname",
            "agent.example.com",
            "--cloudflare-mode",
            "managed",
            "--cloudflare-api-token-ref",
            "CF_TOKEN_MISSING",
            "--cloudflare-account-id-ref",
            "CF_ACCOUNT_MISSING",
            "--cloudflared-deployment",
            "external",
        ])
        .assert()
        .failure()
        .get_output()
        .stdout
        .clone();

    let body: serde_json::Value =
        serde_json::from_slice(&output).expect("failure handoff json parses");
    assert_eq!(body["status"], "failed", "{body}");
    let run_id = body["deps_apply_run_id"]
        .as_str()
        .unwrap_or_else(|| panic!("failure handoff must carry deps_apply_run_id: {body}"));
    assert!(run_id.starts_with("dap_"), "{run_id}");

    let store = open_state(tempdir.path());
    let run = store
        .lookup_deps_apply_run(run_id)
        .expect("run lookup should query")
        .expect("the handoff id must name a real run row");
    assert_eq!(run.origin, DEPS_APPLY_ORIGIN_INIT_BACKGROUND, "{run:?}");
}

#[test]
fn init_deps_apply_async_pre_spawn_setup_failure_settles_the_claimed_row() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    // A regular file where the per-run log directory belongs fails `create_dir_owner_only` after
    // the row is claimed but before the worker spawns; that row must settle, not wedge the slot.
    let log_base = tempdir.path().join(".local/share/acp-stack/installer-logs");
    fs::create_dir_all(&log_base).expect("installer-logs base");
    fs::write(log_base.join("deps_apply"), b"not a directory").expect("plant blocker file");

    acps_command(tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--dep",
            "acpstack-blocked-tool=exit 0",
            "--deps-apply",
            "--deps-apply-yes",
            "--deps-apply-async",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .failure();

    let store = open_state(tempdir.path());
    let run = latest_run(&store);
    assert_eq!(run.status, DEPS_APPLY_RUN_FAILED, "{run:?}");
    assert!(run.pid.is_none(), "no worker ever spawned: {run:?}");
    assert!(
        run.finished_at.is_some(),
        "a settled row must be finished: {run:?}"
    );
    assert_eq!(
        run.error_code.as_deref(),
        Some("deps.apply_failed"),
        "{run:?}"
    );
    assert!(
        store
            .running_deps_apply_run()
            .expect("running query")
            .is_none(),
        "the claimed row must not wedge the single-flight slot"
    );
}

#[test]
fn init_deps_apply_async_rejects_a_foreign_live_background_install() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let state_path = default_state_path(tempdir.path());
    fs::create_dir_all(state_path.parent().expect("state parent")).expect("state dir");
    let seeded = StateStore::open(&state_path).expect("seed store");
    seeded.migrate().expect("seed migrate");
    // A live background install owned by a different init run: adopting it would record this
    // init's deps step against foreign work, so the step must fail with the in-flight error.
    seeded
        .claim_deps_apply_run(
            NewDepsApplyRun {
                id: "dap_foreign_seed",
                origin: DEPS_APPLY_ORIGIN_INIT_BACKGROUND,
                init_run_id: Some("irun_prior"),
                feature: None,
                pid: Some(i64::from(std::process::id())),
                boot_id: acp_stack::runtime::process_runner::current_boot_id().as_deref(),
                total: 1,
            },
            &|_, _| true,
        )
        .expect("seed claim");
    drop(seeded);

    acps_command(tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--dep",
            "acpstack-adopt-tool=exit 0",
            "--deps-apply",
            "--deps-apply-yes",
            "--deps-apply-async",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "a dependency apply is already running (apply_run_id=dap_foreign_seed)",
        ));

    let store = open_state(tempdir.path());
    let runs = store
        .query_deps_apply_runs(10)
        .expect("deps apply runs should query");
    assert_eq!(
        runs.len(),
        1,
        "the rejected init must not claim a second run: {runs:?}"
    );
    assert_eq!(runs[0].id, "dap_foreign_seed");
    assert_eq!(runs[0].status, DEPS_APPLY_RUN_RUNNING);
}
