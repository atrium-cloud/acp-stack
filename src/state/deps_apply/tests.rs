use super::*;

fn open_store() -> (tempfile::TempDir, StateStore) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = StateStore::open(dir.path().join("state.sqlite")).expect("open");
    store.migrate().expect("migrate");
    (dir, store)
}

fn claim(
    store: &StateStore,
    id: &str,
    pid: Option<i64>,
    is_live: &dyn Fn(i64, Option<&str>) -> bool,
) -> Result<DepsApplyRunRecord> {
    store.claim_deps_apply_run(
        NewDepsApplyRun {
            id,
            origin: DEPS_APPLY_ORIGIN_CLI,
            init_run_id: None,
            feature: None,
            pid,
            boot_id: Some("boot-a"),
            total: 2,
        },
        is_live,
    )
}

fn always_live(_pid: i64, _boot_id: Option<&str>) -> bool {
    true
}

fn never_live(_pid: i64, _boot_id: Option<&str>) -> bool {
    false
}

fn age_row_past_grace(store: &StateStore, id: &str) {
    let aged = chrono::Utc::now()
        - chrono::Duration::seconds(DEPS_APPLY_NULL_PID_GRACE.as_secs() as i64 + 5);
    store
        .connection()
        .execute(
            "UPDATE deps_apply_runs SET started_at = ?1 WHERE id = ?2",
            params![aged.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true), id],
        )
        .expect("age row");
}

#[test]
fn claim_progress_finish_round_trip() {
    let (_dir, store) = open_store();
    let record = claim(&store, "dap_rt", Some(4242), &always_live).expect("claim");
    assert_eq!(record.status, DEPS_APPLY_RUN_RUNNING);
    assert_eq!(record.total, 2);

    store
        .update_deps_apply_progress("dap_rt", 1, Some("ripgrep"))
        .expect("progress");
    let mid = store
        .lookup_deps_apply_run("dap_rt")
        .expect("lookup")
        .expect("row");
    assert_eq!(mid.completed, 1);
    assert_eq!(mid.current_dep.as_deref(), Some("ripgrep"));
    assert!(mid.finished_at.is_none());

    store
        .finish_deps_apply_run(
            "dap_rt",
            DepsApplyRunFinish {
                status: DEPS_APPLY_RUN_SUCCEEDED,
                completed: 2,
                installed: 1,
                already_present: 1,
                privilege_required: 0,
                failed: 0,
                error_code: None,
                error_detail: None,
                payload_json: "{}",
            },
        )
        .expect("finish");
    let done = store
        .lookup_deps_apply_run("dap_rt")
        .expect("lookup")
        .expect("row");
    assert_eq!(done.status, DEPS_APPLY_RUN_SUCCEEDED);
    assert_eq!(done.installed, 1);
    assert_eq!(done.already_present, 1);
    assert!(done.finished_at.is_some());
    assert!(done.current_dep.is_none());
    assert!(store.running_deps_apply_run().expect("running").is_none());
}

#[test]
fn second_claim_while_one_is_live_is_rejected_with_the_live_id() {
    let (_dir, store) = open_store();
    claim(&store, "dap_first", Some(4242), &always_live).expect("claim");
    let error = claim(&store, "dap_second", Some(4243), &always_live)
        .expect_err("second live claim must be rejected");
    match error {
        StackError::DepsApplyInFlight { apply_run_id } => {
            assert_eq!(apply_run_id, "dap_first");
        }
        other => panic!("expected DepsApplyInFlight, got {other:?}"),
    }
    let live = store
        .running_deps_apply_run()
        .expect("running")
        .expect("row");
    assert_eq!(live.id, "dap_first");
}

#[test]
fn dead_pid_row_reconciles_as_abandoned_and_frees_the_claim() {
    let (_dir, store) = open_store();
    claim(&store, "dap_dead", Some(4242), &always_live).expect("claim");
    let second =
        claim(&store, "dap_next", Some(4243), &never_live).expect("dead owner must free the slot");
    assert_eq!(second.status, DEPS_APPLY_RUN_RUNNING);
    let abandoned = store
        .lookup_deps_apply_run("dap_dead")
        .expect("lookup")
        .expect("row");
    assert_eq!(abandoned.status, DEPS_APPLY_RUN_FAILED);
    assert_eq!(
        abandoned.error_code.as_deref(),
        Some(DEPS_APPLY_ABANDONED_ERROR_CODE)
    );
    assert!(abandoned.finished_at.is_some());
}

#[test]
fn boot_id_is_the_liveness_predicate_input() {
    // The state layer hands the stored boot id to the predicate verbatim; reuse policy is the
    // caller's.
    let (_dir, store) = open_store();
    claim(&store, "dap_boot", Some(4242), &always_live).expect("claim");
    let saw = std::cell::RefCell::new(None);
    let is_live = |pid: i64, boot_id: Option<&str>| {
        *saw.borrow_mut() = Some((pid, boot_id.map(str::to_owned)));
        false
    };
    store
        .reconcile_stale_deps_apply_runs(&is_live)
        .expect("reconcile");
    assert_eq!(
        saw.borrow().clone(),
        Some((4242, Some("boot-a".to_owned())))
    );
}

#[test]
fn null_pid_row_survives_the_grace_window_then_reconciles() {
    let (_dir, store) = open_store();
    claim(&store, "dap_gap", None, &always_live).expect("claim");
    // Fresh null-pid row (claim-to-stamp gap): not abandoned.
    assert_eq!(
        store
            .reconcile_stale_deps_apply_runs(&always_live)
            .expect("reconcile"),
        0
    );
    age_row_past_grace(&store, "dap_gap");
    assert_eq!(
        store
            .reconcile_stale_deps_apply_runs(&always_live)
            .expect("reconcile"),
        1
    );
    let abandoned = store
        .lookup_deps_apply_run("dap_gap")
        .expect("lookup")
        .expect("row");
    assert_eq!(abandoned.status, DEPS_APPLY_RUN_FAILED);
    assert_eq!(
        abandoned.error_code.as_deref(),
        Some(DEPS_APPLY_ABANDONED_ERROR_CODE)
    );
}

#[test]
fn stamp_child_records_pid_boot_and_log_dir() {
    let (_dir, store) = open_store();
    claim(&store, "dap_stamp", None, &always_live).expect("claim");
    store
        .stamp_deps_apply_child("dap_stamp", 555, Some("boot-b"), Some("/logs/dap_stamp"))
        .expect("stamp");
    let row = store
        .lookup_deps_apply_run("dap_stamp")
        .expect("lookup")
        .expect("row");
    assert_eq!(row.pid, Some(555));
    assert_eq!(row.boot_id.as_deref(), Some("boot-b"));
    assert_eq!(row.log_dir.as_deref(), Some("/logs/dap_stamp"));
}

#[test]
fn self_owned_stale_row_is_force_failed_but_foreign_rows_are_left_alone() {
    let (_dir, store) = open_store();
    // A row this process owns whose terminal write never landed: the liveness reconcile can never
    // free it (the owner pid is live), so the API path settles it before its next claim.
    claim(&store, "dap_self", Some(4242), &always_live).expect("claim self");

    assert_eq!(
        store
            .fail_self_owned_stale_deps_apply_runs(9999, Some("boot-a"))
            .expect("clear"),
        0,
        "a foreign pid must not be settled"
    );
    // Same pid, different boot id: a previous-boot row is the liveness reconcile's job.
    assert_eq!(
        store
            .fail_self_owned_stale_deps_apply_runs(4242, Some("boot-other"))
            .expect("clear"),
        0,
        "a different boot id must not be settled"
    );
    assert_eq!(
        store
            .lookup_deps_apply_run("dap_self")
            .expect("lookup")
            .expect("row")
            .status,
        DEPS_APPLY_RUN_RUNNING
    );

    assert_eq!(
        store
            .fail_self_owned_stale_deps_apply_runs(4242, Some("boot-a"))
            .expect("clear"),
        1
    );
    let settled = store
        .lookup_deps_apply_run("dap_self")
        .expect("lookup")
        .expect("row");
    assert_eq!(settled.status, DEPS_APPLY_RUN_FAILED);
    assert_eq!(
        settled.error_code.as_deref(),
        Some(DEPS_APPLY_ABANDONED_ERROR_CODE)
    );
    assert!(settled.finished_at.is_some());
    assert!(store.running_deps_apply_run().expect("running").is_none());
    let next = claim(&store, "dap_after", Some(4242), &always_live)
        .expect("a freed slot must accept a new claim");
    assert_eq!(next.status, DEPS_APPLY_RUN_RUNNING);
}

#[test]
fn self_owned_clear_matches_a_null_boot_id_row() {
    // The macOS-dev daemon claims with no boot id, so `boot_id IS ?` must match a NULL-boot row
    // against a NULL probe and never against a non-null one.
    let (_dir, store) = open_store();
    store
        .claim_deps_apply_run(
            NewDepsApplyRun {
                id: "dap_nullboot",
                origin: DEPS_APPLY_ORIGIN_API,
                init_run_id: None,
                feature: None,
                pid: Some(4242),
                boot_id: None,
                total: 1,
            },
            &always_live,
        )
        .expect("claim");
    assert_eq!(
        store
            .fail_self_owned_stale_deps_apply_runs(4242, Some("boot-a"))
            .expect("clear"),
        0,
        "a non-null probe boot id must not match a NULL-boot row"
    );
    assert_eq!(
        store
            .fail_self_owned_stale_deps_apply_runs(4242, None)
            .expect("clear"),
        1
    );
    assert!(store.running_deps_apply_run().expect("running").is_none());
}

#[test]
fn query_runs_orders_newest_first_and_honors_limit() {
    let (_dir, store) = open_store();
    for index in 0..3 {
        let id = format!("dap_seq_{index}");
        claim(&store, &id, Some(4242), &always_live).expect("claim");
        store
            .finish_deps_apply_run(
                &id,
                DepsApplyRunFinish {
                    status: DEPS_APPLY_RUN_SUCCEEDED,
                    completed: 2,
                    installed: 2,
                    already_present: 0,
                    privilege_required: 0,
                    failed: 0,
                    error_code: None,
                    error_detail: None,
                    payload_json: "{}",
                },
            )
            .expect("finish");
    }
    let runs = store.query_deps_apply_runs(2).expect("query");
    assert_eq!(runs.len(), 2);
    assert!(runs[0].started_at >= runs[1].started_at);
    let latest = store.latest_deps_apply_run().expect("latest").expect("row");
    assert_eq!(latest.id, runs[0].id);
}
