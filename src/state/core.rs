//! `StateStore` wiring: connection lifetime, default on-disk path, and the
//! `pub(super)` accessors that the domain leaves use to reach the underlying
//! `rusqlite::Connection`. Each domain table's persistence logic lives in the
//! sibling leaf (`sessions`, `events`, `commands`, ...); this file is just
//! the store struct + opener.

use crate::error::Result;
use crate::events::EventHub;
use rusqlite::Connection;
use std::path::{Path, PathBuf};

pub struct StateStore {
    connection: Connection,
    /// Optional fan-out for every `append_event` write. Set via
    /// `attach_event_hub` from `acps serve`; CLI tools that open the store
    /// read-only leave it `None`.
    event_hub: Option<EventHub>,
}

pub fn default_state_path(home: &Path) -> PathBuf {
    home.join(".local")
        .join("share")
        .join("acp-stack")
        .join("state.sqlite")
}

impl StateStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let connection = Connection::open(path)?;
        connection.execute_batch("PRAGMA foreign_keys = ON;")?;
        Ok(Self {
            connection,
            event_hub: None,
        })
    }

    /// Attach a live `EventHub` so every `append_event` write also fans out on
    /// the `logs` topic. The daemon (`acps serve`) calls this once at startup;
    /// CLI tools that open the store for ad-hoc queries leave it unset.
    pub fn attach_event_hub(&mut self, hub: EventHub) {
        self.event_hub = Some(hub);
    }

    /// `pub(super)` accessor so the domain leaves can issue queries against
    /// the shared connection without exposing the field publicly. Kept off
    /// the public API: external callers reach the store through the typed
    /// `query_*` / `append_*` methods, not the raw `Connection`.
    pub(super) fn connection(&self) -> &Connection {
        &self.connection
    }

    pub(super) fn event_hub(&self) -> Option<&EventHub> {
        self.event_hub.as_ref()
    }
}
