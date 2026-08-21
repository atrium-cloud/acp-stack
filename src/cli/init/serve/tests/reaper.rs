//! Reaper and lifetime tests: idle expiry, max lifetime, websocket teardown,
//! and what an expiry clears.

use super::super::*;
use super::support::*;

use http::Method;
use serde_json::json;

#[test]
fn parse_optional_duration_accepts_suffixes_and_zero_disables() {
    assert_eq!(
        parse_optional_duration("15m", "idle timeout").expect("15m parses"),
        Some(std::time::Duration::from_secs(900))
    );
    assert_eq!(
        parse_optional_duration("0s", "idle timeout").expect("0s parses"),
        None
    );
    assert!(parse_optional_duration("banana", "idle timeout").is_err());
}

fn reaper_test_manager(session_id: &str) -> (Arc<HostedInitManager>, Arc<HostedInitSession>) {
    let manager = HostedInitManager::new();
    let session = HostedInitSession::new(session_id.to_owned(), manager.shutdown.clone(), false);
    *lock_unpoisoned(&manager.active) = Some(session.clone());
    (manager, session)
}

#[tokio::test(start_paused = true)]
async fn idle_reaper_expires_abandoned_session() {
    let (manager, session) = reaper_test_manager("init_idle_reap");
    tokio::spawn(reap_idle_session(
        manager.clone(),
        Some(std::time::Duration::from_secs(10)),
    ));
    tokio::time::timeout(
        std::time::Duration::from_secs(60),
        manager.wait_for_terminal(),
    )
    .await
    .expect("idle reaper should shut the server down");
    assert_eq!(session.status(), "cancelled");
}

#[tokio::test(start_paused = true)]
async fn idle_reaper_skips_sessions_with_connected_websocket() {
    let (manager, session) = reaper_test_manager("init_idle_ws");
    session.ws_connected();
    tokio::spawn(reap_idle_session(
        manager.clone(),
        Some(std::time::Duration::from_secs(10)),
    ));
    // A listen-only backend holds the socket past the timeout; the
    // session must survive.
    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    assert_eq!(session.status(), "running");
    session.ws_disconnected();
    // Disconnect restarts the idle clock so a dropped backend gets the
    // full timeout to reconnect and ack before the reaper fires.
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    assert_eq!(session.status(), "running");
    tokio::time::timeout(
        std::time::Duration::from_secs(60),
        manager.wait_for_terminal(),
    )
    .await
    .expect("reaper should fire once the reconnect grace lapses");
    assert_eq!(session.status(), "cancelled");
}

#[tokio::test(start_paused = true)]
async fn idle_reaper_respects_route_lookup_activity() {
    let (manager, session) = reaper_test_manager("init_idle_poll");
    let app = app_with_manager(manager.clone());
    tokio::spawn(reap_idle_session(
        manager.clone(),
        Some(std::time::Duration::from_secs(10)),
    ));
    tokio::time::sleep(std::time::Duration::from_secs(8)).await;
    // Polling the status endpoint is API activity; it is what keeps a
    // REST-polling backend's session alive.
    let (status, _) = request_json(
        app,
        Method::GET,
        "/v1/init/sessions/init_idle_poll",
        None,
        Some(TEST_TOKEN),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    tokio::time::sleep(std::time::Duration::from_secs(8)).await;
    assert_eq!(session.status(), "running");
    tokio::time::timeout(
        std::time::Duration::from_secs(60),
        manager.wait_for_terminal(),
    )
    .await
    .expect("reaper should fire after polling stops");
    assert_eq!(session.status(), "cancelled");
}

#[tokio::test(start_paused = true)]
async fn status_reports_idle_age_before_counting_itself() {
    let (manager, _session) = reaper_test_manager("init_age");
    let app = app_with_manager(manager);
    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    let (status, body) = request_json(
        app.clone(),
        Method::GET,
        "/v1/init/sessions/init_age",
        None,
        Some(TEST_TOKEN),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // The age is the idleness leading up to the poll; the poll itself
    // must not reset the value it reports.
    assert_eq!(body["data"]["last_activity_age_secs"], 30);
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    let (_, body) = request_json(
        app,
        Method::GET,
        "/v1/init/sessions/init_age",
        None,
        Some(TEST_TOKEN),
    )
    .await;
    assert_eq!(body["data"]["last_activity_age_secs"], 5);
}

#[tokio::test(start_paused = true)]
async fn idle_reaper_respects_pre_session_api_activity() {
    let manager = HostedInitManager::new();
    let app = app_with_manager(manager.clone());
    tokio::spawn(reap_idle_session(
        manager.clone(),
        Some(std::time::Duration::from_secs(10)),
    ));
    tokio::time::sleep(std::time::Duration::from_secs(8)).await;
    // Even a 404 poll for a not-yet-created session is authenticated API
    // activity and restarts the pre-session idle clock.
    let (status, _) = request_json(
        app,
        Method::GET,
        "/v1/init/sessions/init_unknown",
        None,
        Some(TEST_TOKEN),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    tokio::time::sleep(std::time::Duration::from_secs(8)).await;
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            manager.wait_for_terminal()
        )
        .await
        .is_err(),
        "active backend polling must hold off the pre-session idle shutdown"
    );
    tokio::time::timeout(
        std::time::Duration::from_secs(60),
        manager.wait_for_terminal(),
    )
    .await
    .expect("server should idle out once polling stops");
    let error = manager
        .terminal_result()
        .expect_err("pre-session idle-out must exit non-zero");
    assert!(error.public_message().contains("idle_timeout"));
}

#[tokio::test]
async fn shutdown_if_no_session_is_atomic_with_session_creation() {
    let manager = HostedInitManager::new();
    let session = HostedInitSession::new("init_atomic".to_owned(), manager.shutdown.clone(), false);
    *lock_unpoisoned(&manager.active) = Some(session);
    assert!(!manager.shutdown_if_no_session("idle_timeout"));
    assert!(manager.terminal_result().is_ok());

    let empty = HostedInitManager::new();
    assert!(empty.shutdown_if_no_session("idle_timeout"));
    tokio::time::timeout(std::time::Duration::from_secs(1), empty.wait_for_terminal())
        .await
        .expect("shutdown should have fired");
    assert!(empty.terminal_result().is_err());
}

#[tokio::test]
async fn websocket_closes_when_session_turns_terminal() {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let (manager, session) = reaper_test_manager("init_ws_terminal");
    let app = app_with_manager(manager);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });

    let url = format!("ws://{addr}/v1/init/sessions/init_ws_terminal/ws");
    let mut request = url.as_str().into_client_request().expect("ws request");
    request.headers_mut().insert(
        http::header::AUTHORIZATION,
        format!("Bearer {TEST_TOKEN}").parse().expect("auth header"),
    );
    let (mut stream, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("ws connect");
    let hello = stream.next().await.expect("hello frame").expect("hello ok");
    assert!(hello.is_text());
    let hello: Value =
        serde_json::from_str(hello.to_text().expect("hello text")).expect("hello json");
    // A fresh session has emitted no signals; the client folds the empty replay
    // to the starting view.
    assert_eq!(hello["signals"], json!([]));

    // Signals reach a real socket, not just the history.
    session.apply_state_signal(InitStateSignal::CategorySettled {
        category: InitCategory::Agent,
        value: Some("opencode".to_owned()),
    });
    let signal = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
        .await
        .expect("timed out waiting for the signal frame")
        .expect("stream ended before the signal frame")
        .expect("frame");
    let signal: Value =
        serde_json::from_str(signal.to_text().expect("signal text")).expect("signal json");
    assert_eq!(signal["type"], json!("signal"));
    assert_eq!(signal["signal"], json!("category_settled"));
    assert_eq!(signal["category"], json!("agent"));
    assert_eq!(signal["value"], json!("opencode"));

    // A reaper expiry while a client holds the socket must end the
    // connection server-side; waiting on the client would let a hung
    // backend pin the process past --max-lifetime.
    session.expire("max_lifetime");

    let mut saw_canceled = false;
    loop {
        let message = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
            .await
            .expect("timed out waiting for server-side close")
            .expect("stream ended before a close frame")
            .expect("frame");
        if let tokio_tungstenite::tungstenite::Message::Text(text) = &message {
            assert!(text.contains("cancelled"));
            saw_canceled = true;
        } else if message.is_close() {
            break;
        }
    }
    assert!(saw_canceled);
}

#[tokio::test(start_paused = true)]
async fn idle_reaper_respects_activity_touch() {
    let (manager, session) = reaper_test_manager("init_idle_touch");
    tokio::spawn(reap_idle_session(
        manager.clone(),
        Some(std::time::Duration::from_secs(10)),
    ));
    tokio::time::sleep(std::time::Duration::from_secs(8)).await;
    session.touch();
    tokio::time::sleep(std::time::Duration::from_secs(8)).await;
    assert_eq!(session.status(), "running");
    tokio::time::timeout(
        std::time::Duration::from_secs(60),
        manager.wait_for_terminal(),
    )
    .await
    .expect("reaper should fire after activity stops");
    assert_eq!(session.status(), "cancelled");
}

#[tokio::test(start_paused = true)]
async fn idle_reaper_expires_server_without_session() {
    let manager = HostedInitManager::new();
    tokio::spawn(reap_idle_session(
        manager.clone(),
        Some(std::time::Duration::from_secs(10)),
    ));
    tokio::time::timeout(
        std::time::Duration::from_secs(60),
        manager.wait_for_terminal(),
    )
    .await
    .expect("server with no session should idle out");
    let error = manager
        .terminal_result()
        .expect_err("no-session idle-out must exit non-zero");
    assert!(error.public_message().contains("idle_timeout"));
}

#[tokio::test(start_paused = true)]
async fn max_lifetime_enforcer_expires_active_session() {
    let (manager, session) = reaper_test_manager("init_max_lifetime");
    tokio::spawn(enforce_max_lifetime(
        manager.clone(),
        std::time::Duration::from_secs(5),
    ));
    tokio::time::timeout(
        std::time::Duration::from_secs(60),
        manager.wait_for_terminal(),
    )
    .await
    .expect("max lifetime should shut the server down");
    assert_eq!(session.status(), "cancelled");
}

#[tokio::test(start_paused = true)]
async fn max_lifetime_enforcer_shuts_down_server_without_session() {
    let manager = HostedInitManager::new();
    tokio::spawn(enforce_max_lifetime(
        manager.clone(),
        std::time::Duration::from_secs(5),
    ));
    tokio::time::timeout(
        std::time::Duration::from_secs(60),
        manager.wait_for_terminal(),
    )
    .await
    .expect("max lifetime should shut down a server with no session");
    let error = manager
        .terminal_result()
        .expect_err("no-session max-lifetime shutdown must exit non-zero");
    assert!(error.public_message().contains("max_lifetime"));
}

#[test]
fn expire_clears_unacked_result_and_secrets() {
    let session = test_session("init_expire");
    session.set_result(json!({
        "status": "initialized",
        "session_key": "acps_session_expire_secret",
        "admin_key": "acps_admin_expire_secret"
    }));
    assert_eq!(session.status(), "completed_awaiting_ack");
    // Backend-driven cancel must not kill a session holding an un-acked
    // result; only the internal reaper may.
    session.cancel("backend_cancel");
    assert_eq!(session.status(), "completed_awaiting_ack");
    session.expire("idle_timeout");
    assert_eq!(session.status(), "cancelled");
    assert!(session.result_frame().is_none());
    let snapshot = serde_json::to_string(&session.status_snapshot()).expect("snapshot");
    assert!(snapshot.contains("last_activity_age_secs"));
    assert!(!snapshot.contains("acps_session_expire_secret"));
    let events = serde_json::to_string(&session.events_after(0)).expect("events");
    assert!(events.contains("idle_timeout"));
    assert!(!events.contains("acps_session_expire_secret"));
    // A second expiry is a no-op on an already terminal session.
    session.expire("max_lifetime");
    assert_eq!(session.status(), "cancelled");
}
