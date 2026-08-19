//! `/v1/deps/apply` behavior that only shows up against a live server: the
//! apply runs operator install snippets that can take minutes, so it must not
//! park the rest of the HTTP surface while it works.

use std::time::{Duration, Instant};

use reqwest::StatusCode;

use acp_stack::config::{Config, DependencyEntry, DependencyInstallAction, DependencyInstallScope};
use acp_stack::runtime::dependencies::deps_apply::DEPS_APPLY_AGENT_ID;
use acp_stack::state::StateStore;

mod common;
use common::api::{ADMIN_KEY, SESSION_KEY, ServerHarness, test_config};

/// Long enough that the probe below lands inside the apply window, short
/// enough to keep the test fast.
const SLOW_INSTALL_SECONDS: u64 = 3;
/// The probe touches no state, so it must answer immediately. A wait anywhere
/// near the install duration means it was queued behind the apply.
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
            // Never resolves, so the runner always executes the snippet
            // instead of taking the already-present shortcut.
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

    // The action's `running` row is committed before its shell spawns, so its
    // appearance pins the probe inside the apply window instead of relying on
    // a sleep. Read it on a dedicated connection: going through the harness
    // store handle would contend for the very mutex under test.
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
    // middleware takes the shared state mutex after the handler returns — so
    // an apply that holds that mutex stalls even this request.
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

/// The apply outlives the client that started it, so an orchestrator that
/// restarts the runtime to pick up new config would tear a running install
/// down mid-flight. `/v1/status` is where it learns to wait instead.
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

    // Same pinning as above: the action's `running` row lands before its shell
    // spawns, so the probe below is inside the apply window by construction.
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

    // The handler awaits the blocking task, so the guard is already released
    // by the time the apply response lands.
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
