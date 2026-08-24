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

    /// Fan out a workspace mutation on the `workspace` topic.
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

    /// Fan out a per-command output chunk or lifecycle transition on the
    /// `commands.{id}` topic.
    pub fn publish_command_event(&self, command_id: &str, event: &Event, data: Value) {
        self.publish(LiveEvent {
            event_type: "event",
            id: event.id.clone(),
            topic: format!("commands.{command_id}"),
            created_at: event.created_at.clone(),
            payload: json!({
                "kind": event.kind,
                "data": data,
            }),
        });
    }

    /// Fan out an agent-lifecycle row (`agent.*`) on the `agent.lifecycle`
    /// topic.
    pub fn publish_agent_event(&self, id: &str, created_at: &str, kind: &str, data: Value) {
        self.publish(LiveEvent {
            event_type: "event",
            id: id.to_owned(),
            topic: "agent.lifecycle".to_owned(),
            created_at: created_at.to_owned(),
            payload: json!({
                "kind": kind,
                "data": data,
            }),
        });
    }

    /// Fan out a runtime-status row (`server.*`) on the `status` topic, so a
    /// UI can watch runtime health without every agent-spawn transition.
    pub fn publish_status_event(&self, id: &str, created_at: &str, kind: &str, data: Value) {
        self.publish(LiveEvent {
            event_type: "event",
            id: id.to_owned(),
            topic: "status".to_owned(),
            created_at: created_at.to_owned(),
            payload: json!({
                "kind": kind,
                "data": data,
            }),
        });
    }

    /// Fan out a permission-lifecycle row (`permission.*`) on the
    /// `permissions` topic.
    pub fn publish_permission_event(&self, id: &str, created_at: &str, kind: &str, data: Value) {
        self.publish(LiveEvent {
            event_type: "event",
            id: id.to_owned(),
            topic: "permissions".to_owned(),
            created_at: created_at.to_owned(),
            payload: json!({
                "kind": kind,
                "data": data,
            }),
        });
    }

    /// Fan out every `events` row on the `logs` topic. `payload_json` was
    /// validated before write, so a parse failure here means downstream
    /// corruption: keep the wire shape stable and warn rather than swallow.
    pub fn publish_log_event(&self, event: &Event) {
        let data: Value = match serde_json::from_str(&event.payload_json) {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(
                    event_id = %event.id,
                    %error,
                    "publish_log_event: payload_json failed to parse after validation; \
                     emitting Null payload to keep wire shape stable"
                );
                Value::Null
            }
        };
        let mut data_object = serde_json::Map::new();
        data_object.insert("level".to_owned(), Value::String(event.level.clone()));
        data_object.insert("kind".to_owned(), Value::String(event.kind.clone()));
        data_object.insert("source".to_owned(), Value::String(event.source.clone()));
        data_object.insert("message".to_owned(), Value::String(event.message.clone()));
        data_object.insert("payload".to_owned(), data);
        if let Some(session_id) = event.session_id.as_ref() {
            data_object.insert("session_id".to_owned(), Value::String(session_id.clone()));
        }
        self.publish(LiveEvent {
            event_type: "event",
            id: event.id.clone(),
            topic: "logs".to_owned(),
            created_at: event.created_at.clone(),
            payload: json!({
                "kind": event.kind,
                "data": Value::Object(data_object),
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
            source: "system".to_owned(),
            session_id: None,
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

    #[tokio::test]
    async fn publish_command_event_routes_to_per_command_topic() {
        let hub = EventHub::new();
        let mut rx = hub.subscribe();
        let event = fake_event("command.stdout");

        hub.publish_command_event("cmd_42", &event, json!({"seq": 0, "data": "hello"}));

        let live = rx.recv().await.expect("event");
        assert_eq!(live.topic, "commands.cmd_42");
        assert_eq!(live.payload["kind"], "command.stdout");
        assert_eq!(live.payload["data"]["seq"], 0);
    }

    #[tokio::test]
    async fn publish_agent_event_routes_to_agent_lifecycle_topic() {
        let hub = EventHub::new();
        let mut rx = hub.subscribe();

        hub.publish_agent_event(
            "agl_1",
            "2026-05-15T00:00:00Z",
            "agent.started",
            json!({"pid": 123}),
        );

        let live = rx.recv().await.expect("event");
        assert_eq!(live.topic, "agent.lifecycle");
        assert_eq!(live.payload["kind"], "agent.started");
        assert_eq!(live.payload["data"]["pid"], 123);
    }

    #[tokio::test]
    async fn publish_status_event_routes_to_status_topic() {
        let hub = EventHub::new();
        let mut rx = hub.subscribe();

        hub.publish_status_event(
            "agl_2",
            "2026-05-15T00:00:00Z",
            "server.started",
            json!({"bind": "127.0.0.1:0"}),
        );

        let live = rx.recv().await.expect("event");
        assert_eq!(live.topic, "status");
        assert_eq!(live.payload["kind"], "server.started");
    }

    #[tokio::test]
    async fn publish_log_event_routes_to_logs_topic() {
        let hub = EventHub::new();
        let mut rx = hub.subscribe();
        let event = fake_event("workspace.write");

        hub.publish_log_event(&event);

        let live = rx.recv().await.expect("event");
        assert_eq!(live.topic, "logs");
        assert_eq!(live.payload["kind"], "workspace.write");
        assert_eq!(live.payload["data"]["level"], "info");
        assert_eq!(live.payload["data"]["kind"], "workspace.write");
    }
}
