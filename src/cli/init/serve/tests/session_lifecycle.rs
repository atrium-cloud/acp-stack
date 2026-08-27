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
