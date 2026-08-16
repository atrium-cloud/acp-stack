//! Session state model: the `state` frame, the snapshot embedded in hello and
//! the REST status, and the signal-to-status derivation behind both.

use super::super::*;
use super::support::*;

use crate::runtime::init_runner::StepDisposition;

use http::Method;
use serde_json::json;

#[tokio::test]
async fn state_snapshot_rides_hello_and_rest_status() {
    let session = test_session("init_state_rest");
    let fresh: Value = serde_json::from_str(&session.hello_frame()).expect("hello must be json");
    assert_eq!(category_ids(&fresh["state"]), CANONICAL_CATEGORY_IDS);
    assert_eq!(fresh["state"]["current_step"], Value::Null);

    session.apply_state_signal(InitStateSignal::StepStarted {
        kind: step_kind::AGENT_INSTALL,
    });
    let hello: Value = serde_json::from_str(&session.hello_frame()).expect("hello must be json");
    assert_eq!(hello["state"]["current_step"], json!("agent_install"));

    let app = app_with_session(session.clone());
    let (status, body) = request_json(
        app,
        Method::GET,
        "/v1/init/sessions/init_state_rest",
        None,
        Some(TEST_TOKEN),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // WebSocket and REST clients must not have to reconcile two shapes.
    assert_eq!(body["data"]["state"], hello["state"]);
}

#[test]
fn each_transition_emits_exactly_one_state_frame() {
    let session = test_session("init_state_transitions");
    let driver = SessionPromptDriver {
        session: session.clone(),
    };
    session.apply_state_signal(InitStateSignal::StepStarted {
        kind: step_kind::WORKSPACE_MATERIALIZE,
    });
    driver.progress("materializing workspace".to_owned());
    session.apply_state_signal(InitStateSignal::StepFinished {
        kind: step_kind::WORKSPACE_MATERIALIZE,
        disposition: StepDisposition::Executed,
        error_code: None,
    });
    driver.progress("workspace ready".to_owned());
    session.apply_state_signal(InitStateSignal::CategorySettled {
        category: InitCategory::Agent,
        value: Some("opencode".to_owned()),
    });

    let seqs = session
        .events_after(0)
        .iter()
        .map(|event| event["seq"].as_u64().unwrap_or_default())
        .collect::<Vec<_>>();
    assert!(
        seqs.windows(2).all(|pair| pair[1] > pair[0]),
        "seq must stay strictly monotonic across interleaved frames: {seqs:?}"
    );
    assert_eq!(state_events(&session).len(), 3);
}

#[test]
fn input_required_is_followed_immediately_by_its_state_frame() {
    let session = test_session("init_state_prompt");
    let driver = SessionPromptDriver {
        session: session.clone(),
    };
    let request = hosted_test_request(
        HostedPromptKind::ProviderId,
        HostedPromptStyle::Select,
        "provider",
        &["openrouter"],
    );
    let handle = std::thread::spawn(move || driver.select(request));
    let pending = wait_for_pending_input(&session);

    let events = session.events_after(0);
    let prompt_index = events
        .iter()
        .position(|event| event["type"] == json!("input_required"))
        .expect("input_required must be recorded");
    let announced = &events[prompt_index + 1];
    assert_eq!(announced["type"], json!("state"));
    assert_eq!(
        announced["seq"].as_u64(),
        events[prompt_index]["seq"].as_u64().map(|seq| seq + 1),
        "the state frame must sit directly behind the prompt it describes"
    );
    assert_eq!(awaiting_ids(announced), ["provider"]);

    session
        .submit_input(&pending.request_id, json!(0))
        .expect("submit input");
    handle.join().expect("driver thread").expect("selection");
    // Two transitions, two frames: the prompt going up, and the wizard
    // thread waking to release it. Accepting the answer is not itself a
    // transition — the frontier only moves once the wizard resumes.
    assert_eq!(state_events(&session).len(), 2);
    assert!(awaiting_ids(&latest_state(&session)).is_empty());
}

#[test]
fn at_most_one_category_awaits_input_across_the_whole_surface() {
    let session = test_session("init_state_single_await");
    for (kind, expected) in [
        (HostedPromptKind::ProviderId, "provider"),
        (HostedPromptKind::Model, "model"),
    ] {
        let request = hosted_test_request(kind, HostedPromptStyle::Select, "pick one", &["only"]);
        let driver = SessionPromptDriver {
            session: session.clone(),
        };
        let handle = std::thread::spawn(move || driver.select(request));
        let pending = wait_for_pending_input(&session);
        let hello: Value =
            serde_json::from_str(&session.hello_frame()).expect("hello must be json");
        assert_eq!(awaiting_ids(&hello["state"]), [expected]);
        assert_eq!(
            hello["pending_input"]["kind"],
            json!(kind.as_str()),
            "the awaiting category must be the pending prompt's own"
        );
        session
            .submit_input(&pending.request_id, json!(0))
            .expect("submit input");
        handle.join().expect("driver thread").expect("selection");
    }
    for state in state_events(&session) {
        assert!(
            awaiting_ids(&state).len() <= 1,
            "a frame claimed two awaiting categories: {state}"
        );
    }
}

#[test]
fn secret_answers_never_reach_the_state_surface() {
    const SECRET: &str = "sk-hosted-state-secret";
    let session = test_session("init_state_secret");
    let driver = SessionPromptDriver {
        session: session.clone(),
    };
    let request = HostedPromptRequest {
        kind: HostedPromptKind::ProviderApiKeyValue,
        style: HostedPromptStyle::Password,
        prompt: "OPENROUTER_API_KEY".to_owned(),
        required: true,
        default: None,
        items: Vec::new(),
        inspection: None,
    };
    let handle = std::thread::spawn(move || driver.password(request));
    let pending = wait_for_pending_input(&session);
    session
        .submit_input(&pending.request_id, json!(SECRET))
        .expect("submit password");
    handle.join().expect("driver thread").expect("password");
    // Settlement names the provider that was written, never the answer:
    // the signal is emitted at the config-write site, which carries the
    // provider id it just wrote.
    session.apply_state_signal(InitStateSignal::CategorySettled {
        category: InitCategory::Provider,
        value: Some("openrouter".to_owned()),
    });

    let state = latest_state(&session);
    assert_eq!(category(&state, "provider")["value"], json!("openrouter"));
    let history = serde_json::to_string(&session.events_after(0)).expect("history");
    let hello = session.hello_frame();
    let status = serde_json::to_string(&session.status_snapshot()).expect("status");
    for surface in [&history, &hello, &status] {
        assert!(!surface.contains(SECRET), "secret leaked into {surface}");
    }
    // The prompt named the ref, so history keeps it; the settled snapshot
    // that hello and status carry does not repeat it.
    assert!(history.contains("OPENROUTER_API_KEY"));
    assert!(hello.contains("openrouter"));
    assert!(status.contains("openrouter"));
}

#[test]
fn replay_after_seq_returns_state_frames_in_order() {
    let session = test_session("init_state_replay");
    session.apply_state_signal(InitStateSignal::CategorySettled {
        category: InitCategory::Agent,
        value: Some("opencode".to_owned()),
    });
    let after = session
        .events_after(0)
        .last()
        .and_then(|event| event["seq"].as_u64())
        .expect("a recorded seq");
    session.apply_state_signal(InitStateSignal::CategorySettled {
        category: InitCategory::Provider,
        value: Some("openrouter".to_owned()),
    });
    session.apply_state_signal(InitStateSignal::CategorySettled {
        category: InitCategory::Model,
        value: Some("deepseek-v4-flash".to_owned()),
    });

    let replayed = session.events_after(after);
    let seqs = replayed
        .iter()
        .map(|event| event["seq"].as_u64().unwrap_or_default())
        .collect::<Vec<_>>();
    assert!(seqs.windows(2).all(|pair| pair[1] > pair[0]), "{seqs:?}");
    let settled = replayed
        .iter()
        .filter(|event| event["type"] == json!("state"))
        .map(|event| category(event, "model")["value"].clone())
        .collect::<Vec<_>>();
    assert_eq!(settled, [Value::Null, json!("deepseek-v4-flash")]);
}

#[test]
fn probe_verdict_flips_mcp_and_outranks_the_registry() {
    let session = test_session("init_state_probe");
    session.apply_state_signal(InitStateSignal::CategorySettled {
        category: InitCategory::Agent,
        value: Some("placebo".to_owned()),
    });
    let ready = latest_state(&session);
    assert_eq!(category(&ready, "mcp")["status"], json!("ready"));
    // A lane that is still live explains nothing: `reason` says why a
    // category is hidden, so it rides only with `not_applicable`.
    assert_eq!(category(&ready, "mcp")["reason"], Value::Null);

    session.apply_state_signal(InitStateSignal::CategoryApplicability {
        category: InitCategory::Mcp,
        applicable: false,
        source: ApplicabilitySource::Probe,
        reason: Some("agent does not advertise MCP support".to_owned()),
    });
    let corrected = latest_state(&session);
    assert_eq!(
        category(&corrected, "mcp")["status"],
        json!("not_applicable")
    );
    assert_eq!(
        category(&corrected, "mcp")["reason"],
        json!("agent does not advertise MCP support"),
        "a hidden lane must say what hid it"
    );

    // The installed harness is the authority: a registry claim arriving
    // afterwards must not resurrect the lane.
    let before = corrected["seq"].as_u64();
    session.apply_state_signal(InitStateSignal::CategoryApplicability {
        category: InitCategory::Mcp,
        applicable: true,
        source: ApplicabilitySource::Registry,
        reason: None,
    });
    let latest = latest_state(&session);
    assert_eq!(latest["seq"].as_u64(), before);
    assert_eq!(category(&latest, "mcp")["status"], json!("not_applicable"));
    // The outranked verdict is refused as one write group: had the reason
    // been cleared while the verdict stood, the lane would still hide but
    // could no longer say what hid it.
    assert_eq!(
        category(&latest, "mcp")["reason"],
        json!("agent does not advertise MCP support")
    );
}

// Applicability is a claim about whether init will drive a lane, and it can
// arrive after the lane already ran: the Kimi model pin writes its model
// before `session/new` reports which values the harness advertises, and
// that discovery pass retracts a lane it finds nothing for. A lane that
// wrote a value demonstrably applied, so the retraction is refused rather
// than erasing what landed in config.
#[test]
fn a_late_inapplicable_verdict_cannot_retract_a_settled_lane() {
    let session = test_session("init_state_late_retraction");
    session.apply_state_signal(InitStateSignal::CategorySettled {
        category: InitCategory::Model,
        value: Some("kimi-k2-thinking".to_owned()),
    });
    let before = latest_state(&session)["seq"].as_u64();

    session.apply_state_signal(InitStateSignal::CategoryApplicability {
        category: InitCategory::Model,
        applicable: false,
        source: ApplicabilitySource::Discovery,
        reason: Some("agent advertised no models".to_owned()),
    });

    let latest = latest_state(&session);
    assert_eq!(latest["seq"].as_u64(), before, "nothing observable moved");
    assert_eq!(category(&latest, "model")["status"], json!("settled"));
    assert_eq!(
        category(&latest, "model")["value"],
        json!("kimi-k2-thinking")
    );
    // The refused verdict must leave no trace: a `reason` on a settled lane
    // would tell the client a lane it can see was ruled out.
    assert_eq!(category(&latest, "model")["reason"], Value::Null);
}

// The mirror case: a retraction that arrives before the lane breaks is real
// when it lands, but the failure that follows is the last word.
#[test]
fn a_failure_after_an_inapplicable_verdict_still_displays_failed() {
    let session = test_session("init_state_failure_after_retraction");
    session.apply_state_signal(InitStateSignal::CategoryApplicability {
        category: InitCategory::Mode,
        applicable: false,
        source: ApplicabilitySource::Discovery,
        reason: Some("agent advertised no modes".to_owned()),
    });
    assert_eq!(
        category(&latest_state(&session), "mode")["status"],
        json!("not_applicable")
    );

    session.apply_state_signal(InitStateSignal::CategoryFailed {
        category: InitCategory::Mode,
        code: "init.mode_write_failed".to_owned(),
    });

    let latest = latest_state(&session);
    assert_eq!(category(&latest, "mode")["status"], json!("failed"));
    assert_eq!(
        category(&latest, "mode")["code"],
        json!("init.mode_write_failed")
    );
}

// A step that fails without parking the session badges its lane from the
// `StepFinished` signal alone, and only once: the failure and the step
// ending are one transition.
#[test]
fn a_failed_step_badges_its_category_once_on_a_live_session() {
    let session = test_session("init_state_step_failure");
    session.apply_state_signal(InitStateSignal::StepStarted {
        kind: step_kind::PROVIDER_CONFIGURE,
    });
    let before = state_events(&session).len();

    session.apply_state_signal(InitStateSignal::StepFinished {
        kind: step_kind::PROVIDER_CONFIGURE,
        disposition: StepDisposition::Executed,
        error_code: Some("init.provider_write_failed".to_owned()),
    });

    assert_eq!(state_events(&session).len(), before + 1);
    let latest = latest_state(&session);
    assert_eq!(category(&latest, "provider")["status"], json!("failed"));
    assert_eq!(
        category(&latest, "provider")["code"],
        json!("init.provider_write_failed")
    );
    assert!(session.is_active());
}

// `provider_configure` owns three lanes, and the model and mode lanes badge
// themselves before the error leaves the step. The step then reports the
// same error on its way out, and must not read as a second, provider-shaped
// failure on top of the blame that was already assigned.
#[test]
fn a_step_failure_leaves_a_lane_that_already_took_the_blame_alone() {
    let session = test_session("init_state_step_failure_attributed");
    // The provider lane settles at its own write site, inside the step and
    // ahead of the model lane that goes on to break.
    session.apply_state_signal(InitStateSignal::CategorySettled {
        category: InitCategory::Provider,
        value: Some("openrouter".to_owned()),
    });
    session.apply_state_signal(InitStateSignal::StepStarted {
        kind: step_kind::PROVIDER_CONFIGURE,
    });
    session.apply_state_signal(InitStateSignal::CategoryFailed {
        category: InitCategory::Model,
        code: "init.model_write_failed".to_owned(),
    });

    session.apply_state_signal(InitStateSignal::StepFinished {
        kind: step_kind::PROVIDER_CONFIGURE,
        disposition: StepDisposition::Executed,
        error_code: Some("init.model_write_failed".to_owned()),
    });

    let latest = latest_state(&session);
    assert_eq!(category(&latest, "model")["status"], json!("failed"));
    assert_eq!(
        category(&latest, "provider")["status"],
        json!("settled"),
        "the provider lane finished before the model lane broke"
    );
    assert_eq!(category(&latest, "provider")["value"], json!("openrouter"));
}

// `failed` outranks `not_applicable`, so a step badging a lane this run does
// not have would invent a broken lane. The terminal error frame and
// `current_step` are what carry such a failure.
#[test]
fn a_step_failure_never_badges_a_lane_this_run_does_not_have() {
    let session = test_session("init_state_step_failure_absent_lane");
    session.apply_state_signal(InitStateSignal::CategoryApplicability {
        category: InitCategory::Provider,
        applicable: false,
        source: ApplicabilitySource::Registry,
        reason: Some("agent does not take a provider".to_owned()),
    });
    session.apply_state_signal(InitStateSignal::StepStarted {
        kind: step_kind::PROVIDER_CONFIGURE,
    });

    session.apply_state_signal(InitStateSignal::StepFinished {
        kind: step_kind::PROVIDER_CONFIGURE,
        disposition: StepDisposition::Executed,
        error_code: Some("init.secret_store_unavailable".to_owned()),
    });

    let latest = latest_state(&session);
    assert_eq!(
        category(&latest, "provider")["status"],
        json!("not_applicable")
    );
    assert_eq!(
        category(&latest, "provider")["reason"],
        json!("agent does not take a provider")
    );

    // The same holds for the mode-only lane shape, where the blame was
    // assigned explicitly and the step is echoing it.
    session.apply_state_signal(InitStateSignal::CategoryFailed {
        category: InitCategory::Mode,
        code: "init.mode_write_failed".to_owned(),
    });
    session.apply_state_signal(InitStateSignal::StepFinished {
        kind: step_kind::PROVIDER_CONFIGURE,
        disposition: StepDisposition::Executed,
        error_code: Some("init.mode_write_failed".to_owned()),
    });
    let latest = latest_state(&session);
    assert_eq!(
        category(&latest, "provider")["status"],
        json!("not_applicable")
    );
    assert_eq!(category(&latest, "mode")["status"], json!("failed"));
}

// The other direction: nothing claimed the failure, so the step's own lane
// is the only thing that can carry it — settled or not. The MCP lane settles
// at the probe, well before the write that can still break.
#[test]
fn an_unclaimed_step_failure_badges_its_lane_over_a_settlement() {
    let session = test_session("init_state_step_failure_over_settlement");
    session.apply_state_signal(InitStateSignal::CategorySettled {
        category: InitCategory::Mcp,
        value: Some("linear".to_owned()),
    });
    session.apply_state_signal(InitStateSignal::StepStarted {
        kind: step_kind::MCP_CONFIGURE,
    });

    session.apply_state_signal(InitStateSignal::StepFinished {
        kind: step_kind::MCP_CONFIGURE,
        disposition: StepDisposition::Executed,
        error_code: Some("init.mcp_write_failed".to_owned()),
    });

    let latest = latest_state(&session);
    assert_eq!(category(&latest, "mcp")["status"], json!("failed"));
    assert_eq!(
        category(&latest, "mcp")["code"],
        json!("init.mcp_write_failed")
    );
}

// The mirror of the retraction guard: a settlement read off the config that
// was already on disk is a report, not evidence the lane exists. When the
// installed agent has since dropped the lane, the live discovery pass is
// what knows, and the stale value goes with the withdrawn lane.
#[test]
fn discovery_withdraws_a_settlement_carried_over_from_existing_config() {
    let session = test_session("init_state_provisional_retraction");
    session.apply_state_signal(InitStateSignal::CategoryProvisionallySettled {
        category: InitCategory::Mode,
        value: "smart".to_owned(),
    });
    let carried = latest_state(&session);
    assert_eq!(category(&carried, "mode")["status"], json!("settled"));
    assert_eq!(category(&carried, "mode")["value"], json!("smart"));

    session.apply_state_signal(InitStateSignal::CategoryApplicability {
        category: InitCategory::Mode,
        applicable: false,
        source: ApplicabilitySource::Discovery,
        reason: Some("agent advertised no `mode` values on session/new".to_owned()),
    });

    let latest = latest_state(&session);
    assert_eq!(category(&latest, "mode")["status"], json!("not_applicable"));
    assert_eq!(
        category(&latest, "mode")["reason"],
        json!("agent advertised no `mode` values on session/new")
    );
    assert_eq!(
        category(&latest, "mode")["value"],
        Value::Null,
        "a withdrawn lane must not keep the value it no longer has"
    );
}

// A provisional settlement rests on the config, and a check that never ran
// is no evidence against it. The mode-only discovery lane swallows a harness
// that will not open a provisional session, and that skip must not report a
// mode the config genuinely holds as a lane the agent does not have.
#[test]
fn an_unavailable_discovery_check_withdraws_nothing_the_config_holds() {
    let session = test_session("init_state_discovery_unavailable");
    session.apply_state_signal(InitStateSignal::CategoryProvisionallySettled {
        category: InitCategory::Mode,
        value: "smart".to_owned(),
    });
    let before = latest_state(&session)["seq"].as_u64();

    session.apply_state_signal(InitStateSignal::CategoryApplicability {
        category: InitCategory::Mode,
        applicable: false,
        source: ApplicabilitySource::DiscoveryUnavailable,
        reason: Some("mode discovery skipped: agent exited".to_owned()),
    });

    let latest = latest_state(&session);
    assert_eq!(latest["seq"].as_u64(), before, "nothing observable moved");
    assert_eq!(category(&latest, "mode")["status"], json!("settled"));
    assert_eq!(category(&latest, "mode")["value"], json!("smart"));
    assert_eq!(category(&latest, "mode")["reason"], Value::Null);
}

// With nothing on the lane, the same verdict is the whole story: the run
// will not discover a mode and none is configured, so the lane must read as
// absent with the skip reason rather than staying open forever.
#[test]
fn an_unavailable_discovery_check_still_closes_a_lane_with_no_outcome() {
    let session = test_session("init_state_discovery_unavailable_open_lane");
    session.apply_state_signal(InitStateSignal::CategoryApplicability {
        category: InitCategory::Mode,
        applicable: false,
        source: ApplicabilitySource::DiscoveryUnavailable,
        reason: Some("mode discovery skipped: agent exited".to_owned()),
    });

    let latest = latest_state(&session);
    assert_eq!(category(&latest, "mode")["status"], json!("not_applicable"));
    assert_eq!(
        category(&latest, "mode")["reason"],
        json!("mode discovery skipped: agent exited")
    );

    // And the registry does not get to claim the lane back afterwards: the
    // harness is what failed to produce it.
    session.apply_state_signal(InitStateSignal::CategoryApplicability {
        category: InitCategory::Mode,
        applicable: true,
        source: ApplicabilitySource::Registry,
        reason: None,
    });
    assert_eq!(
        category(&latest_state(&session), "mode")["status"],
        json!("not_applicable")
    );
}

// A lane that is really driven re-settles at its write site, which is what
// makes the carried-over report final: from there it is this run's own
// evidence and no verdict takes it back.
#[test]
fn a_write_site_settlement_promotes_a_carried_over_one() {
    let session = test_session("init_state_provisional_promotion");
    session.apply_state_signal(InitStateSignal::CategoryProvisionallySettled {
        category: InitCategory::Model,
        value: "kimi-k2-thinking".to_owned(),
    });
    session.apply_state_signal(InitStateSignal::CategorySettled {
        category: InitCategory::Model,
        value: Some("kimi-k2-thinking".to_owned()),
    });

    session.apply_state_signal(InitStateSignal::CategoryApplicability {
        category: InitCategory::Model,
        applicable: false,
        source: ApplicabilitySource::Discovery,
        reason: Some("agent advertised no models".to_owned()),
    });

    let latest = latest_state(&session);
    assert_eq!(category(&latest, "model")["status"], json!("settled"));
    assert_eq!(
        category(&latest, "model")["value"],
        json!("kimi-k2-thinking")
    );
    assert_eq!(category(&latest, "model")["reason"], Value::Null);
}

// The terminal sweep means "init finished and nothing is left to drive", so
// a failed final step must leave the lanes it never reached alone rather
// than reporting them as settled with nothing behind them.
#[test]
fn a_failed_init_complete_runs_no_terminal_sweep() {
    let session = test_session("init_state_failed_complete");
    session.apply_state_signal(InitStateSignal::CategorySettled {
        category: InitCategory::Agent,
        value: Some("opencode".to_owned()),
    });

    session.apply_state_signal(InitStateSignal::StepFinished {
        kind: step_kind::INIT_COMPLETE,
        disposition: StepDisposition::Executed,
        error_code: Some("init.finalize_failed".to_owned()),
    });

    let latest = latest_state(&session);
    for id in ["workspace", "native_config", "deps", "mcp", "skills"] {
        assert_eq!(
            category(&latest, id)["status"],
            json!("ready"),
            "`{id}` was never driven, so a failed completion must not settle it"
        );
    }
}

#[test]
fn failure_badges_its_category_before_the_terminal_error_frame() {
    let session = test_session("init_state_failure");
    session.apply_state_signal(InitStateSignal::StepStarted {
        kind: step_kind::MCP_CONFIGURE,
    });
    session.set_error(
        "init.mcp_write_failed",
        "mcp config write failed".to_owned(),
    );

    let events = session.events_after(0);
    let tail = &events[events.len() - 2..];
    assert_eq!(tail[0]["type"], json!("state"));
    assert_eq!(tail[1]["type"], json!("error"));
    assert_eq!(category(&tail[0], "mcp")["status"], json!("failed"));
    assert_eq!(
        category(&tail[0], "mcp")["code"],
        json!("init.mcp_write_failed")
    );
    // A parked failure is still live: the backend has to be able to
    // replay and acknowledge it.
    assert_eq!(session.status(), "errored");
    assert!(session.is_active());
}

#[test]
fn a_failure_between_steps_leaves_the_settled_category_alone() {
    let session = test_session("init_state_between_steps");
    session.apply_state_signal(InitStateSignal::StepStarted {
        kind: step_kind::MCP_CONFIGURE,
    });
    session.apply_state_signal(InitStateSignal::StepFinished {
        kind: step_kind::MCP_CONFIGURE,
        disposition: StepDisposition::Executed,
        error_code: None,
    });
    let settled = state_events(&session).len();
    // `current_step` still names `mcp_configure` for the wire, but the step
    // is over: a failure surfacing between steps belongs to no lane.
    session.set_error(
        "init.config_reload_failed",
        "config reload failed".to_owned(),
    );

    let frontier = latest_state(&session);
    assert_eq!(
        state_events(&session).len(),
        settled,
        "a failure owning no live step must not move the frontier"
    );
    assert_eq!(category(&frontier, "mcp")["status"], json!("settled"));
    for id in CANONICAL_CATEGORY_IDS {
        assert_ne!(
            category(&frontier, id)["status"],
            json!("failed"),
            "category `{id}` was badged by a failure that owned no step"
        );
    }
    assert_eq!(session.status(), "errored");
}
