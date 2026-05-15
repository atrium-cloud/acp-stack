use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::broadcast;

use crate::state::Event;

const EVENT_CHANNEL_CAPACITY: usize = 1024;

#[derive(Debug, Clone, Serialize)]
pub struct LiveEvent {
    #[serde(rename = "type")]
    pub event_type: &'static str,
    pub id: String,
    pub topic: String,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    pub payload: Value,
}

#[derive(Clone)]
pub struct EventHub {
    tx: broadcast::Sender<LiveEvent>,
}

impl Default for EventHub {
    fn default() -> Self {
        Self::new()
    }
}

impl EventHub {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        Self { tx }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<LiveEvent> {
        self.tx.subscribe()
    }

    pub fn publish(&self, event: LiveEvent) {
        let _ = self.tx.send(event);
    }

    pub fn publish_session_update(&self, session_id: &str, event: &Event, payload_json: &str) {
        let data = match serde_json::from_str::<Value>(payload_json) {
            Ok(value) => value,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    event_id = %event.id,
                    "failed to parse persisted session update for websocket fanout"
                );
                Value::Null
            }
        };
        self.publish(LiveEvent {
            event_type: "event",
            id: event.id.clone(),
            topic: format!("sessions.{session_id}"),
            created_at: event.created_at.clone(),
            payload: json!({
                "kind": event.kind,
                "data": data,
            }),
        });
    }

    /// Fan out a workspace mutation to subscribers of the `workspace` topic.
    /// Topic name matches `docs/specs/api/api.md:165-176`. Payload is shaped
    /// like the session-update envelope so client code can dispatch on
    /// `payload.kind` uniformly across topics.
    pub fn publish_workspace_event(&self, event: &Event, data: Value) {
        self.publish(LiveEvent {
            event_type: "event",
            id: event.id.clone(),
            topic: "workspace".to_owned(),
            created_at: event.created_at.clone(),
            payload: json!({
                "kind": event.kind,
                "data": data,
            }),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_event(kind: &str) -> Event {
        Event {
            id: "evt_test".to_owned(),
            created_at: "2026-05-15T00:00:00Z".to_owned(),
            level: "info".to_owned(),
            kind: kind.to_owned(),
            message: String::new(),
            payload_json: "{}".to_owned(),
        }
    }

    #[tokio::test]
    async fn publish_workspace_event_routes_to_workspace_topic() {
        let hub = EventHub::new();
        let mut rx = hub.subscribe();
        let event = fake_event("workspace.write");

        hub.publish_workspace_event(&event, json!({"path": "hello.md", "size": 5}));

        let live = rx.recv().await.expect("event");
        assert_eq!(live.event_type, "event");
        assert_eq!(live.topic, "workspace");
        assert_eq!(live.id, "evt_test");
        assert_eq!(live.payload["kind"], "workspace.write");
        assert_eq!(live.payload["data"]["path"], "hello.md");
        assert_eq!(live.payload["data"]["size"], 5);
    }

    #[tokio::test]
    async fn publish_session_update_routes_to_session_topic() {
        let hub = EventHub::new();
        let mut rx = hub.subscribe();
        let event = fake_event("session.update");

        hub.publish_session_update("sess_abc", &event, r#"{"foo": 1}"#);

        let live = rx.recv().await.expect("event");
        assert_eq!(live.topic, "sessions.sess_abc");
        assert_eq!(live.payload["kind"], "session.update");
        assert_eq!(live.payload["data"]["foo"], 1);
    }
}
