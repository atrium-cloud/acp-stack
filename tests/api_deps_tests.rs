//! `/v1/deps/apply` behavior that only shows up against a live server: the
//! apply runs operator install snippets that can take minutes, so it must not
//! park the rest of the HTTP surface while it works.

use std::time::{Duration, Instant};

use reqwest::StatusCode;

use acp_stack::config::{Config, DependencyEntry, DependencyInstallAction, DependencyInstallScope};
use acp_stack::runtime::dependencies::deps_apply::DEPS_APPLY_AGENT_ID;
use acp_stack::state::{
    DEPS_APPLY_ORIGIN_CLI, DEPS_APPLY_RUN_SUCCEEDED, DepsApplyRunFinish, NewDepsApplyRun,
    StateStore,
};

mod common;
use common::api::{ADMIN_KEY, SESSION_KEY, ServerHarness, test_config};

/// Long enough that the probe below lands inside the apply window, short
/// enough to keep the test fast.
const SLOW_INSTALL_SECONDS: u64 = 3;
/// The probe touches no state, so a wait near the install duration means it
/// was queued behind the apply.
const PROBE_TIMEOUT: Duration = Duration::from_secs(1);
/// Upper bound on how long the in-flight action may take to become visible.
const RUNNING_ROW_DEADLINE: Duration = Duration::from_secs(10);

fn config_with_slow_install() -> Config {
    let mut config = test_config();
    config.dependencies.commands.push(DependencyEntry {
        name: "acps-deps-apply-probe".to_owned(),
        required: false,
        feature: None,
        install: Some(DependencyInstallAction {
            shell: format!("sleep {SLOW_INSTALL_SECONDS}"),
            // Never resolves, so the runner always executes the snippet.
            creates: Some("acps-deps-apply-probe".to_owned()),
            scope: DependencyInstallScope::User,
            timeout_secs: Some(60),
        }),
    });
    config
}

fn deps_apply_running(reader: &StateStore) -> bool {
    !reader
        .query_active_installer_runs(Some(DEPS_APPLY_AGENT_ID))
        .expect("query active installer runs")
        .is_empty()
}

#[tokio::test]
async fn deps_apply_does_not_block_unrelated_requests() {
    let harness = ServerHarness::spawn_with_config(config_with_slow_install()).await;
    let apply_url = format!("{}/v1/deps/apply", harness.base_url);
    let apply = tokio::spawn(async move {
        reqwest::Client::new()
            .post(apply_url)
            .header("Authorization", format!("Bearer {ADMIN_KEY}"))
            .json(&serde_json::json!({ "confirmation": true }))
            .send()
            .await
            .expect("deps apply request")
    });

    // The `running` row lands before the shell spawns, so its appearance pins
    // the probe inside the apply window without a sleep. Read it on a
    // dedicated connection: the harness store handle would contend for the
    // very mutex under test.
    let reader = StateStore::open(&harness.state_path).expect("reader connection");
    let deadline = Instant::now() + RUNNING_ROW_DEADLINE;
    while !deps_apply_running(&reader) {
        assert!(
            Instant::now() < deadline,
            "deps apply never reported an in-flight action"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // `/v1/workspace` reads in-memory config only, but the `api.request` audit
    // middleware takes the shared state mutex after the handler returns, so an
    // apply holding that mutex stalls even this request.
    let probe = reqwest::Client::builder()
        .timeout(PROBE_TIMEOUT)
        .build()
        .expect("probe client")
        .get(format!("{}/v1/workspace", harness.base_url))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("unrelated request must answer while a deps apply is in flight");
    assert_eq!(probe.status(), StatusCode::OK);

    assert!(
        deps_apply_running(&reader),
        "the apply finished before the probe returned; the probe proved nothing"
    );

    let response = apply.await.expect("apply task");
    assert_eq!(response.status(), StatusCode::OK);
}

/// The apply outlives the client that started it, so `/v1/status` is where an
/// orchestrator learns to wait rather than restart mid-install.
#[tokio::test]
async fn status_reports_a_deps_apply_in_flight() {
    let harness = ServerHarness::spawn_with_config(config_with_slow_install()).await;
    let status_url = format!("{}/v1/status", harness.base_url);
    let client = reqwest::Client::new();

    let idle: serde_json::Value = client
        .get(&status_url)
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("status before the apply")
        .json()
        .await
        .expect("json");
    assert_eq!(idle["data"]["deps_apply_in_flight"], false);

    let apply_url = format!("{}/v1/deps/apply", harness.base_url);
    let apply = tokio::spawn(async move {
        reqwest::Client::new()
            .post(apply_url)
            .header("Authorization", format!("Bearer {ADMIN_KEY}"))
            .json(&serde_json::json!({ "confirmation": true }))
            .send()
            .await
            .expect("deps apply request")
    });

    // Same `running`-row pinning as above.
    let reader = StateStore::open(&harness.state_path).expect("reader connection");
    let deadline = Instant::now() + RUNNING_ROW_DEADLINE;
    while !deps_apply_running(&reader) {
        assert!(
            Instant::now() < deadline,
            "deps apply never reported an in-flight action"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let during: serde_json::Value = client
        .get(&status_url)
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("status must answer while an apply is in flight")
        .json()
        .await
        .expect("json");
    assert_eq!(during["data"]["deps_apply_in_flight"], true);
    assert!(
        deps_apply_running(&reader),
        "the apply finished before the probe returned; the probe proved nothing"
    );

    let response = apply.await.expect("apply task");
    assert_eq!(response.status(), StatusCode::OK);

    // The handler awaits the blocking task, so the guard is released by the
    // time the apply response lands.
    let after: serde_json::Value = client
        .get(&status_url)
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("status after the apply")
        .json()
        .await
        .expect("json");
    assert_eq!(after["data"]["deps_apply_in_flight"], false);
}

/// A second apply while one runs is rejected retryable, never queued: the
/// caller polls the surfaced apply_run_id to completion and re-POSTs.
#[tokio::test]
async fn concurrent_deps_apply_is_rejected_with_409_in_flight() {
    let harness = ServerHarness::spawn_with_config(config_with_slow_install()).await;
    let apply_url = format!("{}/v1/deps/apply", harness.base_url);
    let first_url = apply_url.clone();
    let first = tokio::spawn(async move {
        reqwest::Client::new()
            .post(first_url)
            .header("Authorization", format!("Bearer {ADMIN_KEY}"))
            .json(&serde_json::json!({ "confirmation": true }))
            .send()
            .await
            .expect("first deps apply request")
    });

    let reader = StateStore::open(&harness.state_path).expect("reader connection");
    let deadline = Instant::now() + RUNNING_ROW_DEADLINE;
    while !deps_apply_running(&reader) {
        assert!(
            Instant::now() < deadline,
            "deps apply never reported an in-flight action"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let second: serde_json::Value = {
        let response = reqwest::Client::new()
            .post(&apply_url)
            .header("Authorization", format!("Bearer {ADMIN_KEY}"))
            .json(&serde_json::json!({ "confirmation": true }))
            .send()
            .await
            .expect("second deps apply request");
        assert_eq!(response.status(), StatusCode::CONFLICT);
        response.json().await.expect("json")
    };
    assert_eq!(second["error"]["code"], "deps.apply_in_flight");

    let response = first.await.expect("first apply task");
    assert_eq!(response.status(), StatusCode::OK);
}

/// The per-run polling surface: `running` (live) while the apply works, a
/// terminal retryable state with per-action rows afterwards.
#[tokio::test]
async fn deps_apply_runs_routes_expose_progress_and_retryable_outcome() {
    let harness = ServerHarness::spawn_with_config(config_with_slow_install()).await;
    let client = reqwest::Client::new();
    let latest_url = format!("{}/v1/deps/apply/runs/latest", harness.base_url);

    let apply_url = format!("{}/v1/deps/apply", harness.base_url);
    let apply = tokio::spawn(async move {
        reqwest::Client::new()
            .post(apply_url)
            .header("Authorization", format!("Bearer {ADMIN_KEY}"))
            .json(&serde_json::json!({ "confirmation": true }))
            .send()
            .await
            .expect("deps apply request")
    });

    let reader = StateStore::open(&harness.state_path).expect("reader connection");
    let deadline = Instant::now() + RUNNING_ROW_DEADLINE;
    while !deps_apply_running(&reader) {
        assert!(
            Instant::now() < deadline,
            "deps apply never reported an in-flight action"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let during: serde_json::Value = client
        .get(&latest_url)
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("latest run during the apply")
        .json()
        .await
        .expect("json");
    assert_eq!(during["data"]["status"], "running");
    assert_eq!(during["data"]["live"], true);
    assert_eq!(during["data"]["origin"], "api");
    assert_eq!(during["data"]["progress"]["total"], 1);

    let response = apply.await.expect("apply task");
    assert_eq!(response.status(), StatusCode::OK);

    // The probe dep's `creates` never resolves, so the settled run is failed
    // and retryable, which is what a hosting client keys its retry surface on.
    let after: serde_json::Value = client
        .get(&latest_url)
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("latest run after the apply")
        .json()
        .await
        .expect("json");
    assert_eq!(after["data"]["status"], "failed");
    assert_eq!(after["data"]["retryable"], true);
    assert_eq!(after["data"]["counts"]["failed"], 1);
    let apply_run_id = after["data"]["apply_run_id"]
        .as_str()
        .expect("apply_run_id string")
        .to_owned();
    assert_eq!(
        after["data"]["actions"]
            .as_array()
            .expect("actions array")
            .len(),
        1
    );

    let by_id: serde_json::Value = client
        .get(format!(
            "{}/v1/deps/apply/runs/{apply_run_id}",
            harness.base_url
        ))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("run by id")
        .json()
        .await
        .expect("json");
    assert_eq!(by_id["data"]["apply_run_id"], apply_run_id.as_str());

    let list: serde_json::Value = client
        .get(format!("{}/v1/deps/apply/runs", harness.base_url))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("runs list")
        .json()
        .await
        .expect("json");
    assert_eq!(
        list["data"]["runs"].as_array().expect("runs array").len(),
        1
    );
}

/// `deps_apply_in_flight` must observe applies the daemon did not start (a
/// detached init child, a CLI apply) via the live run row, not just the
/// in-process lock.
#[tokio::test]
async fn status_reports_an_externally_owned_deps_apply() {
    let harness = ServerHarness::spawn_with_config(test_config()).await;
    let status_url = format!("{}/v1/status", harness.base_url);
    let client = reqwest::Client::new();

    let external = StateStore::open(&harness.state_path).expect("external connection");
    // Stands in for a CLI/detached apply: a running row owned by a live pid,
    // with no daemon lock held.
    external
        .claim_deps_apply_run(
            NewDepsApplyRun {
                id: "dap_external",
                origin: DEPS_APPLY_ORIGIN_CLI,
                init_run_id: None,
                feature: None,
                pid: Some(i64::from(std::process::id())),
                boot_id: acp_stack::runtime::process_runner::current_boot_id().as_deref(),
                total: 1,
            },
            &|_, _| true,
        )
        .expect("external claim");

    let during: serde_json::Value = client
        .get(&status_url)
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("status during the external apply")
        .json()
        .await
        .expect("json");
    assert_eq!(during["data"]["deps_apply_in_flight"], true);

    external
        .finish_deps_apply_run(
            "dap_external",
            DepsApplyRunFinish {
                status: DEPS_APPLY_RUN_SUCCEEDED,
                completed: 1,
                installed: 1,
                already_present: 0,
                privilege_required: 0,
                failed: 0,
                error_code: None,
                error_detail: None,
                payload_json: "{}",
            },
        )
        .expect("external finish");

    let after: serde_json::Value = client
        .get(&status_url)
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("status after the external apply")
        .json()
        .await
        .expect("json");
    assert_eq!(after["data"]["deps_apply_in_flight"], false);
}
