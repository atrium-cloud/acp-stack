//! Category settlement across the state surface: the terminal sweep, the
//! config-derived settlements a resumed or declared run reports, and the
//! frontier's behaviour under cancel and parked failures.

use super::super::*;
use super::support::*;

use crate::runtime::init_runner::StepDisposition;

use serde_json::json;

#[test]
fn a_cancel_mid_prompt_freezes_the_category_frontier() {
    // The wizard thread does not stop where the cancel lands: it unwinds
    // through the lane's own failure badge and the step's finish signal.
    // Neither may record a frame after the terminal one, and neither may
    // move the snapshot `hello` and the status route derive live.
    let session = test_session("init_state_cancel_freeze");
    session.apply_state_signal(InitStateSignal::StepStarted {
        kind: step_kind::PROVIDER_CONFIGURE,
    });
    let driver = SessionPromptDriver {
        session: session.clone(),
    };
    let request = hosted_test_request(
        HostedPromptKind::ProviderId,
        HostedPromptStyle::Select,
        "select a provider",
        &["openrouter"],
    );
    let handle = std::thread::spawn(move || driver.select(request));
    wait_for_pending_input(&session);
    session.cancel("client canceled");
    handle
        .join()
        .expect("driver thread")
        .expect_err("a canceled session must release the pending prompt");

    session.apply_state_signal(InitStateSignal::CategoryFailed {
        category: InitCategory::Provider,
        code: "init.cancelled".to_owned(),
    });
    session.apply_state_signal(InitStateSignal::StepFinished {
        kind: step_kind::PROVIDER_CONFIGURE,
        disposition: StepDisposition::Executed,
        error_code: Some("init.cancelled".to_owned()),
    });

    let events = session.events_after(0);
    assert_eq!(
        events.last().expect("session recorded no events")["type"],
        json!("cancelled"),
        "the cancellation must be the last thing the client is told: {events:?}"
    );
    for frontier in [folded_state(&session), folded_from_hello(&session)] {
        for id in CANONICAL_CATEGORY_IDS {
            assert_ne!(
                category(&frontier, id)["status"],
                json!("failed"),
                "category `{id}` was badged failed after the session was canceled"
            );
        }
    }
}

#[test]
fn a_cross_cutting_prompt_records_input_required_with_no_signal() {
    // `secret_ref_value` belongs to no category, and a prompt never emits a
    // signal anyway: the ask is announced by `input_required` alone, and the
    // fold shows nothing awaiting because the pending kind maps to no category.
    let session = test_session("init_state_cross_cutting");
    let driver = SessionPromptDriver {
        session: session.clone(),
    };
    let request = HostedPromptRequest {
        kind: HostedPromptKind::SecretRefValue,
        style: HostedPromptStyle::Password,
        prompt: "LINEAR_API_KEY".to_owned(),
        required: false,
        default: None,
        items: Vec::new(),
        inspection: None,
    };
    let handle = std::thread::spawn(move || driver.password(request));
    let pending = wait_for_pending_input(&session);
    assert!(
        signal_events(&session).is_empty(),
        "a category-less prompt must raise no signal"
    );
    assert_eq!(
        session
            .events_after(0)
            .last()
            .expect("session recorded no events")["type"],
        json!("input_required")
    );
    assert!(
        awaiting_ids(&folded_state(&session)).is_empty(),
        "a category-less prompt leaves nothing awaiting in the fold"
    );
    session
        .submit_input(&pending.request_id, json!(null))
        .expect("submit input");
    handle.join().expect("driver thread").expect("password");
    assert!(
        signal_events(&session).is_empty(),
        "answering a category-less prompt must raise no signal either"
    );
}

#[test]
fn blocked_on_follows_the_dependency_table() {
    let session = test_session("init_state_blocked");
    let fresh = folded_from_hello(&session);
    assert_eq!(category(&fresh, "model")["blocked_on"], json!("provider"));
    session.apply_state_signal(InitStateSignal::CategorySettled {
        category: InitCategory::Agent,
        value: Some("opencode".to_owned()),
    });
    let after_agent = latest_state(&session);
    assert_eq!(category(&after_agent, "provider")["status"], json!("ready"));
    assert_eq!(
        category(&after_agent, "model")["blocked_on"],
        json!("provider")
    );

    session.apply_state_signal(InitStateSignal::CategorySettled {
        category: InitCategory::Provider,
        value: Some("openrouter".to_owned()),
    });
    let after_provider = latest_state(&session);
    assert_eq!(category(&after_provider, "model")["status"], json!("ready"));
    assert_eq!(
        category(&after_provider, "model")["blocked_on"],
        Value::Null
    );
    assert_eq!(
        category(&after_provider, "mode")["blocked_on"],
        json!("model")
    );

    // An inapplicable dependency unblocks just like a settled one.
    session.apply_state_signal(InitStateSignal::CategoryApplicability {
        category: InitCategory::Model,
        applicable: false,
        source: ApplicabilitySource::Registry,
        reason: Some("agent does not take a model".to_owned()),
    });
    assert_eq!(
        category(&latest_state(&session), "mode")["status"],
        json!("ready")
    );
}

#[test]
fn a_repeated_signal_is_forwarded_but_folds_idempotently() {
    // The instance no longer dedups — every fact is forwarded, so a repeated
    // signal is a second event on the wire. The dedup that used to live here is
    // the client's: folding the stream is idempotent, so the view is unchanged.
    let session = test_session("init_state_dedup");
    let settled = || InitStateSignal::CategorySettled {
        category: InitCategory::Agent,
        value: Some("opencode".to_owned()),
    };
    session.apply_state_signal(settled());
    let once = folded_state(&session);
    session.apply_state_signal(settled());
    assert_eq!(
        signal_events(&session).len(),
        2,
        "both facts ride the wire; the instance does not dedup"
    );
    assert_eq!(
        folded_state(&session),
        once,
        "folding the repeated signal must reach the same view"
    );
}

#[test]
fn history_cap_evicts_signals_while_the_hello_replay_stays_current() {
    let session = test_session("init_state_cap");
    let driver = SessionPromptDriver {
        session: session.clone(),
    };
    session.apply_state_signal(InitStateSignal::CategorySettled {
        category: InitCategory::Agent,
        value: Some("opencode".to_owned()),
    });
    for index in 0..INIT_EVENT_HISTORY_LIMIT + 1 {
        driver.progress(format!("step {index}"));
    }
    assert!(
        signal_events(&session).is_empty(),
        "the early signal should have aged out of the capped history"
    );
    // Which is exactly why the signal log — bounded by init's structure, not by
    // progress chatter — rides hello in full, so a late joiner still folds the
    // settled agent.
    assert_eq!(
        category(&folded_from_hello(&session), "agent")["value"],
        json!("opencode")
    );
}

#[test]
fn init_complete_settles_every_applicable_category_left_open() {
    let session = test_session("init_state_sweep");
    session.apply_state_signal(InitStateSignal::CategorySettled {
        category: InitCategory::Agent,
        value: Some("opencode".to_owned()),
    });
    session.apply_state_signal(InitStateSignal::CategoryApplicability {
        category: InitCategory::Mode,
        applicable: false,
        source: ApplicabilitySource::Registry,
        reason: Some("agent does not take a mode".to_owned()),
    });
    let before = signal_events(&session).len();
    session.apply_state_signal(InitStateSignal::StepFinished {
        kind: step_kind::INIT_COMPLETE,
        disposition: StepDisposition::Executed,
        error_code: None,
    });
    // The sweep is the client's: the instance forwards one `step_finished`
    // signal for init_complete, and the fold settles every open lane from it.
    assert_eq!(signal_events(&session).len(), before + 1);

    let swept = latest_state(&session);
    assert_eq!(category(&swept, "agent")["value"], json!("opencode"));
    assert_eq!(category(&swept, "mode")["status"], json!("not_applicable"));
    for id in CANONICAL_CATEGORY_IDS {
        let status = category(&swept, id)["status"].clone();
        assert!(
            status == json!("settled") || status == json!("not_applicable"),
            "category `{id}` still derives as {status} after init completed"
        );
    }
    assert_eq!(category(&swept, "deps")["value"], Value::Null);
}

/// The real derivation a hosted run performs the instant its agent is
/// written, driven through the session so the wire snapshot — not just the
/// signal list — is what gets asserted.
fn apply_agent_settlement(session: &HostedInitSession, agent_id: &str, args: &InitArgs) {
    let mut config = settlement_fixture_config();
    config.agent.id = agent_id.to_owned();
    apply_settlement_signals(session, &config, args);
}

fn settlement_fixture_config() -> config::Config {
    config::load_config_from_str(include_str!(
        "../../../../../tests/fixtures/valid-opencode-stack.toml"
    ))
    .expect("fixture config")
}

fn apply_settlement_signals(session: &HostedInitSession, config: &config::Config, args: &InitArgs) {
    let registry = crate::runtime::install::agent_registry::RegistryCatalog::load_embedded()
        .expect("registry");
    for signal in super::super::super::run::agent_settlement_signals(config, &registry, args, false)
    {
        session.apply_state_signal(signal);
    }
}

#[test]
fn hosted_custom_agent_settles_the_agent_and_strands_no_harness_lane() {
    let args = request_from_json(
        r#"{
                "custom_agent_id": "housebot",
                "custom_agent_command": "housebot-acp",
                "custom_agent_install": "npm install -g housebot"
            }"#,
    )
    .into_init_args()
    .expect("valid request");
    let session = test_session("init_state_custom_agent");
    apply_agent_settlement(&session, "housebot", &args);

    let state = latest_state(&session);
    assert_eq!(category(&state, "agent")["status"], json!("settled"));
    assert_eq!(category(&state, "agent")["value"], json!("housebot"));
    // A registry-less agent takes its provider, model, mode, effort, and
    // skills from its own environment, so a client must never render those
    // lanes as input that is still coming.
    for id in ["provider", "model", "mode", "effort", "skills"] {
        assert_eq!(
            category(&state, id)["status"],
            json!("not_applicable"),
            "custom agents configure `{id}` outside acp-stack"
        );
    }
    // MCP has no registry column; only the live probe may rule on it.
    assert_eq!(category(&state, "mcp")["status"], json!("ready"));
}

// A resumed run replays its configuration steps as skipped and a declared
// run never prompts, so no write site fires on either path. Without the
// config-derived settlements the harness lanes would report `settled` with
// a null value, telling a client the run configured nothing.
#[test]
fn hosted_settlement_reports_the_harness_values_already_in_the_config() {
    let args = request_from_json(r#"{"resume": true, "agent": "opencode"}"#)
        .into_init_args()
        .expect("valid request");
    let mut config = settlement_fixture_config();
    config.agent.provider = Some(crate::config::AgentProviderConfig {
        id: "openrouter".to_owned(),
        model: Some("deepseek-v4-flash".to_owned()),
        api_key_ref: Some("OPENROUTER_API_KEY".to_owned()),
        custom: None,
    });
    config.agent.mode = Some("smart".to_owned());
    config.agent.effort = Some("high".to_owned());
    config.mcp.servers = vec![crate::config::McpServerConfig::Stdio(
        crate::config::McpStdioServer {
            name: "linear".to_owned(),
            command: "linear-mcp".to_owned(),
            args: Vec::new(),
            env: Vec::new(),
        },
    )];
    let session = test_session("init_state_declared_values");
    apply_settlement_signals(&session, &config, &args);

    let state = latest_state(&session);
    for (id, value) in [
        ("agent", "opencode"),
        ("provider", "openrouter"),
        // Provider-backed agents keep the model inside `[agent.provider]`.
        ("model", "deepseek-v4-flash"),
        ("mode", "smart"),
    ] {
        assert_eq!(category(&state, id)["status"], json!("settled"), "`{id}`");
        assert_eq!(
            category(&state, id)["value"],
            json!(value),
            "`{id}` must report what is on disk, not null"
        );
    }
    // The registry outranks the disk: opencode takes no effort, so a stray
    // `agent.effort` in config renders the lane not applicable rather than
    // settled with a value no session will honor.
    assert_eq!(
        category(&state, "effort")["status"],
        json!("not_applicable")
    );
    // MCP is the exception: declaring servers says nothing about whether
    // the installed agent can be handed any, so the lane is still open here
    // and the probe is what closes it.
    assert_eq!(category(&state, "mcp")["status"], json!("ready"));

    // The probe's turn. Both of its signals are what the capability step
    // emits, in that order.
    session.apply_state_signal(super::super::super::run::mcp_applicability_from_probe(
        &super::super::super::CapabilityProbeOutcome::Probed(mcp_capabilities(
            json!({"stdio": true}),
        )),
    ));
    let settlement = super::super::super::run::mcp_settlement_from_probe(
        &mcp_capabilities(json!({"stdio": true})),
        &config,
        &[],
    )
    .expect("a declared server the agent can take settles the lane");
    session.apply_state_signal(settlement);

    let state = latest_state(&session);
    assert_eq!(category(&state, "mcp")["status"], json!("settled"));
    assert_eq!(category(&state, "mcp")["value"], json!("linear"));
}

// The case the probe-first ordering exists for: the servers are declared,
// the installed agent advertises no MCP at all, and runtime will hand it
// nothing — so the lane must read as absent, not as configured.
#[test]
fn a_declared_mcp_server_stays_inapplicable_when_the_agent_advertises_none() {
    let args = request_from_json(r#"{"agent": "opencode"}"#)
        .into_init_args()
        .expect("valid request");
    let mut config = settlement_fixture_config();
    config.mcp.servers = vec![crate::config::McpServerConfig::Stdio(
        crate::config::McpStdioServer {
            name: "linear".to_owned(),
            command: "linear-mcp".to_owned(),
            args: Vec::new(),
            env: Vec::new(),
        },
    )];
    let session = test_session("init_state_declared_mcp_unsupported");
    apply_settlement_signals(&session, &config, &args);

    let silent = mcp_capabilities(json!({}));
    assert_eq!(
        super::super::super::run::mcp_settlement_from_probe(&silent, &config, &[]),
        None,
        "an agent that takes no MCP servers settles nothing"
    );
    session.apply_state_signal(super::super::super::run::mcp_applicability_from_probe(
        &super::super::super::CapabilityProbeOutcome::Probed(silent),
    ));

    let state = latest_state(&session);
    assert_eq!(category(&state, "mcp")["status"], json!("not_applicable"));
    assert_eq!(
        category(&state, "mcp")["reason"],
        json!("agent does not advertise MCP support")
    );
}

// The other model slot: an agent with no provider block keeps its model at
// the config root, and settlement has to read it from there.
#[test]
fn hosted_settlement_reads_a_root_model_for_a_provider_less_agent() {
    let args = request_from_json(r#"{"agent": "amp"}"#)
        .into_init_args()
        .expect("valid request");
    let mut config = settlement_fixture_config();
    config.agent.id = "amp".to_owned();
    config.agent.provider = None;
    config.agent.model = Some("gpt-5-codex".to_owned());
    let session = test_session("init_state_root_model");
    apply_settlement_signals(&session, &config, &args);

    let state = latest_state(&session);
    // amp declares `set_model = false`, so the lane still reads as absent —
    // what is asserted is that the settlement carried the root value, which
    // the wire shows the moment a model-taking agent is in the same shape.
    assert_eq!(
        category(&state, "model")["status"],
        json!("not_applicable"),
        "the registry verdict stands over a value init will not rewrite"
    );

    let session = test_session("init_state_root_model_applicable");
    config.agent.id = "opencode".to_owned();
    apply_settlement_signals(&session, &config, &args);
    let state = latest_state(&session);
    assert_eq!(category(&state, "model")["status"], json!("settled"));
    assert_eq!(category(&state, "model")["value"], json!("gpt-5-codex"));
}

#[test]
fn hosted_resume_settles_categories_from_replayed_steps() {
    let args = request_from_json(r#"{"resume": true, "agent": "opencode"}"#)
        .into_init_args()
        .expect("valid request");
    let session = test_session("init_state_resume");
    apply_agent_settlement(&session, "opencode", &args);

    // A resumed run replays already-verified rows as skipped; the category
    // behind each one must settle exactly as an executed step settles it,
    // or the snapshot would report work that will never be driven again.
    for kind in [
        step_kind::AGENT_INSTALL,
        step_kind::WORKSPACE_MATERIALIZE,
        step_kind::PROVIDER_CONFIGURE,
        step_kind::MCP_CONFIGURE,
    ] {
        session.apply_state_signal(InitStateSignal::StepStarted { kind });
        session.apply_state_signal(InitStateSignal::StepFinished {
            kind,
            disposition: StepDisposition::Skipped,
            error_code: None,
        });
    }
    let mid_run = latest_state(&session);
    for id in ["agent", "workspace", "provider", "mcp"] {
        assert_eq!(
            category(&mid_run, id)["status"],
            json!("settled"),
            "a skipped step must settle `{id}`"
        );
    }

    session.apply_state_signal(InitStateSignal::StepFinished {
        kind: step_kind::INIT_COMPLETE,
        disposition: StepDisposition::Skipped,
        error_code: None,
    });
    let completed = latest_state(&session);
    for id in CANONICAL_CATEGORY_IDS {
        let status = completed["categories"]
            .as_array()
            .expect("categories")
            .iter()
            .find(|entry| entry["id"] == json!(id))
            .map(|entry| entry["status"].clone())
            .expect("every category is reported");
        assert!(
            status == json!("settled") || status == json!("not_applicable"),
            "category `{id}` still derives as {status} after a resumed run completed"
        );
    }
    assert!(
        awaiting_ids(&completed).is_empty(),
        "a completed resume must await nothing: {completed}"
    );
}

#[test]
fn parking_a_failure_releases_a_blocked_prompt_and_keeps_the_first_code() {
    // The frame-encode path parks through exactly this call: a payload
    // that will not serialize cannot be constructed from production types,
    // so the semantics it depends on are asserted directly.
    let session = test_session("init_state_park");
    let driver = SessionPromptDriver {
        session: session.clone(),
    };
    let request = hosted_test_request(
        HostedPromptKind::Model,
        HostedPromptStyle::Text,
        "model",
        &[],
    );
    let handle = std::thread::spawn(move || driver.text(request));
    wait_for_pending_input(&session);
    session.set_error(
        FRAME_ENCODE_FAILED_CODE,
        FRAME_ENCODE_FAILED_MESSAGE.to_owned(),
    );

    let error = handle
        .join()
        .expect("driver thread")
        .expect_err("a parked session must release the blocked prompt");
    assert!(error.to_string().contains(FRAME_ENCODE_FAILED_CODE));
    assert_eq!(session.status(), "errored");
    assert!(
        session
            .error_replay_frame()
            .expect("replay frame")
            .contains(FRAME_ENCODE_FAILED_CODE)
    );
    assert!(awaiting_ids(&latest_state(&session)).is_empty());

    // The error the wizard propagates afterwards is downstream of the
    // parked one and must not replace it.
    session.set_error("init.invalid_param", "prompt failed".to_owned());
    assert!(
        session
            .error_replay_frame()
            .expect("replay frame")
            .contains(FRAME_ENCODE_FAILED_CODE)
    );
}
