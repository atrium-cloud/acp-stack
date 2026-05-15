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
}
