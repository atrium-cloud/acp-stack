//! Ignored integration tests for real ACP agents.
//!
//! These exercise ACP `session/new` model config options against installed
//! agents rather than the fake test agent. Run explicitly with:
//!
//! `ACP_STACK_RUN_REAL_AGENT_TESTS=1 cargo test --test real_agent_model_options_tests -- --ignored`

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use acp_stack::config::AgentConfig;
use acp_stack::runtime::agent::acp_bridge::{
    AcpBridge, AgentSessionConfigCategory, SessionEventSink, session_config_values,
    session_model_selection_for_value, session_model_values,
};
use agent_client_protocol::schema::v1::{ContentBlock, PromptRequest, TextContent};

const REAL_AGENT_TESTS_ENV: &str = "ACP_STACK_RUN_REAL_AGENT_TESTS";

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
        providers: None,
        subagent: None,
        auto_update: None,
        install: None,
    }
}

fn required_env(names: &[&str]) -> HashMap<String, String> {
    require_real_agent_tests_enabled();
    names
        .iter()
        .map(|name| {
            let value = std::env::var(name)
                .unwrap_or_else(|_| panic!("real-agent test requires `{name}` in the environment"));
            ((*name).to_owned(), value)
        })
        .collect()
}

fn require_real_agent_tests_enabled() {
    if std::env::var(REAL_AGENT_TESTS_ENV).as_deref() != Ok("1") {
        panic!("real-agent tests require `{REAL_AGENT_TESTS_ENV}=1`");
    }
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
        &Default::default(),
        None,
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
        &Default::default(),
        None,
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
        &Default::default(),
        None,
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
        .expect("real ACP agent should complete prompt")
        .stop_reason;
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

// Deterministic client-terminal acceptance probe: force one real harness into
// a shell action and assert the command surfaced through our terminal/create
// handler as an `acp`-origin row in the durable command log. This is the
// milestone gate for claiming terminal support end-to-end.
async fn real_terminal_uname_probe(agent: AgentConfig, env: HashMap<String, String>) {
    let cwd = std::env::current_dir().expect("current dir");
    let state_dir = tempfile::tempdir().expect("tempdir");
    let store = acp_stack::state::StateStore::open(state_dir.path().join("state.sqlite"))
        .expect("state open");
    store.migrate().expect("migrate");
    let state = Arc::new(tokio::sync::Mutex::new(store));

    let bridge = AcpBridge::spawn(
        &agent,
        env,
        cwd.clone(),
        Arc::new(NoopSessionEventSink),
        None,
        &Default::default(),
        None,
        Some(acp_stack::runtime::agent::acp_bridge::TerminalCommandLog {
            state: state.clone(),
            event_hub: acp_stack::events::EventHub::new(),
        }),
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
    // Pin the default test model before prompting; a fresh OpenCode session
    // has no model selected and fails the prompt otherwise.
    let values = session_model_values(&response).expect("advertised models");
    let model_value = values
        .iter()
        .find(|value| {
            value.as_str() == "opencode-go/deepseek-v4-flash"
                || model_base_matches(value, "opencode-go", "deepseek-v4-flash")
        })
        .cloned()
        .expect("deepseek-v4-flash advertised");
    let acp_stack::runtime::agent::acp_bridge::AgentSessionModelSelection::ConfigOption {
        config_id,
    } = session_model_selection_for_value(&response, &model_value).expect("model selection");
    bridge
        .set_session_config_option(response.session_id.clone(), &config_id, &model_value)
        .await
        .expect("set model config option");

    let request = PromptRequest::new(
        response.session_id,
        vec![ContentBlock::Text(TextContent::new(
            "run `uname -a` and report the output",
        ))],
    );
    let stop = tokio::time::timeout(Duration::from_secs(180), bridge.prompt_session(request))
        .await
        .expect("real ACP agent prompt timed out")
        .expect("real ACP agent should complete prompt")
        .stop_reason;
    println!("{} terminal probe stop reason: {stop:?}", agent.id);
    bridge.shutdown().await.expect("real ACP agent shutdown");

    let commands = state
        .lock()
        .await
        .query_commands(acp_stack::state::CommandFilter {
            limit: 50,
            ..Default::default()
        })
        .expect("query commands");
    let acp_rows: Vec<_> = commands.iter().filter(|row| row.origin == "acp").collect();
    println!(
        "acp-origin command rows: {:?}",
        acp_rows.iter().map(|row| &row.command).collect::<Vec<_>>()
    );
    assert!(
        acp_rows.iter().any(|row| row.command.contains("uname")),
        "expected the agent to run `uname -a` through a client terminal; acp-origin rows: {:?}",
        acp_rows.iter().map(|row| &row.command).collect::<Vec<_>>()
    );
}

#[tokio::test]
#[ignore = "requires installed OpenCode and OPENCODE_API_KEY; sends one real prompt"]
async fn real_opencode_terminal_uname_probe() {
    real_terminal_uname_probe(
        real_agent_config("opencode", "OpenCode", "opencode", &["acp"]),
        required_env(&["OPENCODE_API_KEY"]),
    )
    .await;
}

#[tokio::test]
#[ignore = "requires installed pi-acp and OPENCODE_API_KEY; sends one real prompt"]
async fn real_pi_terminal_uname_probe() {
    real_terminal_uname_probe(
        real_agent_config("pi", "Pi Agent", "pi-acp", &[]),
        required_env(&["OPENCODE_API_KEY"]),
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
