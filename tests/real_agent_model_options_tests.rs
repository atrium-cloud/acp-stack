//! Ignored integration tests for real ACP agents.
//!
//! These exercise ACP `session/new` model config options against installed
//! agents rather than the fake test agent. Run explicitly with:
//!
//! `cargo test --test real_agent_model_options_tests -- --ignored`

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use acp_stack::config::AgentConfig;
use acp_stack::runtime::agent::acp_bridge::{
    AcpBridge, AgentSessionConfigCategory, SessionEventSink, session_config_values,
    session_model_selection_for_value, session_model_values,
};
use agent_client_protocol::schema::{ContentBlock, PromptRequest, TextContent};

struct NoopSessionEventSink;

impl SessionEventSink for NoopSessionEventSink {
    fn append<'a>(
        &'a self,
        _session_id: &'a str,
        _kind: &'a str,
        _payload_json: &'a str,
    ) -> futures::future::BoxFuture<'a, ()> {
        Box::pin(async {})
    }
}

fn real_agent_config(id: &str, name: &str, command: &str, args: &[&str]) -> AgentConfig {
    AgentConfig {
        id: id.to_owned(),
        name: name.to_owned(),
        command: command.to_owned(),
        args: args.iter().map(|arg| (*arg).to_owned()).collect(),
        cwd: None,
        env: Vec::new(),
        expected_sha256: None,
        restart: "never".to_owned(),
        mode: None,
        model: None,
        harness_version: None,
        adapter: None,
        provider: None,
        subagent: None,
        install: None,
    }
}

fn required_env(names: &[&str]) -> HashMap<String, String> {
    names
        .iter()
        .map(|name| {
            let value = std::env::var(name)
                .unwrap_or_else(|_| panic!("real-agent test requires `{name}` in the environment"));
            ((*name).to_owned(), value)
        })
        .collect()
}

async fn assert_real_agent_advertises_model(
    agent: AgentConfig,
    env: HashMap<String, String>,
    provider: &str,
    model: &str,
) {
    let cwd = std::env::current_dir().expect("current dir");
    let bridge = AcpBridge::spawn(
        &agent,
        env,
        cwd.clone(),
        Arc::new(NoopSessionEventSink),
        None,
    )
    .await
    .expect("real ACP agent should initialize");

    let response = tokio::time::timeout(
        Duration::from_secs(120),
        bridge.new_session(cwd.clone(), Vec::new()),
    )
    .await
    .expect("real ACP agent session/new timed out")
    .expect("real ACP agent should create a session");

    let values = session_model_values(&response).expect("real ACP agent should advertise models");
    let provider_qualified = format!("{provider}/{model}");
    let expected = values
        .iter()
        .find(|value| {
            value.as_str() == provider_qualified || model_base_matches(value, provider, model)
        })
        .cloned()
        .unwrap_or_else(|| {
            panic!("expected provider `{provider}` model `{model}` in advertised model values: {values:?}")
        });
    session_model_selection_for_value(&response, &expected)
        .expect("expected model should validate against ACP model options");

    bridge.shutdown().await.expect("real ACP agent shutdown");
}

async fn print_real_agent_mode_values(agent: AgentConfig, env: HashMap<String, String>) {
    let cwd = std::env::current_dir().expect("current dir");
    let agent_id = agent.id.clone();
    let bridge = AcpBridge::spawn(
        &agent,
        env,
        cwd.clone(),
        Arc::new(NoopSessionEventSink),
        None,
    )
    .await
    .expect("real ACP agent should initialize");

    let response = tokio::time::timeout(
        Duration::from_secs(120),
        bridge.new_session(cwd.clone(), Vec::new()),
    )
    .await
    .expect("real ACP agent session/new timed out")
    .expect("real ACP agent should create a session");

    match session_config_values(
        response.config_options.as_deref(),
        AgentSessionConfigCategory::Mode,
    ) {
        Ok(values) => println!("{agent_id} mode values: {values:?}"),
        Err(error) => println!("{agent_id} mode values: <not advertised> ({error})"),
    }

    bridge.shutdown().await.expect("real ACP agent shutdown");
}

async fn send_real_agent_prompt(agent: AgentConfig, env: HashMap<String, String>, prompt: &str) {
    let cwd = std::env::current_dir().expect("current dir");
    let agent_id = agent.id.clone();
    let bridge = AcpBridge::spawn(
        &agent,
        env,
        cwd.clone(),
        Arc::new(NoopSessionEventSink),
        None,
    )
    .await
    .expect("real ACP agent should initialize");

    let response = tokio::time::timeout(
        Duration::from_secs(120),
        bridge.new_session(cwd.clone(), Vec::new()),
    )
    .await
    .expect("real ACP agent session/new timed out")
    .expect("real ACP agent should create a session");
    let request = PromptRequest::new(
        response.session_id,
        vec![ContentBlock::Text(TextContent::new(prompt))],
    );
    let stop = tokio::time::timeout(Duration::from_secs(180), bridge.prompt_session(request))
        .await
        .expect("real ACP agent prompt timed out")
        .expect("real ACP agent should complete prompt");
    println!("{agent_id} prompt stop reason: {stop:?}");

    bridge.shutdown().await.expect("real ACP agent shutdown");
}

fn model_base_matches(value: &str, provider: &str, model: &str) -> bool {
    let base = value.split_once('[').map_or(value, |(base, _)| base);
    if let Some((advertised_provider, advertised_model)) = base.split_once('/') {
        return advertised_provider == provider && advertised_model == model;
    }
    base == model
}

#[tokio::test]
#[ignore = "requires installed OpenCode, pi-acp, Cursor CLI, amp-acp, OPENCODE_API_KEY, CURSOR_API_KEY, and AMP_API_KEY"]
async fn real_agents_print_mode_values() {
    print_real_agent_mode_values(
        real_agent_config("opencode", "OpenCode", "opencode", &["acp"]),
        required_env(&["OPENCODE_API_KEY"]),
    )
    .await;
    print_real_agent_mode_values(
        real_agent_config("pi", "Pi Agent", "pi-acp", &[]),
        required_env(&["OPENCODE_API_KEY"]),
    )
    .await;
    print_real_agent_mode_values(
        real_agent_config("cursor", "Cursor CLI", "cursor-agent", &["acp"]),
        required_env(&["CURSOR_API_KEY"]),
    )
    .await;
    print_real_agent_mode_values(
        real_agent_config("amp", "Amp Code", "amp-acp", &[]),
        required_env(&["AMP_API_KEY"]),
    )
    .await;
}

#[tokio::test]
#[ignore = "requires installed amp-acp and AMP_API_KEY; sends one real Amp prompt"]
async fn real_amp_prompt_probe_for_dashboard() {
    send_real_agent_prompt(
        real_agent_config("amp", "Amp Code", "amp-acp", &[]),
        required_env(&["AMP_API_KEY"]),
        "Reply with exactly: amp acp probe",
    )
    .await;
}

#[tokio::test]
#[ignore = "requires installed OpenCode and OPENCODE_API_KEY"]
async fn real_opencode_advertises_opencode_go_deepseek_model() {
    assert_real_agent_advertises_model(
        real_agent_config("opencode", "OpenCode", "opencode", &["acp"]),
        required_env(&["OPENCODE_API_KEY"]),
        "opencode-go",
        "deepseek-v4-flash",
    )
    .await;
}

#[tokio::test]
#[ignore = "requires installed pi-acp and OPENCODE_API_KEY"]
async fn real_pi_advertises_opencode_go_deepseek_model() {
    assert_real_agent_advertises_model(
        real_agent_config("pi", "Pi Agent", "pi-acp", &[]),
        required_env(&["OPENCODE_API_KEY"]),
        "opencode-go",
        "deepseek-v4-flash",
    )
    .await;
}

#[tokio::test]
#[ignore = "requires installed Cursor CLI and CURSOR_API_KEY"]
async fn real_cursor_advertises_openai_gpt_5_5_model() {
    assert_real_agent_advertises_model(
        real_agent_config("cursor", "Cursor CLI", "cursor-agent", &["acp"]),
        required_env(&["CURSOR_API_KEY"]),
        "openai",
        "gpt-5.5",
    )
    .await;
}
