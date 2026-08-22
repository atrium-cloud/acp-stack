use std::fs;

use acp_stack::config::load_config_from_str;
use acp_stack::state::{InitStepRecord, StateStore, default_state_path};
use predicates::prelude::PredicateBooleanExt as _;

use crate::common::cli::*;
use crate::support::write_workspace_init_config;

#[test]
fn init_rejects_mode_flag_for_agents_without_set_mode() {
    // pi/hermes declare set_mode=false; `--mode` must fail fast as a
    // capability check rather than being silently dropped.
    for agent in ["pi", "hermes"] {
        let tempdir = tempfile::tempdir().expect("tempdir");

        acps_command()
            .env("HOME", tempdir.path())
            .args(["init", "--agent", agent, "--mode", "plan"])
            .assert()
            .failure()
            .stderr(predicates::str::contains(
                "does not support mode configuration through `acps init`",
            ));
    }
}

#[test]
fn init_rejects_mode_flag_for_custom_agents() {
    let tempdir = tempfile::tempdir().expect("tempdir");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "init",
            "--custom-agent-id",
            "bespoke",
            "--custom-agent-command",
            "bespoke",
            "--custom-agent-install",
            "true",
            "--mode",
            "plan",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "cannot be used with '--mode <MODE>'",
        ));
}

#[test]
fn init_model_setup_does_not_write_agent_mode() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    write_workspace_init_config(tempdir.path());
    seed_init_secrets(tempdir.path(), &[("OPENAI_API_KEY", "test-openai-key")]);
    let options_path =
        write_acp_config_options(tempdir.path(), &["openai/gpt-5.5"], &["build", "plan"]);

    acps_with_empty_path(tempdir.path())
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args([
            "init",
            "--agent",
            "opencode",
            "--provider",
            "openai",
            "--api-key-ref",
            "OPENAI_API_KEY",
            "--model",
            "openai/gpt-5.5",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("advertised modes").not());

    let config = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    assert!(!config.contains(r#"mode = "plan""#));
    assert!(!config.contains(r#"mode = "build""#));
    assert!(provider_configure_payload(tempdir.path()).contains(r#""mode_action":"Skipped""#));
}

/// The `provider_configure` step payload, which records what each lane did.
fn provider_configure_step(home: &std::path::Path) -> InitStepRecord {
    let store = StateStore::open(default_state_path(home)).expect("state store");
    let run = store
        .latest_init_run()
        .expect("latest init run")
        .expect("init run exists");
    let steps = store.query_init_steps(&run.id).expect("init steps");
    steps
        .iter()
        .find(|step| step.kind == "provider_configure")
        .unwrap_or_else(|| panic!("provider_configure recorded: {steps:?}"))
        .clone()
}

fn provider_configure_payload(home: &std::path::Path) -> String {
    provider_configure_step(home).payload_json
}

#[test]
fn init_explicit_mode_writes_agent_mode() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    write_workspace_init_config(tempdir.path());
    seed_init_secrets(tempdir.path(), &[("OPENAI_API_KEY", "test-openai-key")]);
    let options_path =
        write_acp_config_options(tempdir.path(), &["openai/gpt-5.5"], &["build", "plan"]);

    acps_with_empty_path(tempdir.path())
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args([
            "init",
            "--agent",
            "opencode",
            "--provider",
            "openai",
            "--api-key-ref",
            "OPENAI_API_KEY",
            "--model",
            "openai/gpt-5.5",
            "--mode",
            "plan",
        ])
        .assert()
        .success();

    let config = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    assert!(config.contains(r#"mode = "plan""#));
    assert!(config.contains(r#"model = "openai/gpt-5.5""#));
    let payload = provider_configure_payload(tempdir.path());
    assert!(
        payload.contains(r#""mode_action":"Set""#),
        "payload {payload}"
    );
    assert!(
        payload.contains(r#""model_action":"Set""#),
        "payload {payload}"
    );
}

#[test]
fn init_rejects_mode_not_advertised_by_the_agent() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    write_workspace_init_config(tempdir.path());
    seed_init_secrets(tempdir.path(), &[("OPENAI_API_KEY", "test-openai-key")]);
    let options_path =
        write_acp_config_options(tempdir.path(), &["openai/gpt-5.5"], &["build", "plan"]);

    acps_with_empty_path(tempdir.path())
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args([
            "init",
            "--agent",
            "opencode",
            "--provider",
            "openai",
            "--api-key-ref",
            "OPENAI_API_KEY",
            "--mode",
            "bogus",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("advertised modes: [build, plan]"));

    let config_text = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    let config = load_config_from_str(&config_text).expect("canonical config parses");
    assert!(config.agent.mode.is_none());
}

#[test]
fn init_mode_lane_runs_for_an_agent_without_model_support() {
    // amp declares set_model=false/set_mode=true: the mode lane must be
    // reachable even though the model lane returns before any discovery.
    let tempdir = tempfile::tempdir().expect("tempdir");
    write_workspace_init_config(tempdir.path());
    seed_init_secrets(tempdir.path(), &[("AMP_API_KEY", "test-amp-key")]);
    let options_path = write_acp_config_options(tempdir.path(), &[], &["smart", "rush", "deep"]);

    acps_with_empty_path(tempdir.path())
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args(["init", "--agent", "amp", "--mode", "deep"])
        .assert()
        .success();

    let config_text = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    let config = load_config_from_str(&config_text).expect("canonical config parses");
    assert_eq!(config.agent.mode.as_deref(), Some("deep"));
    assert!(config.agent.model.is_none());
    assert!(config.agent.provider.is_none());
}

#[test]
fn init_resume_reapplies_a_recorded_mode_instead_of_replaying_provider_configure_as_skipped() {
    // amp has neither a provider nor a model lane, so `--mode` is the only
    // explicit flag the `provider_configure` verifier can see: without its
    // `args.mode.is_none()` term the prior succeeded row would replay as
    // skipped and the recorded mode would never be re-applied.
    let tempdir = tempfile::tempdir().expect("tempdir");
    write_workspace_init_config(tempdir.path());
    seed_init_secrets(tempdir.path(), &[("AMP_API_KEY", "test-amp-key")]);
    let options_path = write_acp_config_options(tempdir.path(), &[], &["smart", "rush", "deep"]);
    // Fail the run after `provider_configure`: the edge step cannot create its
    // artifact directory while a file sits at that path.
    let cloudflared_dir = tempdir.path().join(".config/acp-stack/cloudflared");
    fs::write(&cloudflared_dir, "not a directory").expect("edge path collision");

    let edge_args = [
        "--edge",
        "cloudflare",
        "--exposure",
        "tunnel",
        "--hostname",
        "agent.example.com",
        "--cloudflared-deployment",
        "external",
    ];
    let output = acps_with_empty_path(tempdir.path())
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args(["init", "--agent", "amp", "--mode", "deep"])
        .args(edge_args)
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let stderr = String::from_utf8(output).expect("stderr should be utf8");
    let run_id = stderr
        .lines()
        .find_map(|line| line.strip_prefix("init failed in run "))
        .expect("stderr should include failed init run id");
    assert_eq!(provider_configure_step(tempdir.path()).status, "succeeded");

    fs::remove_file(&cloudflared_dir).expect("clear the edge path collision");
    acps_with_empty_path(tempdir.path())
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args(["init", "--resume", "--run-id", run_id])
        .assert()
        .success();

    let step = provider_configure_step(tempdir.path());
    assert_eq!(
        step.status, "succeeded",
        "a resumed run carrying a recorded --mode must re-run provider_configure: {step:?}"
    );
    assert!(
        step.payload_json.contains(r#""mode_action":"Set""#),
        "payload {}",
        step.payload_json
    );
    let config_text = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    let config = load_config_from_str(&config_text).expect("canonical config parses");
    assert_eq!(config.agent.mode.as_deref(), Some("deep"));
}

#[test]
fn init_codex_openrouter_writes_both_explicit_model_and_mode() {
    // codex+openrouter takes `--model` verbatim without consulting the
    // advertised list; the mode lane must still reach the shared session.
    let tempdir = tempfile::tempdir().expect("tempdir");
    write_workspace_init_config(tempdir.path());
    seed_init_secrets(
        tempdir.path(),
        &[("OPENROUTER_API_KEY", "test-openrouter-key")],
    );
    let options_path = write_acp_config_options(
        tempdir.path(),
        &["gpt-5.5"],
        &["read-only", "auto", "full-access"],
    );

    acps_with_empty_path(tempdir.path())
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args([
            "init",
            "--agent",
            "codex",
            "--provider",
            "openrouter",
            "--api-key-ref",
            "OPENROUTER_API_KEY",
            "--model",
            "deepseek/deepseek-v4-flash",
            "--mode",
            "auto",
        ])
        .assert()
        .success();

    let config_text = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    let config = load_config_from_str(&config_text).expect("canonical config parses");
    assert_eq!(config.agent.mode.as_deref(), Some("auto"));
    let provider = config.agent.provider.as_ref().expect("provider configured");
    assert_eq!(
        provider.model.as_deref(),
        Some("deepseek/deepseek-v4-flash")
    );
}

#[test]
fn init_kimi_pins_its_model_once_and_still_reaches_the_mode_lane() {
    // Kimi's model is pinned without discovery; the pin must happen exactly
    // once and must not suppress the mode lane that follows it.
    let tempdir = tempfile::tempdir().expect("tempdir");
    write_workspace_init_config(tempdir.path());
    seed_init_secrets(tempdir.path(), &[("KIMI_API_KEY", "test-kimi-key")]);
    let options_path = write_acp_config_options(tempdir.path(), &[], &["default", "plan"]);

    acps_with_empty_path(tempdir.path())
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args([
            "init",
            "--agent",
            "kimi",
            "--provider",
            "kimi-code",
            "--mode",
            "plan",
        ])
        .assert()
        .success();

    let config_text = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    assert_eq!(config_text.matches("model = ").count(), 1);
    let config = load_config_from_str(&config_text).expect("canonical config parses");
    assert_eq!(
        config
            .agent
            .provider
            .as_ref()
            .and_then(|provider| provider.model.as_deref()),
        Some("kimi-for-coding")
    );
    assert_eq!(config.agent.mode.as_deref(), Some("plan"));
}

#[test]
fn init_without_mode_flag_never_enters_the_mode_lane() {
    // An unattended run for a mode-capable agent with an unspawnable binary
    // must not attempt discovery on the mode lane's behalf: no failure, no
    // mode written, and the step payload records the skip.
    let tempdir = tempfile::tempdir().expect("tempdir");
    write_workspace_init_config(tempdir.path());
    seed_init_secrets(tempdir.path(), &[("AMP_API_KEY", "test-amp-key")]);

    acps_with_empty_path(tempdir.path())
        .env("HOME", tempdir.path())
        .args(["init", "--agent", "amp"])
        .assert()
        .success()
        .stdout(predicates::str::contains("discovery skipped").not());

    let config_text = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    let config = load_config_from_str(&config_text).expect("canonical config parses");
    assert!(config.agent.mode.is_none());
    assert!(provider_configure_payload(tempdir.path()).contains(r#""mode_action":"Skipped""#));
}
