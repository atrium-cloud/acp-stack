//! Model-aware discovery: the provisional probe applies the caller's model before reading the
//! advertised option set, so per-model lists (reasoning effort above all) describe that model.

use std::time::Duration;

use acp_stack::config::{AgentProviderConfig, Config};
use acp_stack::runtime::agent::acp_bridge::AgentSessionConfigCategory;
use acp_stack::runtime::agent::model_discovery::{
    advertised_values_for_category, fetch_session_config_with_timeout,
};
use agent_client_protocol::schema::v1::{
    NewSessionResponse, SessionConfigKind, SessionConfigOptionCategory,
};
use tempfile::TempDir;

use crate::common::agent::{EnvVarGuard, test_config};

const PROBE_TIMEOUT: Duration = Duration::from_secs(20);
const MODEL_A: &str = "fixture/model-a";
const MODEL_B: &str = "fixture/model-b";
/// Effort values the placebo advertises for `MODEL_A`, the model its select starts on.
const MODEL_A_EFFORTS: [&str; 2] = ["low", "medium"];
/// Effort values that replace them once `MODEL_B` is applied.
const MODEL_B_EFFORTS: [&str; 2] = ["high", "xhigh"];

/// A placebo advertising a two-value model select plus a reasoning-effort select whose values
/// depend on the applied model, the shape pi-acp and OpenCode present.
fn per_model_effort_config(extra_args: &[&str]) -> Config {
    let mut config = test_config();
    let mut args = vec![
        "acp".to_owned(),
        "--config-option-select".to_owned(),
        format!("model@model={MODEL_A}:{MODEL_A},{MODEL_B}"),
        "--config-option-select".to_owned(),
        format!(
            "reasoning_effort@thought_level={}:{}",
            MODEL_A_EFFORTS[1],
            MODEL_A_EFFORTS.join(",")
        ),
        "--config-option-select-for-model".to_owned(),
        format!(
            "{MODEL_B}=reasoning_effort@thought_level={}:{}",
            MODEL_B_EFFORTS[0],
            MODEL_B_EFFORTS.join(",")
        ),
    ];
    args.extend(extra_args.iter().map(|arg| (*arg).to_owned()));
    config.agent.args = args;
    config
}

/// The effort values the probe ends on, plus the model the placebo reports as current. The
/// placebo echoes an applied value as `current` only under the config id the set actually named,
/// so the second half pins the wire shape of the set.
async fn discovered_efforts_and_model(
    config: &Config,
    model: Option<&str>,
) -> (Vec<String>, Option<String>) {
    let tempdir = TempDir::new().expect("tempdir");
    discovered_efforts_and_model_in(tempdir.path(), config, model).await
}

async fn discovered_efforts_and_model_in(
    home: &std::path::Path,
    config: &Config,
    model: Option<&str>,
) -> (Vec<String>, Option<String>) {
    let discovered = fetch_session_config_with_timeout(home, config, model, PROBE_TIMEOUT)
        .await
        .expect("discovery");
    let applied = discovered.applied();
    let efforts = advertised_values_for_category(&applied, AgentSessionConfigCategory::Effort)
        .expect("advertised efforts");
    (efforts, current_model_value(&applied))
}

async fn discovered_efforts(config: &Config, model: Option<&str>) -> Vec<String> {
    discovered_efforts_and_model(config, model).await.0
}

/// The `current` value of the select carrying the model category.
fn current_model_value(response: &NewSessionResponse) -> Option<String> {
    let option = response
        .config_options
        .as_ref()?
        .iter()
        .find(|option| option.category.as_ref() == Some(&SessionConfigOptionCategory::Model))?;
    match &option.kind {
        SessionConfigKind::Select(select) => Some(select.current_value.0.to_string()),
        _ => None,
    }
}

#[tokio::test]
async fn a_requested_model_is_applied_before_the_options_are_read() {
    let _fixture_guard = EnvVarGuard::unset("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH");

    let (efforts, current_model) =
        discovered_efforts_and_model(&per_model_effort_config(&[]), Some(MODEL_B)).await;

    assert_eq!(efforts, MODEL_B_EFFORTS, "the applied model's effort list");
    assert_eq!(
        current_model.as_deref(),
        Some(MODEL_B),
        "the set must name the model option's own config id and the resolved value"
    );
}

/// An adapter that answers a set with only the options its change touched must not cost the
/// caller the rest of the advertisement.
#[tokio::test]
async fn a_partial_set_response_is_overlaid_onto_the_session_new_options() {
    let _fixture_guard = EnvVarGuard::unset("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH");
    let config = per_model_effort_config(&["--set-config-option-omits-model"]);
    let tempdir = TempDir::new().expect("tempdir");

    let discovered =
        fetch_session_config_with_timeout(tempdir.path(), &config, Some(MODEL_B), PROBE_TIMEOUT)
            .await
            .expect("discovery");

    let models =
        advertised_values_for_category(&discovered.response, AgentSessionConfigCategory::Model)
            .expect("session/new advertises every model");
    assert_eq!(models, vec![MODEL_A.to_owned(), MODEL_B.to_owned()]);
    let applied = discovered.applied();
    assert_eq!(
        advertised_values_for_category(&applied, AgentSessionConfigCategory::Model)
            .expect("the overlay keeps the model option the set omitted"),
        models,
    );
    assert_eq!(
        advertised_values_for_category(&applied, AgentSessionConfigCategory::Effort)
            .expect("advertised efforts"),
        MODEL_B_EFFORTS,
        "the options the set did return still govern"
    );
}

/// Adapters advertise their own provider ids, so the probe must resolve against the agent-native
/// id rather than the canonical one the config records.
#[tokio::test]
async fn a_mapped_provider_id_resolves_the_advertised_model_value() {
    let _fixture_guard = EnvVarGuard::unset("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH");
    // OpenCode calls the canonical `fireworks` provider `fireworks-ai`, so only the mapped id
    // matches the advertised `fireworks-ai/model-x`.
    let native_model = "fireworks-ai/model-x";
    let mut config = test_config();
    config.agent.provider = Some(AgentProviderConfig {
        id: "fireworks".to_owned(),
        model: None,
        api_key_ref: Some("FIREWORKS_API_KEY".to_owned()),
        custom: None,
    });
    config.agent.env = vec!["FIREWORKS_API_KEY".to_owned()];
    config.agent.args = vec![
        "acp".to_owned(),
        "--config-option-select".to_owned(),
        format!("model@model=other/model-x:other/model-x,{native_model}"),
        "--config-option-select".to_owned(),
        format!(
            "reasoning_effort@thought_level={}:{}",
            MODEL_A_EFFORTS[1],
            MODEL_A_EFFORTS.join(",")
        ),
        "--config-option-select-for-model".to_owned(),
        format!(
            "{native_model}=reasoning_effort@thought_level={}:{}",
            MODEL_B_EFFORTS[0],
            MODEL_B_EFFORTS.join(",")
        ),
    ];

    // A configured provider sends the launch environment through the secret store.
    let tempdir = TempDir::new().expect("tempdir");
    let mut secrets =
        acp_stack::secrets::SecretStore::open_or_create(tempdir.path()).expect("secret store");
    secrets
        .set_many([("FIREWORKS_API_KEY", "test-fireworks-key")])
        .expect("provider secret");

    let (efforts, current_model) =
        discovered_efforts_and_model_in(tempdir.path(), &config, Some("model-x")).await;

    assert_eq!(
        current_model.as_deref(),
        Some(native_model),
        "the probe must resolve through the agent-native provider id"
    );
    assert_eq!(efforts, MODEL_B_EFFORTS);
}

/// A model the session already sits on needs no round trip, so a harness that would reject the
/// set never sees one.
#[tokio::test]
async fn an_already_current_model_is_not_re_applied() {
    let _fixture_guard = EnvVarGuard::unset("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH");

    let (efforts, current_model) = discovered_efforts_and_model(
        &per_model_effort_config(&["--fail-set-config-option"]),
        Some(MODEL_A),
    )
    .await;

    assert_eq!(efforts, MODEL_A_EFFORTS);
    assert_eq!(current_model.as_deref(), Some(MODEL_A));
}

#[tokio::test]
async fn no_model_reads_the_boot_model_options() {
    let _fixture_guard = EnvVarGuard::unset("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH");

    let efforts = discovered_efforts(&per_model_effort_config(&[]), None).await;

    assert_eq!(efforts, MODEL_A_EFFORTS, "no model, no set");
}

#[tokio::test]
async fn a_disk_only_harness_is_never_asked_to_switch_models() {
    let _fixture_guard = EnvVarGuard::unset("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH");
    let mut config = per_model_effort_config(&[]);
    // Hermes Agent pins its model in config.yaml and reads it at process start, so the advertised
    // list is an echo and the ACP set would only fail spuriously.
    config.agent.id = "hermes".to_owned();
    config.agent.name = "Hermes Agent".to_owned();

    let efforts = discovered_efforts(&config, Some(MODEL_B)).await;

    assert_eq!(efforts, MODEL_A_EFFORTS, "the set must be skipped");
}

#[tokio::test]
async fn a_rejected_set_keeps_the_session_new_options() {
    let _fixture_guard = EnvVarGuard::unset("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH");

    let efforts = discovered_efforts(
        &per_model_effort_config(&["--fail-set-config-option"]),
        Some(MODEL_B),
    )
    .await;

    assert_eq!(
        efforts, MODEL_A_EFFORTS,
        "a failed set must not fail discovery"
    );
}

#[tokio::test]
async fn an_empty_set_response_keeps_the_session_new_options() {
    let _fixture_guard = EnvVarGuard::unset("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH");

    // A lax adapter carries the refresh only in a notification, which a probe session never reads.
    let efforts = discovered_efforts(
        &per_model_effort_config(&["--set-config-option-responds-empty"]),
        Some(MODEL_B),
    )
    .await;

    assert_eq!(efforts, MODEL_A_EFFORTS);
}

#[tokio::test]
async fn an_unadvertised_model_keeps_the_session_new_options() {
    let _fixture_guard = EnvVarGuard::unset("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH");

    let efforts =
        discovered_efforts(&per_model_effort_config(&[]), Some("fixture/no-such-model")).await;

    assert_eq!(efforts, MODEL_A_EFFORTS);
}

/// The probe child must be reaped on the model-application path too, not just on the
/// `session/new` paths that already had teardown coverage.
#[cfg(unix)]
#[tokio::test]
async fn the_probe_child_is_reaped_after_a_model_application() {
    let _fixture_guard = EnvVarGuard::unset("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH");
    let tempdir = TempDir::new().expect("tempdir");
    let pid_path = tempdir.path().join("placebo-agent.pid");
    let mut config = per_model_effort_config(&["--write-pid"]);
    config
        .agent
        .args
        .push(pid_path.to_string_lossy().into_owned());

    let discovered =
        fetch_session_config_with_timeout(tempdir.path(), &config, Some(MODEL_B), PROBE_TIMEOUT)
            .await
            .expect("discovery");
    assert!(discovered.refreshed_options.is_some(), "the set landed");

    let pid_text = std::fs::read_to_string(&pid_path).expect("pid written");
    let pid: u32 = pid_text.trim().parse().expect("pid parses");
    for _ in 0..40 {
        // SAFETY: signal 0 only probes for the process, it delivers nothing.
        let alive = unsafe { libc::kill(pid as i32, 0) } == 0;
        if !alive && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("probe child {pid} outlived the discovery call");
}
