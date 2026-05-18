use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::sync::atomic::Ordering;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::response::{IntoResponse, Response};
use http::StatusCode;
use serde::Deserialize;

use super::auth::{persist_security_event, reject};
use super::core::AppState;

#[derive(Deserialize)]
struct WsClientMessage {
    #[serde(rename = "type")]
    message_type: String,
    #[serde(default)]
    topics: Vec<String>,
}

pub(super) async fn ws_handler(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: http::HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let request_origin =
        crate::http_hardening::request_origin(&headers, Some(peer.ip()), &state.config);
    // Enforce Origin allowlist on upgrade. Browser clients always send an
    // Origin header; CLI/local clients don't. We honor the allowlist only
    // when an Origin is present, so local tools continue to work. The
    // self-check already warns about wildcard origins on public binds.
    let origin = headers
        .get(http::header::ORIGIN)
        .and_then(|value| value.to_str().ok());
    if !crate::http_hardening::origin_allowed(origin, &state.config.security.http) {
        let origin_text = origin.unwrap_or("").to_owned();
        persist_security_event(
            &state,
            crate::state::EVENT_SOURCE_API,
            "warn",
            "security.ws_origin_denied",
            "rejected ws upgrade with disallowed Origin",
            serde_json::json!({"origin": origin_text, "request_origin": request_origin}),
        )
        .await;
        return reject(
            StatusCode::FORBIDDEN,
            "auth.origin_not_allowed",
            "Origin is not in the configured allowlist",
        );
    }
    let app_state = state.clone();
    ws.on_upgrade(move |socket| ws_connection(socket, app_state, request_origin))
        .into_response()
}

async fn ws_connection(
    mut socket: WebSocket,
    state: AppState,
    origin: crate::http_hardening::RequestOrigin,
) {
    let mut receiver = state.event_hub.subscribe();
    let mut subscribed_topics = BTreeSet::<String>::new();
    let connection_id = next_ws_connection_id();
    let registration = state
        .ws_registry
        .register(connection_id.clone(), origin.clone());
    let started_at = std::time::Instant::now();
    persist_ws_lifecycle_event(
        &state,
        "ws.client_connected",
        serde_json::json!({"connection_id": connection_id, "origin": origin}),
    )
    .await;

    let mut disconnect_reason = "client_disconnect";
    loop {
        tokio::select! {
            () = registration.notify.notified() => {
                if registration.disconnect_requested.load(Ordering::Relaxed) {
                    disconnect_reason = "operator_disconnect";
                    let _ = socket.send(Message::Close(None)).await;
                    break;
                }
            }
            inbound = socket.recv() => {
                let Some(inbound) = inbound else {
                    break;
                };
                let Ok(message) = inbound else {
                    break;
                };
                state.ws_registry.touch(&registration.connection_id);
                match message {
                    Message::Text(text) => {
                        handle_ws_client_message(&mut subscribed_topics, text.as_str()).await;
                        state.ws_registry.update_topics(&registration.connection_id, &subscribed_topics);
                    }
                    Message::Binary(_) | Message::Ping(_) | Message::Pong(_) => {}
                    Message::Close(_) => break,
                }
            }
            event = receiver.recv() => {
                let Ok(event) = event else {
                    continue;
                };
                if !subscribed_topics.contains(&event.topic) {
                    continue;
                }
                let payload = match serde_json::to_string(&event) {
                    Ok(payload) => payload,
                    Err(err) => {
                        tracing::warn!(error = %err, event_id = %event.id, "failed to serialize websocket event");
                        continue;
                    }
                };
                if socket.send(Message::Text(payload.into())).await.is_err() {
                    break;
                }
                state.ws_registry.touch(&registration.connection_id);
            }
        }
    }

    let duration_ms = started_at.elapsed().as_millis().min(i64::MAX as u128) as i64;
    let mut topics: Vec<String> = subscribed_topics.into_iter().collect();
    topics.sort();
    let session_ids = topics
        .iter()
        .filter_map(|topic| topic.strip_prefix("sessions.").map(str::to_owned))
        .collect::<Vec<_>>();
    state.ws_registry.unregister(&registration.connection_id);
    persist_ws_lifecycle_event(
        &state,
        "ws.client_disconnected",
        serde_json::json!({
            "connection_id": connection_id,
            "topics": topics,
            "session_ids": session_ids,
            "duration_ms": duration_ms,
            "reason": disconnect_reason,
        }),
    )
    .await;
}

/// Monotonically-increasing connection identifier. Pairs the connect/disconnect
/// events for a single client across the durable event log. Reset per process
/// — durability of the pair is provided by the timestamp + connection_id
/// composite, not the counter alone.
fn next_ws_connection_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
    format!("ws_{nanos}_{seq}")
}

async fn persist_ws_lifecycle_event(state: &AppState, kind: &str, payload: serde_json::Value) {
    let payload_text = match serde_json::to_string(&payload) {
        Ok(text) => text,
        Err(err) => {
            tracing::warn!(error = %err, kind, "failed to serialize ws lifecycle event payload");
            return;
        }
    };
    let store = state.state.lock().await;
    if let Err(err) = store.append_event_with_source(
        "info",
        kind,
        crate::state::EVENT_SOURCE_API,
        "",
        &payload_text,
    ) {
        tracing::warn!(error = %err, kind, "failed to persist ws lifecycle event");
    }
}

async fn handle_ws_client_message(subscribed_topics: &mut BTreeSet<String>, text: &str) {
    let message: WsClientMessage = match serde_json::from_str(text) {
        Ok(message) => message,
        Err(err) => {
            tracing::debug!(error = %err, "dropping malformed websocket client message");
            return;
        }
    };
    if message.message_type != "subscribe" {
        return;
    }
    for topic in message.topics {
        if topic.starts_with("sessions.")
            || topic.starts_with("commands.")
            || topic == "workspace"
            || topic == "agent"
            || topic == "status"
            || topic == "logs"
            || topic == "permissions"
        {
            subscribed_topics.insert(topic);
        } else {
            tracing::debug!(topic = %topic, "dropping unsupported websocket subscription topic");
        }
    }
}
