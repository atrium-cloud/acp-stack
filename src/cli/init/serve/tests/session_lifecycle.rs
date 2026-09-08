//! Session lifecycle tests: input staleness, result replay/ack, cancel, and the
//! parked-error grace.

use super::super::*;
use super::support::*;

use http::Method;
use serde_json::json;
use std::time::Duration;

#[test]
fn stale_input_request_id_is_rejected() {
    let session = test_session("init_stale_input");
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
        config_option: None,
    };
    let handle = std::thread::spawn(move || driver.password(request));
    let pending = wait_for_pending_input(&session);

    let stale_frame = json!({
        "type": "input",
        "request_id": "stale_request",
        "value": "sk-hosted-secret"
    })
    .to_string();
    match handle_client_frame(&session, &stale_frame) {
        ClientFrameOutcome::Send(frame) => {
            let value: Value = serde_json::from_str(&frame).expect("error frame");
            assert_eq!(value["type"], "error");
            assert_eq!(value["code"], "init.input_rejected");
        }
        _ => panic!("stale input should be rejected with an error frame"),
    }

    session
        .submit_input(&pending.request_id, json!("sk-hosted-secret"))
        .expect("submit correct input");
    let password = handle.join().expect("driver thread").expect("password");
    assert_eq!(
        password,
        HostedPromptOutcome::Handled(Some("sk-hosted-secret".to_owned()))
    );
}

#[test]
fn a_discovery_revision_wakes_the_wizard_waiting_on_a_later_prompt() {
    let session = test_session("init_revise_pending");
    let wizard = session.clone();
    let handle = std::thread::spawn(move || {
        let model = wizard.request_input_revisable(revisable_select_request(
            HostedPromptKind::Model,
            &["alpha", "beta"],
        ))?;
        let mode = wizard.request_input_revisable(revisable_select_request(
            HostedPromptKind::Mode,
            &["fast", "deep"],
        ))?;
        Ok::<_, StackError>((model, mode))
    });

    let model_prompt = answer_pending(&session, "model", json!({"value": "id_alpha"}));
    wait_for_pending_kind(&session, "mode");
    session
        .submit_input(&model_prompt.request_id, json!({"value": "id_beta"}))
        .expect("model revision");

    let (model, mode) = handle
        .join()
        .expect("wizard thread")
        .expect("wizard result");
    assert!(matches!(model, Some(HostedInput::Answer(_))));
    let Some(HostedInput::Revision(revision)) = mode else {
        panic!("the pending mode prompt should be abandoned for the model revision");
    };
    assert_eq!(revision.request_id, model_prompt.request_id);
    assert_eq!(revision.kind.as_str(), "model");
    // The wire form is resolved to the offered option's stable id before the
    // wizard ever sees it.
    assert_eq!(
        revision.answer,
        RevisedAnswer::Select(Some("id_beta".to_owned()))
    );
    assert!(
        lock_unpoisoned(&session.inner).pending_input.is_none(),
        "the abandoned prompt must not stay pending"
    );
}

#[test]
fn a_discovery_revision_wakes_the_wizard_awaiting_close() {
    let session = test_session("init_revise_awaiting_close");
    let handle = spawn_discovery_wizard(
        session.clone(),
        vec![revisable_select_request(
            HostedPromptKind::Model,
            &["alpha", "beta"],
        )],
    );
    let model = answer_pending(&session, "model", json!(0));
    wait_for_status(&session, "awaiting_discovery_close");
    session
        .submit_input(&model.request_id, json!(1))
        .expect("model revision");

    let revisions = close_and_join(&session, handle);
    assert_eq!(revisions.len(), 1);
    assert_eq!(revisions[0].request_id, model.request_id);
    assert_eq!(session.status(), "running");
}

#[test]
fn a_revision_queued_while_the_wizard_is_busy_is_picked_up_next() {
    let session = test_session("init_revise_queued");
    let (answered, wizard_answered) = std::sync::mpsc::channel();
    let (release, wizard_release) = std::sync::mpsc::channel::<()>();
    let wizard = session.clone();
    let handle = std::thread::spawn(move || {
        wizard.request_input_revisable(revisable_select_request(
            HostedPromptKind::Model,
            &["alpha", "beta"],
        ))?;
        answered.send(()).expect("signal the answer landed");
        wizard_release.recv().expect("release the wizard");
        wizard.request_input_revisable(revisable_select_request(
            HostedPromptKind::Mode,
            &["fast", "deep"],
        ))
    });

    let model = answer_pending(&session, "model", json!(0));
    wizard_answered.recv().expect("wizard answered");
    session
        .submit_input(&model.request_id, json!(1))
        .expect("queued revision");
    release.send(()).expect("release the wizard");

    let outcome = handle
        .join()
        .expect("wizard thread")
        .expect("wizard result");
    let Some(HostedInput::Revision(revision)) = outcome else {
        panic!("the next revisable prompt should pick the queued revision up");
    };
    assert_eq!(revision.request_id, model.request_id);
}

#[test]
fn a_second_revision_of_one_lane_replaces_the_first_in_the_queue() {
    let session = test_session("init_revise_replace");
    let (answered, wizard_answered) = std::sync::mpsc::channel();
    let (release, wizard_release) = std::sync::mpsc::channel::<()>();
    let wizard = session.clone();
    // Gated between calls, so both revisions are in the queue before either can
    // be picked up.
    let handle = std::thread::spawn(move || {
        wizard.request_input_revisable(revisable_select_request(
            HostedPromptKind::Model,
            &["alpha", "beta", "gamma"],
        ))?;
        answered.send(()).expect("signal the answer landed");
        wizard_release.recv().expect("release the wizard");
        let mut revisions = Vec::new();
        while let DiscoveryWait::Revised(revision) = wizard.await_discovery_close()? {
            revisions.push(revision);
        }
        Ok::<_, StackError>(revisions)
    });

    let model = answer_pending(&session, "model", json!(0));
    wizard_answered.recv().expect("wizard answered");
    session
        .submit_input(&model.request_id, json!(1))
        .expect("first revision");
    session
        .submit_input(&model.request_id, json!(2))
        .expect("second revision");
    release.send(()).expect("release the wizard");

    close_discovery_when_ready(&session);
    let revisions = handle
        .join()
        .expect("wizard thread")
        .expect("wizard result");
    assert_eq!(revisions.len(), 1, "one lane holds at most one revision");
    assert_eq!(revisions[0].request_id, model.request_id);
    assert_eq!(
        revisions[0].answer,
        RevisedAnswer::Select(Some("id_gamma".to_owned())),
        "the latest revision of a lane wins"
    );
}

#[test]
fn revisions_of_two_lanes_are_both_delivered_in_arrival_order() {
    let session = test_session("init_revise_two_lanes");
    let (answered, wizard_answered) = std::sync::mpsc::channel();
    let (release, wizard_release) = std::sync::mpsc::channel::<()>();
    let wizard = session.clone();
    let handle = std::thread::spawn(move || {
        wizard.request_input_revisable(revisable_select_request(
            HostedPromptKind::Model,
            &["alpha", "beta"],
        ))?;
        wizard.request_input_revisable(revisable_select_request(
            HostedPromptKind::Mode,
            &["fast", "deep"],
        ))?;
        answered.send(()).expect("signal both answers landed");
        wizard_release.recv().expect("release the wizard");
        let mut revisions = Vec::new();
        while let DiscoveryWait::Revised(revision) = wizard.await_discovery_close()? {
            revisions.push(revision);
        }
        Ok::<_, StackError>(revisions)
    });

    let model = answer_pending(&session, "model", json!(0));
    let mode = answer_pending(&session, "mode", json!(0));
    wizard_answered.recv().expect("wizard answered both");
    session
        .submit_input(&mode.request_id, json!(1))
        .expect("mode revision");
    session
        .submit_input(&model.request_id, json!(1))
        .expect("model revision");
    release.send(()).expect("release the wizard");

    close_discovery_when_ready(&session);
    let revisions = handle
        .join()
        .expect("wizard thread")
        .expect("wizard result");
    assert_eq!(
        revisions
            .iter()
            .map(|revision| revision.request_id.clone())
            .collect::<Vec<_>>(),
        vec![mode.request_id, model.request_id],
        "cross-lane revisions drain oldest first"
    );
}

#[test]
fn two_config_option_revisions_do_not_clobber_each_other() {
    let session = test_session("init_revise_config_options");
    let handle = spawn_discovery_wizard(
        session.clone(),
        vec![
            config_option_test_request(config_option_snapshot("first")),
            config_option_test_request(config_option_snapshot("second")),
        ],
    );
    let first = answer_pending(
        &session,
        "config_option",
        json!({"config_id": "first", "value": "balanced"}),
    );
    let second = answer_pending(
        &session,
        "config_option",
        json!({"config_id": "second", "value": "balanced"}),
    );
    wait_for_status(&session, "awaiting_discovery_close");
    session
        .submit_input(
            &first.request_id,
            json!({"config_id": "first", "value": "research"}),
        )
        .expect("first config-option revision");
    session
        .submit_input(
            &second.request_id,
            json!({"config_id": "second", "value": "research"}),
        )
        .expect("second config-option revision");

    let revisions = close_and_join(&session, handle);
    assert_eq!(
        revisions
            .iter()
            .map(|revision| revision.config_id.clone())
            .collect::<Vec<_>>(),
        vec![Some("first".to_owned()), Some("second".to_owned())],
        "config options are one lane each, not one lane per kind"
    );
}

#[test]
fn superseded_lane_ids_are_rejected_as_stale_input() {
    let session = test_session("init_revise_supersede");
    let handle = spawn_discovery_wizard(
        session.clone(),
        vec![
            revisable_select_request(HostedPromptKind::Model, &["alpha", "beta"]),
            revisable_select_request(HostedPromptKind::Mode, &["fast", "deep"]),
        ],
    );
    let model = answer_pending(&session, "model", json!(0));
    let mode = answer_pending(&session, "mode", json!(0));
    wait_for_status(&session, "awaiting_discovery_close");

    session.supersede_discovery_lanes(&[HostedPromptKind::Mode]);
    let rejection = session
        .submit_input(&mode.request_id, json!(1))
        .expect_err("a superseded id names nothing");
    assert!(matches!(rejection, AnswerRejected::Input(_)));
    // The lane that was not superseded still revises.
    session
        .submit_input(&model.request_id, json!(1))
        .expect("model revision");

    let revisions = close_and_join(&session, handle);
    assert_eq!(revisions.len(), 1);
    assert_eq!(revisions[0].request_id, model.request_id);
}

#[test]
fn superseding_a_lane_drops_its_queued_revision() {
    let session = test_session("init_revise_supersede_queue");
    let (answered, wizard_answered) = std::sync::mpsc::channel();
    let (release, wizard_release) = std::sync::mpsc::channel::<()>();
    let wizard = session.clone();
    let handle = std::thread::spawn(move || {
        wizard.request_input_revisable(revisable_select_request(
            HostedPromptKind::Mode,
            &["fast", "deep"],
        ))?;
        answered.send(()).expect("signal the answer landed");
        wizard_release.recv().expect("release the wizard");
        let mut revisions = Vec::new();
        while let DiscoveryWait::Revised(revision) = wizard.await_discovery_close()? {
            revisions.push(revision);
        }
        Ok::<_, StackError>(revisions)
    });

    let mode = answer_pending(&session, "mode", json!(0));
    wizard_answered.recv().expect("wizard answered");
    session
        .submit_input(&mode.request_id, json!(1))
        .expect("mode revision");
    // The model change that invalidated the mode options arrives next.
    session.supersede_discovery_lanes(&[HostedPromptKind::Mode]);
    release.send(()).expect("release the wizard");

    close_discovery_when_ready(&session);
    let revisions = handle
        .join()
        .expect("wizard thread")
        .expect("wizard result");
    assert!(
        revisions.is_empty(),
        "a queued revision must not survive the supersede that invalidated it"
    );
}

#[test]
fn a_revision_naming_a_value_the_prompt_never_offered_is_revision_rejected() {
    let session = test_session("init_revise_bad_value");
    let handle = spawn_discovery_wizard(
        session.clone(),
        vec![revisable_select_request(
            HostedPromptKind::Model,
            &["alpha", "beta"],
        )],
    );
    let model = answer_pending(&session, "model", json!(0));
    wait_for_status(&session, "awaiting_discovery_close");

    let rejection = session
        .submit_input(&model.request_id, json!({"value": "id_absent"}))
        .expect_err("a value that prompt never offered");
    match rejection {
        AnswerRejected::Revision(message) => {
            assert!(message.contains("id_absent"), "message was `{message}`");
        }
        AnswerRejected::Input(message) => panic!("expected a revision refusal, got `{message}`"),
    }
    // The accepted answer stands, so the lane stays addressable.
    let snapshot = session.status_snapshot();
    let discovery = snapshot.discovery.expect("the phase is open");
    assert_eq!(discovery.revisable.len(), 1);
    assert_eq!(discovery.revisable[0].request_id, model.request_id);

    let revisions = close_and_join(&session, handle);
    assert!(revisions.is_empty());
}

#[test]
fn a_revision_after_the_phase_closes_is_revision_rejected() {
    let session = test_session("init_revise_tombstone");
    let handle = spawn_discovery_wizard(
        session.clone(),
        vec![revisable_select_request(
            HostedPromptKind::Model,
            &["alpha", "beta"],
        )],
    );
    let model = answer_pending(&session, "model", json!(0));
    close_and_join(&session, handle);

    let rejection = session
        .submit_input(&model.request_id, json!(1))
        .expect_err("the phase is closed");
    match rejection {
        AnswerRejected::Revision(message) => assert!(message.contains("model")),
        AnswerRejected::Input(message) => panic!("expected a revision refusal, got `{message}`"),
    }
    assert!(
        session.status_snapshot().discovery.is_none(),
        "a closed phase is not advertised"
    );
}

/// Park a wizard on the close wait, accept a close, and immediately try to
/// revise. The wizard may or may not have woken yet, which is exactly the window
/// the close latch has to cover.
fn revise_after_an_accepted_close(
    label: &str,
) -> (AnswerRejected, Vec<DiscoveryRevision>, Option<String>) {
    let session = test_session(label);
    let handle = spawn_discovery_wizard(
        session.clone(),
        vec![revisable_select_request(
            HostedPromptKind::Model,
            &["alpha", "beta"],
        )],
    );
    let model = answer_pending(&session, "model", json!(0));
    wait_for_status(&session, "awaiting_discovery_close");
    session.close_discovery().expect("close the phase");
    let rejection = session
        .submit_input(&model.request_id, json!(1))
        .expect_err("a close already settled this lane");
    let revisions = handle
        .join()
        .expect("wizard thread")
        .expect("wizard result");
    let state = session
        .status_snapshot()
        .discovery
        .map(|phase| phase.state.clone());
    (rejection, revisions, state)
}

#[test]
fn a_revision_after_close_was_accepted_is_revision_rejected() {
    // Repeated because the wizard's wake races the revision; both sides of that
    // race must refuse, and neither may accept.
    for attempt in 0..6 {
        let (rejection, _, _) =
            revise_after_an_accepted_close(&format!("init_revise_after_close_{attempt}"));
        match rejection {
            AnswerRejected::Revision(message) => assert!(
                message == "the discovery phase is closing; the recorded answer for `model` stands"
                    || message
                        == "the discovery phase is closed; the recorded answer for `model` stands",
                "message was `{message}`"
            ),
            AnswerRejected::Input(message) => {
                panic!("expected a revision refusal, got `{message}`")
            }
        }
    }
}

#[test]
fn a_closing_phase_is_never_reopened_by_a_drain() {
    // The latch is set only with an empty queue and refuses every later
    // revision, so no drain can hand the wizard one and walk the phase back to
    // `open` without a second close.
    for attempt in 0..6 {
        let (_, revisions, state) =
            revise_after_an_accepted_close(&format!("init_close_latch_{attempt}"));
        assert!(
            revisions.is_empty(),
            "a closing phase must not drain a revision: {revisions:?}"
        );
        assert_eq!(state, None, "the phase must end closed, not reopened");
    }
}

#[test]
fn a_second_close_before_the_wizard_wakes_is_refused() {
    // Repeated because the second close races the wizard's wake: latched or
    // already closed, both are the same no-longer-open refusal, and only one
    // close ever takes effect.
    for attempt in 0..6 {
        let session = test_session(&format!("init_double_close_{attempt}"));
        let handle = spawn_discovery_wizard(
            session.clone(),
            vec![revisable_select_request(
                HostedPromptKind::Model,
                &["alpha", "beta"],
            )],
        );
        answer_pending(&session, "model", json!(0));
        wait_for_status(&session, "awaiting_discovery_close");
        session
            .close_discovery()
            .expect("the first close is accepted");

        match session.close_discovery() {
            Err(CloseRejected::NotOpen(message)) => assert!(
                message == "the discovery phase is already closing"
                    || message == "the discovery phase is already closed",
                "message was `{message}`"
            ),
            other => panic!("a second close must be refused: {other:?}"),
        }

        let revisions = handle
            .join()
            .expect("wizard thread")
            .expect("wizard result");
        assert!(revisions.is_empty());
        assert!(
            session.status_snapshot().discovery.is_none(),
            "the phase closed once and stayed closed"
        );
        assert_eq!(session.status(), "running");
    }
}

#[test]
fn a_revision_with_no_phase_open_is_input_rejected() {
    let session = test_session("init_revise_no_phase");
    let rejection = session
        .submit_input("ireq_unknown", json!(0))
        .expect_err("no phase, no pending prompt");
    match rejection {
        AnswerRejected::Input(message) => assert_eq!(message, "no input request is pending"),
        AnswerRejected::Revision(message) => panic!("expected an input refusal, got `{message}`"),
    }
}

#[test]
fn close_discovery_is_refused_before_the_phase_opens_and_while_a_prompt_pends() {
    let session = test_session("init_close_refusals");
    match session.close_discovery() {
        Err(CloseRejected::NotOpen(message)) => {
            assert_eq!(message, "no discovery phase is open");
        }
        other => panic!("closing before the phase opens must be refused: {other:?}"),
    }

    let handle = spawn_discovery_wizard(
        session.clone(),
        vec![
            revisable_select_request(HostedPromptKind::Model, &["alpha", "beta"]),
            revisable_select_request(HostedPromptKind::Mode, &["fast", "deep"]),
        ],
    );
    let model = answer_pending(&session, "model", json!(0));
    wait_for_pending_kind(&session, "mode");
    match session.close_discovery() {
        Err(CloseRejected::Busy(message)) => assert!(message.contains("still in progress")),
        other => panic!("closing while a prompt pends must be refused: {other:?}"),
    }

    session
        .submit_input(&model.request_id, json!(1))
        .expect("model revision releases the pending mode prompt");
    close_and_join(&session, handle);
    match session.close_discovery() {
        Err(CloseRejected::NotOpen(message)) => {
            assert_eq!(message, "the discovery phase is already closed");
        }
        other => panic!("closing twice must be refused: {other:?}"),
    }
}

#[test]
fn awaiting_discovery_close_keeps_the_session_active() {
    let session = test_session("init_close_active");
    let handle = spawn_discovery_wizard(
        session.clone(),
        vec![revisable_select_request(
            HostedPromptKind::Model,
            &["alpha", "beta"],
        )],
    );
    answer_pending(&session, "model", json!(0));
    wait_for_status(&session, "awaiting_discovery_close");
    assert!(session.is_active(), "the parked wait is not terminal");
    session.push_event(ServerEvent::Progress {
        message: "still live".to_owned(),
    });
    assert!(
        session
            .events_after(0)
            .iter()
            .any(|event| event["message"] == json!("still live")),
        "events must still flow while the phase is parked"
    );
    close_and_join(&session, handle);
}

#[test]
fn cancel_clears_the_discovery_phase_and_releases_the_close_wait() {
    let session = test_session("init_close_cancel");
    let handle = spawn_discovery_wizard(
        session.clone(),
        vec![revisable_select_request(
            HostedPromptKind::Model,
            &["alpha", "beta"],
        )],
    );
    let model = answer_pending(&session, "model", json!(0));
    wait_for_status(&session, "awaiting_discovery_close");

    session.cancel("backend_cancel");
    handle
        .join()
        .expect("wizard thread")
        .expect_err("a cancelled session releases the close wait as a failure");
    assert!(session.status_snapshot().discovery.is_none());
    assert!(matches!(
        session.submit_input(&model.request_id, json!(1)),
        Err(AnswerRejected::Input(_))
    ));
}

#[test]
fn a_result_clears_the_open_discovery_phase() {
    let session = test_session("init_close_result");
    let wizard = session.clone();
    // A wizard that answers and returns, leaving the phase open with nothing
    // parked on it.
    let handle = std::thread::spawn(move || {
        wizard.request_input_revisable(revisable_select_request(
            HostedPromptKind::Model,
            &["alpha", "beta"],
        ))
    });
    let model = answer_pending(&session, "model", json!(0));
    handle.join().expect("wizard thread").expect("answer");
    assert!(session.status_snapshot().discovery.is_some());

    session.set_result(json!({"status": "initialized"}));
    assert!(
        session.status_snapshot().discovery.is_none(),
        "a terminal session must not advertise a revisable prompt"
    );
    assert!(matches!(
        session.submit_input(&model.request_id, json!(1)),
        Err(AnswerRejected::Input(_))
    ));
}

#[test]
fn result_is_replay_only_and_ack_is_terminal() {
    let session = test_session("init_result");
    session.set_result(json!({
        "status": "initialized",
        "session_key": "acps_session_secret",
        "admin_key": "acps_admin_secret"
    }));

    let snapshot = serde_json::to_string(&session.status_snapshot()).expect("snapshot");
    assert!(snapshot.contains("completed_awaiting_ack"));
    assert!(!snapshot.contains("acps_session_secret"));
    assert!(!snapshot.contains("acps_admin_secret"));

    let replay = match handle_client_frame(&session, r#"{"type":"replay_result"}"#) {
        ClientFrameOutcome::Send(frame) => frame,
        _ => panic!("replay_result should return a result frame"),
    };
    assert!(replay.contains("acps_session_secret"));
    assert!(replay.contains("acps_admin_secret"));

    match handle_client_frame(&session, r#"{"type":"ack_result"}"#) {
        ClientFrameOutcome::Close(frame) => {
            let value: Value = serde_json::from_str(&frame).expect("ack frame");
            assert_eq!(value["type"], "ack_accepted");
        }
        _ => panic!("ack_result should close the session"),
    }

    assert_eq!(session.status(), "closed");
    assert!(session.result_frame().is_none());
    assert!(!session.is_active());
}

#[test]
fn cancel_prevents_late_result_publication() {
    let session = test_session("init_cancel");
    session.cancel("backend_cancel");
    session.set_result(json!({
        "status": "initialized",
        "session_key": "acps_session_after_cancel",
        "admin_key": "acps_admin_after_cancel"
    }));
    session.set_error("init.failed", "should not replace cancel".to_owned());

    assert_eq!(session.status(), "cancelled");
    assert!(session.result_frame().is_none());
    let snapshot = serde_json::to_string(&session.status_snapshot()).expect("snapshot");
    assert!(!snapshot.contains("acps_session_after_cancel"));
    assert!(!snapshot.contains("should not replace cancel"));
}

#[tokio::test]
async fn error_is_parked_until_acked() {
    let manager = HostedInitManager::new(test_shared_secret_store().0);
    let session = HostedInitSession::new("init_error".to_owned(), manager.shutdown.clone(), false);
    *lock_unpoisoned(&manager.active) = Some(session.clone());

    {
        let waiter = manager.wait_for_terminal();
        tokio::pin!(waiter);
        session.set_error("init.failed", "provider setup failed".to_owned());

        // The failure parks so the backend can replay and ack the error.
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut waiter)
                .await
                .is_err(),
            "set_error must not notify the terminal waiter"
        );
        assert_eq!(session.status(), "errored");
        assert!(session.is_active());
        assert!(session.unacked_error_age().is_some());

        // A racing backend cancel must not overwrite the typed failure.
        session.cancel("backend_cancel");
        assert_eq!(session.status(), "errored");

        let replay = match handle_client_frame(&session, r#"{"type":"replay_error"}"#) {
            ClientFrameOutcome::Send(frame) => frame,
            _ => panic!("replay_error should return an error frame"),
        };
        let value: Value = serde_json::from_str(&replay).expect("error frame");
        assert_eq!(value["type"], "error");
        assert_eq!(value["code"], "init.failed");
        assert_eq!(value["message"], "provider setup failed");

        match handle_client_frame(&session, r#"{"type":"ack_error"}"#) {
            ClientFrameOutcome::Close(frame) => {
                let value: Value = serde_json::from_str(&frame).expect("ack frame");
                assert_eq!(value["type"], "error_acked");
            }
            _ => panic!("ack_error should close the session"),
        }
        tokio::time::timeout(Duration::from_secs(1), &mut waiter)
            .await
            .expect("terminal waiter should be notified after ack_error");
    }
    assert_eq!(session.status(), "errored");
    assert!(!session.is_active());
    assert!(session.unacked_error_age().is_none());
    let error = manager
        .terminal_result()
        .expect_err("errored session should return failure");
    assert!(
        error
            .public_message()
            .contains("init.failed: provider setup failed")
    );
}

#[tokio::test]
async fn set_result_on_an_errored_session_is_a_no_op() {
    // A late failed handoff must not overwrite a session that already parked
    // `errored`: publishing result_ready after the terminal error frame would
    // flip terminal_result from Err to Ok, exiting zero on a failed bootstrap.
    let manager = HostedInitManager::new(test_shared_secret_store().0);
    let session = HostedInitSession::new(
        "init_errored_guard".to_owned(),
        manager.shutdown.clone(),
        false,
    );
    *lock_unpoisoned(&manager.active) = Some(session.clone());

    session.set_error("init.failed", "provider setup failed".to_owned());
    assert_eq!(session.status(), "errored");

    session.set_result(json!({ "status": "failed" }));

    assert_eq!(
        session.status(),
        "errored",
        "set_result must not overwrite an errored session"
    );
    assert!(
        !session.has_result(),
        "an errored session must not publish a result"
    );
    manager
        .terminal_result()
        .expect_err("an errored session must still report failure after a blocked set_result");
}

#[test]
fn progress_is_frozen_once_the_session_is_terminal() {
    // After a terminal transition, progress must not keep streaming; a line
    // leaking past the terminal frame is what misdirected the hosted-init
    // crash triage.
    let session = test_session("init_progress_freeze");
    session.set_error("init.failed", "boom".to_owned());
    // Subscribe after the terminal transition so only later frames are seen.
    let receiver = session.subscribe();

    session.push_event(ServerEvent::Progress {
        message: "still working".to_owned(),
    });

    assert_eq!(
        receiver.len(),
        0,
        "a terminal session must not broadcast further progress frames",
    );
}

#[tokio::test]
async fn ack_error_is_rejected_without_parked_error() {
    let session = test_session("init_no_error");
    match handle_client_frame(&session, r#"{"type":"ack_error"}"#) {
        ClientFrameOutcome::Send(frame) => {
            let value: Value = serde_json::from_str(&frame).expect("error frame");
            assert_eq!(value["code"], "init.ack_rejected");
        }
        _ => panic!("ack_error without a parked error must be rejected"),
    }
    match handle_client_frame(&session, r#"{"type":"replay_error"}"#) {
        ClientFrameOutcome::Send(frame) => {
            let value: Value = serde_json::from_str(&frame).expect("error frame");
            assert_eq!(value["code"], "init.error_unavailable");
        }
        _ => panic!("replay_error without a recorded error must be rejected"),
    }
}

#[tokio::test]
async fn parked_error_blocks_new_session_and_surfaces_in_status() {
    let session = test_session("init_error_409");
    session.set_error("init.failed", "provider setup failed".to_owned());
    let (app, _store_dir) = app_with_session(session);

    let (status, _) = request_json(
        app.clone(),
        Method::POST,
        "/v1/init/sessions",
        Some(json!({})),
        Some(TEST_TOKEN),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    let (status, body) = request_json(
        app,
        Method::GET,
        "/v1/init/sessions/init_error_409",
        None,
        Some(TEST_TOKEN),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["status"], "errored");
    assert_eq!(body["data"]["error"]["code"], "init.failed");
}

#[tokio::test]
async fn expiring_unacked_error_notifies_shutdown_and_keeps_status() {
    let manager = HostedInitManager::new(test_shared_secret_store().0);
    let session =
        HostedInitSession::new("init_error_exp".to_owned(), manager.shutdown.clone(), false);
    *lock_unpoisoned(&manager.active) = Some(session.clone());
    session.set_error("init.failed", "provider setup failed".to_owned());

    let waiter = manager.wait_for_terminal();
    tokio::pin!(waiter);
    session.expire("error_ack_timeout");
    tokio::time::timeout(Duration::from_secs(1), &mut waiter)
        .await
        .expect("expiring an unacked error must notify shutdown");
    assert_eq!(session.status(), "errored");
    assert!(
        manager.terminal_result().is_err(),
        "expired failure must still exit non-zero"
    );
}

#[tokio::test(start_paused = true)]
async fn errored_session_expires_after_ack_grace_with_connected_ws() {
    let manager = HostedInitManager::new(test_shared_secret_store().0);
    let session =
        HostedInitSession::new("init_error_ws".to_owned(), manager.shutdown.clone(), false);
    *lock_unpoisoned(&manager.active) = Some(session.clone());
    // A held socket must not defer the grace: the check ignores
    // connection state, unlike the idle clock.
    session.ws_connected();
    session.set_error("init.failed", "provider setup failed".to_owned());

    // Idle timeout disabled; only the error-ack grace can fire.
    let reaper = tokio::spawn(reap_idle_session(manager.clone(), None));
    tokio::time::sleep(ERROR_ACK_GRACE + IDLE_REAPER_TICK * 2).await;
    tokio::time::timeout(Duration::from_secs(1), reaper)
        .await
        .expect("reaper should stop after expiring the error")
        .expect("reaper task");
    assert_eq!(session.status(), "errored");
    assert!(!session.is_active());
}
