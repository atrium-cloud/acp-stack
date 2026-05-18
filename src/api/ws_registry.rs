use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use dashmap::DashMap;
use serde::Serialize;
use tokio::sync::Notify;

use crate::http_hardening::RequestOrigin;

#[derive(Default)]
pub struct WsRegistry {
    entries: DashMap<String, WsEntry>,
}

#[derive(Clone)]
struct WsEntry {
    connected_at: String,
    last_activity_at: Arc<std::sync::RwLock<String>>,
    topics: Arc<std::sync::RwLock<BTreeSet<String>>>,
    origin: RequestOrigin,
    disconnect_requested: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

pub struct WsRegistration {
    pub connection_id: String,
    pub disconnect_requested: Arc<AtomicBool>,
    pub notify: Arc<Notify>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WsConnectionView {
    pub connection_id: String,
    pub connected_at: String,
    pub last_activity_at: String,
    pub topics: Vec<String>,
    pub session_ids: Vec<String>,
    pub origin: RequestOrigin,
    pub disconnect_requested: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct WsSessionView {
    pub session_id: String,
    pub connection_count: usize,
}

impl WsRegistry {
    pub fn register(&self, connection_id: String, origin: RequestOrigin) -> WsRegistration {
        let now = current_timestamp();
        let entry = WsEntry {
            connected_at: now.clone(),
            last_activity_at: Arc::new(std::sync::RwLock::new(now)),
            topics: Arc::new(std::sync::RwLock::new(BTreeSet::new())),
            origin,
            disconnect_requested: Arc::new(AtomicBool::new(false)),
            notify: Arc::new(Notify::new()),
        };
        let registration = WsRegistration {
            connection_id: connection_id.clone(),
            disconnect_requested: entry.disconnect_requested.clone(),
            notify: entry.notify.clone(),
        };
        self.entries.insert(connection_id, entry);
        registration
    }

    pub fn unregister(&self, connection_id: &str) {
        self.entries.remove(connection_id);
    }

    pub fn touch(&self, connection_id: &str) {
        if let Some(entry) = self.entries.get(connection_id)
            && let Ok(mut last_activity_at) = entry.last_activity_at.write()
        {
            *last_activity_at = current_timestamp();
        }
    }

    pub fn update_topics(&self, connection_id: &str, topics: &BTreeSet<String>) {
        if let Some(entry) = self.entries.get(connection_id)
            && let Ok(mut current) = entry.topics.write()
        {
            *current = topics.clone();
        }
        self.touch(connection_id);
    }

    pub fn list_connections(&self) -> Vec<WsConnectionView> {
        let mut rows: Vec<_> = self
            .entries
            .iter()
            .map(|entry| {
                let topics = entry
                    .topics
                    .read()
                    .map(|topics| topics.iter().cloned().collect::<Vec<_>>())
                    .unwrap_or_default();
                let session_ids = session_ids_from_topics(&topics);
                WsConnectionView {
                    connection_id: entry.key().clone(),
                    connected_at: entry.connected_at.clone(),
                    last_activity_at: entry
                        .last_activity_at
                        .read()
                        .map(|value| value.clone())
                        .unwrap_or_else(|_| entry.connected_at.clone()),
                    topics,
                    session_ids,
                    origin: sanitized_origin(&entry.origin),
                    disconnect_requested: entry.disconnect_requested.load(Ordering::Relaxed),
                }
            })
            .collect();
        rows.sort_by(|left, right| left.connection_id.cmp(&right.connection_id));
        rows
    }

    pub fn list_sessions(&self) -> Vec<WsSessionView> {
        let mut counts = BTreeMap::<String, usize>::new();
        for connection in self.list_connections() {
            for session_id in connection.session_ids {
                *counts.entry(session_id).or_insert(0) += 1;
            }
        }
        counts
            .into_iter()
            .map(|(session_id, connection_count)| WsSessionView {
                session_id,
                connection_count,
            })
            .collect()
    }

    pub fn disconnect_connections(&self, connection_ids: &[String]) -> usize {
        let mut count = 0;
        for id in connection_ids {
            if let Some(entry) = self.entries.get(id) {
                entry.disconnect_requested.store(true, Ordering::Relaxed);
                entry.notify.notify_waiters();
                count += 1;
            }
        }
        count
    }

    pub fn disconnect_sessions(&self, session_ids: &[String]) -> usize {
        let requested: BTreeSet<&str> = session_ids.iter().map(String::as_str).collect();
        let mut count = 0;
        for entry in self.entries.iter() {
            let topics = entry
                .topics
                .read()
                .map(|topics| topics.iter().cloned().collect::<Vec<_>>())
                .unwrap_or_default();
            if session_ids_from_topics(&topics)
                .iter()
                .any(|session_id| requested.contains(session_id.as_str()))
            {
                entry.disconnect_requested.store(true, Ordering::Relaxed);
                entry.notify.notify_waiters();
                count += 1;
            }
        }
        count
    }
}

fn session_ids_from_topics(topics: &[String]) -> Vec<String> {
    let mut sessions = topics
        .iter()
        .filter_map(|topic| topic.strip_prefix("sessions.").map(str::to_owned))
        .collect::<Vec<_>>();
    sessions.sort();
    sessions.dedup();
    sessions
}

fn current_timestamp() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}

fn sanitized_origin(origin: &RequestOrigin) -> RequestOrigin {
    let mut sanitized = origin.clone();
    sanitized.client_ip = None;
    sanitized
}
