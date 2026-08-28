use crate::common::cli::*;
use crate::support::*;
use acp_stack::config::{AgentInstallConfig, load_config_from_str};
use std::fs;

#[test]
fn init_records_run_with_succeeded_steps() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    let state_path = tempdir.path().join(".local/share/acp-stack/state.sqlite");
    let store = acp_stack::state::StateStore::open(&state_path).expect("state opens");

    let runs = store.query_init_runs(10).expect("query runs");
    assert_eq!(runs.len(), 1, "first init must record exactly one run");
    let run = &runs[0];
    assert_eq!(run.status, acp_stack::state::INIT_RUN_SUCCEEDED);

    let steps = store.query_init_steps(&run.id).expect("query steps");
    assert!(!steps.is_empty(), "run must record at least one step");
    let kinds: Vec<&str> = steps.iter().map(|s| s.kind.as_str()).collect();
    assert!(
        kinds.contains(&"secrets_init"),
        "expected secrets_init in {kinds:?}",
    );
    assert!(
        kinds.contains(&"init_complete"),
        "expected init_complete in {kinds:?}",
    );
    for step in &steps {
        assert!(
            matches!(step.status.as_str(), "succeeded" | "skipped"),
            "step `{}` settled with unexpected status `{}`",
            step.kind,
            step.status,
        );
    }
}

#[test]
fn init_records_workspace_before_provider_configure() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let workspace = tempdir.path().join("workspace");

    acps_command(tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--workspace-root",
            workspace.to_str().expect("workspace path should be UTF-8"),
        ])
        .assert()
        .success();

    let state_path = tempdir.path().join(".local/share/acp-stack/state.sqlite");
    let store = acp_stack::state::StateStore::open(&state_path).expect("state opens");
    let run = store
        .query_init_runs(1)
        .expect("query runs")
        .into_iter()
        .next()
        .expect("init run");
    let steps = store.query_init_steps(&run.id).expect("query steps");
    let workspace_step = steps
        .iter()
        .find(|step| step.kind == "workspace_materialize")
        .expect("workspace step");
    let provider_step = steps
        .iter()
        .find(|step| step.kind == "provider_configure")
        .expect("provider step");

    assert!(
        workspace_step.ordinal < provider_step.ordinal,
        "workspace materialization must run before provider/model discovery: {steps:?}",
    );
}

#[test]
fn init_resume_targets_specific_pending_run_by_id() {
    // The post-crash shape: a run row left `pending` must be picked up by
    // `--resume --run-id` and finalized `succeeded`.
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    let state_path = tempdir.path().join(".local/share/acp-stack/state.sqlite");
    let store = acp_stack::state::StateStore::open(&state_path).expect("state opens");
    let pending = store
        .create_init_run(acp_stack::state::NewInitRun {
            runtime_user: None,
            agent_id: None,
            args_json: "{}",
        })
        .expect("synth pending run");
    let pending_id = pending.id.clone();
    drop(store);

    acps_command(tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--resume",
            "--run-id",
            &pending_id,
            "--skip-workspace-init",
        ])
        .assert()
        .success();

    let store = acp_stack::state::StateStore::open(&state_path).expect("state opens");
    let reloaded = store
        .lookup_init_run(&pending_id)
        .expect("lookup")
        .expect("pending row should still exist");
    assert_eq!(reloaded.status, acp_stack::state::INIT_RUN_SUCCEEDED);
    let steps = store.query_init_steps(&pending_id).expect("steps");
    assert!(
        !steps.is_empty(),
        "resume should have populated steps for the pending run",
    );
    for step in &steps {
        assert!(
            matches!(step.status.as_str(), "succeeded" | "skipped"),
            "step `{}` settled with unexpected status `{}`",
            step.kind,
            step.status,
        );
    }
}

#[test]
fn init_resume_retries_failed_agent_install_even_without_install_flag() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    let workspace = tempdir.path().join("workspace");
    fs::create_dir_all(&workspace).expect("workspace dir should be created");
    let missing_creates = tempdir.path().join("missing-resume-install-marker");
    let config_path = tempdir.path().join(".config/acp-stack/acps-config.toml");
    let mut config =
        load_config_from_str(&fs::read_to_string(&config_path).expect("config should be readable"))
            .expect("config should validate");
    config.workspace.root = workspace.to_string_lossy().into_owned();
    config.agent.id = "resume-install-test".to_owned();
    config.agent.name = "Resume Install Test".to_owned();
    config.agent.command = "resume-install-test-agent".to_owned();
    config.agent.args.clear();
    config.agent.install = Some(AgentInstallConfig {
        install_type: "shell".to_owned(),
        creates: missing_creates.to_string_lossy().into_owned(),
        shell: Some("true".to_owned()),
    });
    fs::write(
        &config_path,
        config.to_canonical_toml().expect("canonical config"),
    )
    .expect("config should be written");

    let state_path = tempdir.path().join(".local/share/acp-stack/state.sqlite");
    let store = acp_stack::state::StateStore::open(&state_path).expect("state opens");
    let failed = store
        .create_init_run(acp_stack::state::NewInitRun {
            runtime_user: None,
            agent_id: Some("placeholder"),
            args_json: "{}",
        })
        .expect("failed run");
    let step = store
        .append_init_step(acp_stack::state::NewInitStep {
            run_id: &failed.id,
            ordinal: 2,
            kind: "agent_install",
            payload_json: "{}",
        })
        .expect("agent install step");
    store.mark_init_step_running(&step.id).expect("running");
    store
        .mark_init_step_failed(
            &step.id,
            None,
            "agent.installer_creates_missing",
            "missing",
            "{}",
        )
        .expect("failed step");
    store
        .finalize_init_run(&failed.id, acp_stack::state::INIT_RUN_FAILED)
        .expect("failed run finalize");
    let failed_id = failed.id.clone();
    drop(store);

    acps_command(tempdir.path())
        .args(["init", "--resume", "--run-id", &failed_id])
        .assert()
        .failure();

    let store = acp_stack::state::StateStore::open(&state_path).expect("state opens");
    let reloaded = store
        .lookup_init_run(&failed_id)
        .expect("lookup")
        .expect("failed row should still exist");
    assert_eq!(reloaded.status, acp_stack::state::INIT_RUN_FAILED);
    let steps = store.query_init_steps(&failed_id).expect("steps");
    let install_step = steps
        .iter()
        .find(|step| step.kind == "agent_install")
        .expect("agent install step");
    assert_eq!(install_step.status, acp_stack::state::INIT_STEP_FAILED);
}

#[test]
fn init_resume_restores_recorded_agent_after_provider_secret_failure() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let workspace = tempdir.path().join("workspace");
    fs::create_dir_all(&workspace).expect("workspace");

    acps_with_empty_path(tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "opencode",
            "--provider",
            "openai",
            "--api-key-ref",
            "CUSTOM_OPENAI_API_KEY",
            "--workspace-root",
            workspace.to_str().expect("workspace UTF-8"),
            "--skip-workspace-init",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("CUSTOM_OPENAI_API_KEY"));

    let config_before =
        fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
            .expect("config should be readable");
    assert!(config_before.contains(r#"id = "opencode""#));

    seed_init_secrets(
        tempdir.path(),
        &[("CUSTOM_OPENAI_API_KEY", "test-openai-key")],
    );

    acps_with_empty_path(tempdir.path())
        .args(["init", "--resume"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent: OpenCode (opencode)"));

    let config_after =
        fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
            .expect("config should be readable");
    assert!(config_after.contains(r#"id = "opencode""#));
    assert!(config_after.contains(r#"id = "openai""#));
    assert!(config_after.contains(r#"api_key_ref = "CUSTOM_OPENAI_API_KEY""#));
    assert!(!config_after.contains(r#"api_key_ref = "OPENAI_API_KEY""#));
}

#[test]
fn init_resume_restores_recorded_custom_provider_args_after_secret_failure() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let workspace = tempdir.path().join("workspace");
    fs::create_dir_all(&workspace).expect("workspace");

    acps_with_empty_path(tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "opencode",
            "--provider",
            "myprovider",
            "--custom-provider",
            "--provider-name",
            "My Provider",
            "--base-url",
            "https://api.myprovider.example/v1",
            "--provider-api",
            "chat-completions",
            "--api-key-ref",
            "MY_PROVIDER_API_KEY",
            "--model",
            "my-model",
            "--model-name",
            "My Model",
            "--context",
            "123456",
            "--output-max-tokens",
            "12345",
            "--workspace-root",
            workspace.to_str().expect("workspace UTF-8"),
            "--skip-workspace-init",
            "--skip-testflight",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("MY_PROVIDER_API_KEY"));

    seed_init_secrets(
        tempdir.path(),
        &[("MY_PROVIDER_API_KEY", "test-provider-key")],
    );

    acps_with_empty_path(tempdir.path())
        .args(["init", "--resume"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent: OpenCode (opencode)"));

    let config_after =
        fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
            .expect("config should be readable");
    assert!(config_after.contains(r#"id = "myprovider""#));
    assert!(config_after.contains("[array.targets.agent.provider.custom]"));
    assert!(config_after.contains(r#"name = "My Provider""#));
    assert!(config_after.contains(r#"api_key_ref = "MY_PROVIDER_API_KEY""#));
    assert!(config_after.contains(r#"base_url = "https://api.myprovider.example/v1""#));
    assert!(config_after.contains(r#"api = "chat-completions""#));
    assert!(config_after.contains(r#"model_name = "My Model""#));
    assert!(config_after.contains("context = 123456"));
    assert!(config_after.contains("output_max_tokens = 12345"));
}

#[test]
fn init_resume_without_prior_run_errors_clearly() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    // No prior `acps init` — the resume target doesn't exist.
    acps_command(tempdir.path())
        .args(["init", "--resume"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("no resumable init run"));
}
