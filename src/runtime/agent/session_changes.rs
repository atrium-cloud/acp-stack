//! Bounded, process-local projection of ACP file-diff tool content.
//!
//! The durable session event stream remains the raw ACP source of truth. This
//! store exists only to give API clients a compact current snapshot without
//! replaying those events or consulting Git/the workspace.

use std::collections::HashMap;
use std::io::{self, Write};
use std::mem::size_of;
use std::path::Path;
use std::sync::Arc;

use agent_client_protocol::schema::v1::{
    Diff, Meta, SessionUpdate, ToolCall, ToolCallContent, ToolCallStatus, ToolCallUpdate, ToolKind,
};
use rand::RngExt;
use serde::{Serialize, Serializer};
use serde_json::value::RawValue;
use tokio::sync::Mutex as TokioMutex;

use crate::envelope::ApiSuccess;

/// Leaves one MiB below Platform's existing eight-MiB ACPS JSON response cap
/// for the standard success envelope and proxy bookkeeping.
pub(crate) const MAX_SESSION_CHANGES_BYTES: usize = 7 * 1024 * 1024;

/// Full old/new file text is intentionally retained only within a fixed
/// daemon-wide budget so a long-running agent cannot grow memory without bound.
pub(crate) const MAX_TOTAL_SESSION_CHANGES_BYTES: usize = 64 * 1024 * 1024;

/// Bounds metadata-heavy agents even when their individual diff bodies are tiny.
pub(crate) const MAX_TRACKED_TOOL_CALLS_PER_SESSION: usize = 512;

/// Conservative allowance for allocator bookkeeping on every owned heap block.
const ALLOCATION_OVERHEAD_BYTES: usize = 2 * size_of::<usize>();

/// `HashMap::capacity` reports usable entries rather than allocated buckets.
/// Charging two full entry slots per usable entry covers the load-factor gap,
/// control bytes, and alignment without depending on hashbrown internals.
const HASH_TABLE_CAPACITY_MULTIPLIER: usize = 2;

/// Cloneable opaque handle shared by the API and every supervised agent
/// bridge. The reducer itself stays private to this module.
#[derive(Clone)]
pub struct SessionChangesHandle {
    inner: Arc<TokioMutex<SessionChangesStore>>,
}

impl Default for SessionChangesHandle {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionChangesHandle {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(TokioMutex::new(SessionChangesStore::new())),
        }
    }

    pub(crate) async fn apply(&self, session_id: &str, update: &SessionUpdate) {
        self.inner.lock().await.apply(session_id, update);
    }

    pub(crate) async fn snapshot(&self, session_id: &str) -> SessionChangesSnapshot {
        self.inner.lock().await.snapshot(session_id)
    }
}

#[derive(Clone, Copy)]
struct SessionChangeLimits {
    max_session_bytes: usize,
    max_total_bytes: usize,
    max_tool_calls_per_session: usize,
}

impl Default for SessionChangeLimits {
    fn default() -> Self {
        Self {
            max_session_bytes: MAX_SESSION_CHANGES_BYTES,
            max_total_bytes: MAX_TOTAL_SESSION_CHANGES_BYTES,
            max_tool_calls_per_session: MAX_TRACKED_TOOL_CALLS_PER_SESSION,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct CapturedMeta(Box<RawValue>);

impl CapturedMeta {
    fn new(meta: &Meta) -> Self {
        let canonical = canonical_json(&serde_json::Value::Object(meta.clone()));
        let raw = serde_json::value::to_raw_value(&canonical)
            .expect("serde_json metadata values must serialize");
        Self(raw)
    }

    fn retained_bytes(&self) -> u128 {
        allocation_bytes(self.0.get().len())
    }
}

impl PartialEq for CapturedMeta {
    fn eq(&self, other: &Self) -> bool {
        self.0.get() == other.0.get()
    }
}

impl Serialize for CapturedMeta {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.serialize(serializer)
    }
}

fn canonical_json(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.iter().map(canonical_json).collect())
        }
        serde_json::Value::Object(object) => {
            let mut entries = object.iter().collect::<Vec<_>>();
            entries.sort_by_key(|(key, _)| *key);
            let mut canonical = serde_json::Map::with_capacity(entries.len());
            for (key, value) in entries {
                canonical.insert(key.clone(), canonical_json(value));
            }
            serde_json::Value::Object(canonical)
        }
        scalar => scalar.clone(),
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum CapturedToolCallContent {
    Diff {
        path: Box<Path>,
        /// Deliberately serialized as `null` for creates instead of omitted.
        #[serde(rename = "oldText")]
        old_text: Option<Box<str>>,
        #[serde(rename = "newText")]
        new_text: Box<str>,
        #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
        meta: Option<CapturedMeta>,
    },
}

impl From<&Diff> for CapturedToolCallContent {
    fn from(diff: &Diff) -> Self {
        Self::Diff {
            path: diff.path.clone().into_boxed_path(),
            old_text: diff.old_text.as_deref().map(Into::into),
            new_text: diff.new_text.as_str().into(),
            meta: diff.meta.as_ref().map(CapturedMeta::new),
        }
    }
}

impl CapturedToolCallContent {
    fn retained_bytes(&self) -> u128 {
        match self {
            Self::Diff {
                path,
                old_text,
                new_text,
                meta,
            } => allocation_bytes(path.as_os_str().len())
                .saturating_add(boxed_str_bytes(old_text.as_deref()))
                .saturating_add(boxed_str_bytes(Some(new_text)))
                .saturating_add(meta.as_ref().map_or(0, CapturedMeta::retained_bytes)),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct CapturedToolCall {
    tool_call_id: Box<str>,
    title: Option<Box<str>>,
    kind: Option<ToolKind>,
    status: Option<ToolCallStatus>,
    content: Box<[CapturedToolCallContent]>,
    #[serde(skip)]
    first_seen: u64,
    #[serde(skip)]
    last_updated: u64,
    #[serde(skip)]
    wire_bytes: usize,
    #[serde(skip)]
    retained_bytes: u128,
}

impl CapturedToolCall {
    fn empty(tool_call_id: String, sequence: u64) -> Self {
        let mut tool_call = Self {
            tool_call_id: tool_call_id.into_boxed_str(),
            title: None,
            kind: None,
            status: None,
            content: Box::default(),
            first_seen: sequence,
            last_updated: sequence,
            wire_bytes: 0,
            retained_bytes: 0,
        };
        tool_call.refresh_cached_sizes();
        tool_call
    }

    fn from_tool_call(tool_call: &ToolCall, sequence: u64, first_seen: u64) -> Self {
        let mut captured = Self {
            tool_call_id: tool_call.tool_call_id.0.to_string().into_boxed_str(),
            title: Some(tool_call.title.as_str().into()),
            kind: Some(tool_call.kind),
            status: Some(tool_call.status),
            content: captured_diffs(&tool_call.content),
            first_seen,
            last_updated: sequence,
            wire_bytes: 0,
            retained_bytes: 0,
        };
        captured.refresh_cached_sizes();
        captured
    }

    fn visible(&self) -> bool {
        !self.content.is_empty()
    }

    fn refresh_cached_sizes(&mut self) {
        self.wire_bytes = serialized_wire_bytes(self, "captured tool call");
        self.retained_bytes = (size_of::<Self>() as u128)
            .saturating_add(allocation_bytes(self.tool_call_id.len()))
            .saturating_add(boxed_str_bytes(self.title.as_deref()))
            .saturating_add(boxed_slice_bytes::<CapturedToolCallContent>(
                self.content.len(),
            ))
            .saturating_add(self.content.iter().fold(0u128, |total, content| {
                total.saturating_add(content.retained_bytes())
            }));
    }
}

impl PartialEq for CapturedToolCall {
    fn eq(&self, other: &Self) -> bool {
        self.tool_call_id == other.tool_call_id
            && self.title == other.title
            && self.kind == other.kind
            && self.status == other.status
            && self.content == other.content
    }
}

fn captured_diffs(content: &[ToolCallContent]) -> Box<[CapturedToolCallContent]> {
    content
        .iter()
        .filter_map(|content| match content {
            ToolCallContent::Diff(diff) => Some(CapturedToolCallContent::from(diff)),
            _ => None,
        })
        .collect::<Vec<_>>()
        .into_boxed_slice()
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct SessionChangesSnapshot {
    session_id: String,
    generation: String,
    revision: u64,
    truncated: bool,
    tool_calls: Vec<CapturedToolCall>,
}

#[derive(Default)]
struct SessionChangesBucket {
    revision: u64,
    truncated: bool,
    tool_calls: HashMap<String, CapturedToolCall>,
    last_access: u64,
    visible_wire_bytes: u128,
    visible_count: usize,
    retained_bytes: u128,
}

impl SessionChangesBucket {
    fn visible_tool_calls(&self) -> Vec<CapturedToolCall> {
        let mut tool_calls = self
            .tool_calls
            .values()
            .filter(|tool_call| tool_call.visible())
            .cloned()
            .collect::<Vec<_>>();
        tool_calls.sort_by(|left, right| {
            left.first_seen
                .cmp(&right.first_seen)
                .then_with(|| left.tool_call_id.cmp(&right.tool_call_id))
        });
        tool_calls
    }

    fn refresh_cached_sizes(&mut self, session_id: &String) {
        self.visible_wire_bytes = 0;
        self.visible_count = 0;
        let mut retained_bytes = (size_of::<Self>() as u128)
            .saturating_add(string_allocation_bytes(session_id))
            .saturating_add(hash_map_capacity_bytes::<String, CapturedToolCall>(
                self.tool_calls.capacity(),
            ));
        for (tool_call_id, tool_call) in &self.tool_calls {
            retained_bytes = retained_bytes
                .saturating_add(string_allocation_bytes(tool_call_id))
                .saturating_add(tool_call.retained_bytes);
            if tool_call.visible() {
                self.visible_count = self.visible_count.saturating_add(1);
                self.visible_wire_bytes = self
                    .visible_wire_bytes
                    .saturating_add(tool_call.wire_bytes as u128);
            }
        }
        self.retained_bytes = retained_bytes;
    }

    fn response_wire_bytes(&self, empty_envelope_bytes: usize) -> u128 {
        let separators = self.visible_count.saturating_sub(1) as u128;
        (empty_envelope_bytes as u128)
            .saturating_add(self.visible_wire_bytes)
            .saturating_add(separators)
    }
}

/// In-memory reducer for the ACP tool-call stream.
pub(crate) struct SessionChangesStore {
    generation: String,
    revision: u64,
    sequence: u64,
    capacity_reached: bool,
    sessions: HashMap<String, SessionChangesBucket>,
    limits: SessionChangeLimits,
    structural_retained_bytes: u128,
    retained_bytes: u128,
}

impl Default for SessionChangesStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionChangesStore {
    pub(crate) fn new() -> Self {
        Self::with_generation_and_limits(new_generation(), SessionChangeLimits::default())
    }

    #[cfg(test)]
    fn with_limits(generation: &str, limits: SessionChangeLimits) -> Self {
        Self::with_generation_and_limits(generation.to_owned(), limits)
    }

    fn with_generation_and_limits(generation: String, limits: SessionChangeLimits) -> Self {
        let mut store = Self {
            generation,
            revision: 0,
            sequence: 0,
            capacity_reached: false,
            sessions: HashMap::new(),
            limits,
            structural_retained_bytes: 0,
            retained_bytes: 0,
        };
        store.refresh_structural_retained_bytes();
        store
    }

    /// Apply the file-change-relevant portion of an ACP session update.
    /// Returns immediately for every non-tool update.
    pub(crate) fn apply(&mut self, session_id: &str, update: &SessionUpdate) {
        if !matches!(
            update,
            SessionUpdate::ToolCall(_) | SessionUpdate::ToolCallUpdate(_)
        ) {
            return;
        }

        let sequence = self.next_sequence();
        let (session_key, mut bucket) = match self.sessions.remove_entry(session_id) {
            Some((session_key, bucket)) => {
                self.retained_bytes = self.retained_bytes.saturating_sub(bucket.retained_bytes);
                (session_key, bucket)
            }
            None => (
                session_id.to_owned(),
                SessionChangesBucket {
                    revision: if self.capacity_reached {
                        self.revision
                    } else {
                        0
                    },
                    truncated: self.capacity_reached,
                    ..SessionChangesBucket::default()
                },
            ),
        };
        let before_truncated = bucket.truncated;

        let mutation = match update {
            SessionUpdate::ToolCall(tool_call) => apply_tool_call(&mut bucket, tool_call, sequence),
            SessionUpdate::ToolCallUpdate(tool_call_update) => {
                apply_tool_call_update(&mut bucket, tool_call_update, sequence)
            }
            _ => unreachable!("non-tool updates return before reducer mutation"),
        };

        bucket.last_access = sequence;
        bucket.refresh_cached_sizes(&session_key);
        let eviction =
            self.enforce_session_limits(&session_key, &mut bucket, &mutation.affected_id);
        let affected_changed = if eviction.affected_removed {
            mutation.old_visible
        } else {
            mutation.visible_state_changed
        };
        if affected_changed
            || eviction.removed_visible_other
            || eviction.removed_any
            || before_truncated != bucket.truncated
        {
            bucket.revision = self.next_revision();
        }
        self.insert_bucket(session_key, bucket);
        self.enforce_global_limit();
    }

    pub(crate) fn snapshot(&mut self, session_id: &str) -> SessionChangesSnapshot {
        let access = self.next_sequence();
        match self.sessions.get_mut(session_id) {
            Some(bucket) => {
                bucket.last_access = access;
                SessionChangesSnapshot {
                    session_id: session_id.to_owned(),
                    generation: self.generation.clone(),
                    revision: bucket.revision,
                    truncated: bucket.truncated,
                    tool_calls: bucket.visible_tool_calls(),
                }
            }
            None => SessionChangesSnapshot {
                session_id: session_id.to_owned(),
                generation: self.generation.clone(),
                revision: if self.capacity_reached {
                    self.revision
                } else {
                    0
                },
                truncated: self.capacity_reached,
                tool_calls: Vec::new(),
            },
        }
    }

    fn enforce_session_limits(
        &self,
        session_id: &String,
        bucket: &mut SessionChangesBucket,
        affected_id: &str,
    ) -> SessionEvictionOutcome {
        let empty_false = empty_envelope_wire_bytes(session_id, &self.generation, u64::MAX, false);
        let empty_true = empty_envelope_wire_bytes(session_id, &self.generation, u64::MAX, true);
        let current_envelope_bytes = if bucket.truncated {
            empty_true
        } else {
            empty_false
        };
        if bucket.tool_calls.len() <= self.limits.max_tool_calls_per_session
            && bucket.response_wire_bytes(current_envelope_bytes)
                <= self.limits.max_session_bytes as u128
        {
            return SessionEvictionOutcome::default();
        }
        let mut victims = bucket
            .tool_calls
            .values()
            .map(|tool_call| (tool_call.last_updated, tool_call.tool_call_id.to_string()))
            .collect::<Vec<_>>();
        victims.sort_by(|(left_updated, left_id), (right_updated, right_id)| {
            left_updated
                .cmp(right_updated)
                .then_with(|| left_id.cmp(right_id))
        });

        let mut outcome = SessionEvictionOutcome::default();
        for (_, victim_id) in victims {
            let envelope_bytes = if bucket.truncated {
                empty_true
            } else {
                empty_false
            };
            if bucket.tool_calls.len() <= self.limits.max_tool_calls_per_session
                && bucket.response_wire_bytes(envelope_bytes)
                    <= self.limits.max_session_bytes as u128
            {
                break;
            }
            let Some(victim) = bucket.tool_calls.remove(&victim_id) else {
                continue;
            };
            if victim.visible() {
                bucket.visible_count = bucket.visible_count.saturating_sub(1);
                bucket.visible_wire_bytes = bucket
                    .visible_wire_bytes
                    .saturating_sub(victim.wire_bytes as u128);
                if victim_id == affected_id {
                    outcome.affected_removed = true;
                } else {
                    outcome.removed_visible_other = true;
                }
            } else if victim_id == affected_id {
                outcome.affected_removed = true;
            }
            bucket.truncated = true;
            outcome.removed_any = true;
        }

        if outcome.removed_any {
            bucket.tool_calls.shrink_to_fit();
            bucket.refresh_cached_sizes(session_id);
        }
        outcome
    }

    fn enforce_global_limit(&mut self) {
        if self.retained_bytes <= self.limits.max_total_bytes as u128 {
            return;
        }

        self.sessions.shrink_to_fit();
        self.refresh_structural_retained_bytes();
        if self.retained_bytes <= self.limits.max_total_bytes as u128 {
            return;
        }

        let mut victims = self
            .sessions
            .iter()
            .map(|(session_id, bucket)| (bucket.last_access, session_id.clone()))
            .collect::<Vec<_>>();
        victims.sort_by(|(left_access, left_id), (right_access, right_id)| {
            left_access
                .cmp(right_access)
                .then_with(|| left_id.cmp(right_id))
        });

        for (_, victim_id) in victims {
            if self.retained_bytes <= self.limits.max_total_bytes as u128 {
                break;
            }
            let Some((victim_id, mut victim)) = self.take_bucket(&victim_id) else {
                continue;
            };
            if victim.tool_calls.is_empty() {
                // Once a compact tombstone itself is the least-recent data,
                // dropping it is the only way to keep the global bound. Its
                // session id is forgotten, so from here on every unknown or
                // new session must conservatively report `truncated` via
                // `capacity_reached` and the daemon-global revision.
                self.capacity_reached = true;
                self.next_revision();
            } else {
                victim.tool_calls = HashMap::new();
                victim.truncated = true;
                victim.revision = self.next_revision();
                victim.refresh_cached_sizes(&victim_id);
                if self.retained_bytes.saturating_add(victim.retained_bytes)
                    > self.limits.max_total_bytes as u128
                {
                    self.capacity_reached = true;
                    self.next_revision();
                } else {
                    self.insert_bucket(victim_id, victim);
                }
            }
        }

        self.sessions.shrink_to_fit();
        self.refresh_structural_retained_bytes();
    }

    fn take_bucket(&mut self, session_id: &str) -> Option<(String, SessionChangesBucket)> {
        let entry = self.sessions.remove_entry(session_id)?;
        self.retained_bytes = self.retained_bytes.saturating_sub(entry.1.retained_bytes);
        Some(entry)
    }

    fn insert_bucket(&mut self, session_id: String, bucket: SessionChangesBucket) {
        self.retained_bytes = self.retained_bytes.saturating_add(bucket.retained_bytes);
        debug_assert!(!self.sessions.contains_key(&session_id));
        self.sessions.insert(session_id, bucket);
        self.refresh_structural_retained_bytes();
    }

    fn refresh_structural_retained_bytes(&mut self) {
        self.retained_bytes = self
            .retained_bytes
            .saturating_sub(self.structural_retained_bytes);
        self.structural_retained_bytes = (size_of::<Self>() as u128)
            .saturating_add(string_allocation_bytes(&self.generation))
            .saturating_add(hash_map_capacity_bytes::<String, SessionChangesBucket>(
                self.sessions.capacity(),
            ));
        self.retained_bytes = self
            .retained_bytes
            .saturating_add(self.structural_retained_bytes);
    }

    #[cfg(test)]
    fn recomputed_retained_bytes(&self) -> u128 {
        self.structural_retained_bytes.saturating_add(
            self.sessions.values().fold(0u128, |total, bucket| {
                total.saturating_add(bucket.retained_bytes)
            }),
        )
    }

    fn next_sequence(&mut self) -> u64 {
        self.sequence = self.sequence.saturating_add(1);
        self.sequence
    }

    fn next_revision(&mut self) -> u64 {
        if self.revision == u64::MAX {
            self.generation = new_generation();
            self.revision = 0;
            for bucket in self.sessions.values_mut() {
                bucket.revision = 0;
            }
            self.refresh_structural_retained_bytes();
        }
        self.revision += 1;
        self.revision
    }
}

struct MutationOutcome {
    affected_id: String,
    old_visible: bool,
    visible_state_changed: bool,
}

#[derive(Default)]
struct SessionEvictionOutcome {
    affected_removed: bool,
    removed_visible_other: bool,
    removed_any: bool,
}

fn apply_tool_call(
    bucket: &mut SessionChangesBucket,
    tool_call: &ToolCall,
    sequence: u64,
) -> MutationOutcome {
    let tool_call_id = tool_call.tool_call_id.0.to_string();
    let existing = bucket.tool_calls.get(&tool_call_id);
    let first_seen = existing.map_or(sequence, |existing| existing.first_seen);
    let replacement = CapturedToolCall::from_tool_call(tool_call, sequence, first_seen);
    let old_visible = existing.is_some_and(CapturedToolCall::visible);
    let visible_state_changed = match existing {
        Some(existing) if existing.visible() && replacement.visible() => existing != &replacement,
        Some(existing) => existing.visible() != replacement.visible(),
        None => replacement.visible(),
    };
    bucket.tool_calls.insert(tool_call_id, replacement);
    MutationOutcome {
        affected_id: tool_call.tool_call_id.0.to_string(),
        old_visible,
        visible_state_changed,
    }
}

fn apply_tool_call_update(
    bucket: &mut SessionChangesBucket,
    update: &ToolCallUpdate,
    sequence: u64,
) -> MutationOutcome {
    let tool_call_id = update.tool_call_id.0.to_string();
    let tool_call = bucket
        .tool_calls
        .entry(tool_call_id.clone())
        .or_insert_with(|| CapturedToolCall::empty(tool_call_id.clone(), sequence));
    let old_visible = tool_call.visible();
    let mut fields_changed = false;
    if let Some(title) = &update.fields.title
        && tool_call.title.as_deref() != Some(title.as_str())
    {
        tool_call.title = Some(title.as_str().into());
        fields_changed = true;
    }
    if let Some(kind) = &update.fields.kind
        && tool_call.kind != Some(*kind)
    {
        tool_call.kind = Some(*kind);
        fields_changed = true;
    }
    if let Some(status) = update.fields.status
        && tool_call.status != Some(status)
    {
        tool_call.status = Some(status);
        fields_changed = true;
    }
    if let Some(content) = &update.fields.content {
        let replacement = captured_diffs(content);
        if tool_call.content != replacement {
            tool_call.content = replacement;
            fields_changed = true;
        }
    }
    tool_call.last_updated = sequence;
    tool_call.refresh_cached_sizes();
    let new_visible = tool_call.visible();
    MutationOutcome {
        affected_id: tool_call_id,
        old_visible,
        visible_state_changed: old_visible != new_visible || (new_visible && fields_changed),
    }
}

fn empty_envelope_wire_bytes(
    session_id: &str,
    generation: &str,
    revision: u64,
    truncated: bool,
) -> usize {
    let snapshot = SessionChangesSnapshot {
        session_id: session_id.to_owned(),
        generation: generation.to_owned(),
        revision,
        truncated,
        tool_calls: Vec::new(),
    };
    serialized_wire_bytes(&ApiSuccess::new(snapshot), "empty session changes envelope")
}

#[derive(Default)]
struct CountingWriter {
    bytes: usize,
}

impl Write for CountingWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.bytes = self.bytes.saturating_add(buffer.len());
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn serialized_wire_bytes<T: Serialize>(value: &T, label: &str) -> usize {
    let mut counter = CountingWriter::default();
    match serde_json::to_writer(&mut counter, value) {
        Ok(()) => counter.bytes,
        Err(error) => {
            tracing::error!(error = %error, label, "failed to size JSON value");
            usize::MAX
        }
    }
}

fn allocation_bytes(payload_bytes: usize) -> u128 {
    if payload_bytes == 0 {
        0
    } else {
        (payload_bytes as u128).saturating_add(ALLOCATION_OVERHEAD_BYTES as u128)
    }
}

fn boxed_str_bytes(value: Option<&str>) -> u128 {
    value.map_or(0, |value| allocation_bytes(value.len()))
}

fn string_allocation_bytes(value: &String) -> u128 {
    allocation_bytes(value.capacity())
}

fn boxed_slice_bytes<T>(length: usize) -> u128 {
    allocation_bytes(length.saturating_mul(size_of::<T>()))
}

fn hash_map_capacity_bytes<Key, Value>(capacity: usize) -> u128 {
    if capacity == 0 {
        return 0;
    }
    let bytes_per_slot = size_of::<Key>()
        .saturating_add(size_of::<Value>())
        .saturating_add(1);
    allocation_bytes(
        capacity
            .saturating_mul(HASH_TABLE_CAPACITY_MULTIPLIER)
            .saturating_mul(bytes_per_slot),
    )
}

fn new_generation() -> String {
    let mut bytes = [0u8; 16];
    rand::rng().fill(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{ToolCallId, ToolCallUpdateFields};

    fn diff(path: &str, old_text: Option<&str>, new_text: &str) -> ToolCallContent {
        ToolCallContent::Diff(Diff::new(path, new_text).old_text(old_text.map(str::to_owned)))
    }

    fn tool_call(id: &str, content: Vec<ToolCallContent>) -> ToolCall {
        ToolCall::new(id.to_owned(), format!("edit {id}"))
            .kind(ToolKind::Edit)
            .status(ToolCallStatus::InProgress)
            .content(content)
    }

    #[test]
    fn captures_create_and_edit_diff_content() {
        let mut store =
            SessionChangesStore::with_limits("generation", SessionChangeLimits::default());
        store.apply(
            "session",
            &SessionUpdate::ToolCall(tool_call(
                "call",
                vec![
                    diff("/workspace/new.rs", None, "new"),
                    diff("/workspace/existing.rs", Some("before"), "after"),
                ],
            )),
        );

        let value = serde_json::to_value(store.snapshot("session")).expect("snapshot JSON");
        assert_eq!(value["generation"], "generation");
        assert_eq!(value["revision"], 1);
        assert_eq!(
            value["tool_calls"][0]["content"][0]["oldText"],
            serde_json::Value::Null
        );
        assert_eq!(value["tool_calls"][0]["content"][1]["oldText"], "before");
        assert_eq!(value["tool_calls"][0]["content"][1]["newText"], "after");
    }

    #[test]
    fn tool_call_update_retains_replaces_and_clears_content() {
        let mut store =
            SessionChangesStore::with_limits("generation", SessionChangeLimits::default());
        store.apply(
            "session",
            &SessionUpdate::ToolCall(tool_call(
                "call",
                vec![diff("/workspace/file", Some("one"), "two")],
            )),
        );
        store.apply(
            "session",
            &SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                ToolCallId::new("call"),
                ToolCallUpdateFields::new().status(ToolCallStatus::Completed),
            )),
        );
        let retained = store.snapshot("session");
        assert_eq!(retained.tool_calls.len(), 1);
        assert_eq!(
            retained.tool_calls[0].status,
            Some(ToolCallStatus::Completed)
        );

        store.apply(
            "session",
            &SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                ToolCallId::new("call"),
                ToolCallUpdateFields::new().content(vec![diff(
                    "/workspace/file",
                    Some("one"),
                    "three",
                )]),
            )),
        );
        let replaced = serde_json::to_value(store.snapshot("session")).expect("snapshot JSON");
        assert_eq!(replaced["tool_calls"][0]["content"][0]["newText"], "three");

        store.apply(
            "session",
            &SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                ToolCallId::new("call"),
                ToolCallUpdateFields::new().content(Vec::new()),
            )),
        );
        assert!(store.snapshot("session").tool_calls.is_empty());
    }

    #[test]
    fn update_before_initial_preserves_unknown_scalar_fields() {
        let mut store =
            SessionChangesStore::with_limits("generation", SessionChangeLimits::default());
        store.apply(
            "session",
            &SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                ToolCallId::new("call"),
                ToolCallUpdateFields::new()
                    .title("create file")
                    .content(vec![diff("/workspace/file", None, "content")]),
            )),
        );
        let snapshot = store.snapshot("session");
        assert_eq!(snapshot.tool_calls.len(), 1);
        assert_eq!(snapshot.tool_calls[0].title.as_deref(), Some("create file"));
        assert_eq!(snapshot.tool_calls[0].kind, None);
        assert_eq!(snapshot.tool_calls[0].status, None);
    }

    #[test]
    fn bare_updates_are_still_bounded() {
        let limits = SessionChangeLimits {
            max_session_bytes: 1_500,
            max_total_bytes: 10_000,
            max_tool_calls_per_session: 1,
        };
        let mut store = SessionChangesStore::with_limits("generation", limits);
        for tool_call_id in ["first", "second"] {
            store.apply(
                "session",
                &SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                    ToolCallId::new(tool_call_id),
                    ToolCallUpdateFields::new(),
                )),
            );
        }

        let bucket = store.sessions.get("session").expect("session bucket");
        assert_eq!(bucket.tool_calls.len(), 1);
        assert!(store.snapshot("session").truncated);
    }

    #[test]
    fn identical_visible_updates_do_not_advance_revision() {
        let mut store =
            SessionChangesStore::with_limits("generation", SessionChangeLimits::default());
        let update = SessionUpdate::ToolCall(tool_call(
            "call",
            vec![diff("/workspace/file", Some("before"), "after")],
        ));
        store.apply("session", &update);
        let revision = store.snapshot("session").revision;

        store.apply("session", &update);

        assert_eq!(store.snapshot("session").revision, revision);
    }

    #[test]
    fn repeated_whole_call_evictions_advance_revision() {
        let mut store = SessionChangesStore::with_limits(
            "generation",
            SessionChangeLimits {
                max_session_bytes: 10_000,
                max_total_bytes: 1_000_000,
                max_tool_calls_per_session: 0,
            },
        );
        store.apply(
            "session",
            &SessionUpdate::ToolCall(tool_call(
                "first",
                vec![diff("/workspace/first", Some("before"), "after")],
            )),
        );
        let first_revision = store.snapshot("session").revision;

        store.apply(
            "session",
            &SessionUpdate::ToolCall(tool_call(
                "second",
                vec![diff("/workspace/second", Some("before"), "after")],
            )),
        );
        let snapshot = store.snapshot("session");

        assert!(snapshot.truncated);
        assert!(snapshot.tool_calls.is_empty());
        assert!(snapshot.revision > first_revision);
    }

    #[test]
    fn revision_overflow_starts_a_new_monotonic_generation() {
        let mut store =
            SessionChangesStore::with_limits("generation", SessionChangeLimits::default());
        for session_id in ["first-session", "second-session"] {
            store.apply(
                session_id,
                &SessionUpdate::ToolCall(tool_call(
                    "call",
                    vec![diff("/workspace/file", Some("before"), "after")],
                )),
            );
        }
        store.revision = u64::MAX;

        store.apply(
            "first-session",
            &SessionUpdate::ToolCall(tool_call(
                "call",
                vec![diff("/workspace/file", Some("before"), "changed")],
            )),
        );
        let first = store.snapshot("first-session");
        let second_before_update = store.snapshot("second-session");
        assert_ne!(first.generation, "generation");
        assert_eq!(first.generation, second_before_update.generation);
        assert_eq!(first.revision, 1);
        assert_eq!(second_before_update.revision, 0);

        store.apply(
            "second-session",
            &SessionUpdate::ToolCall(tool_call(
                "call",
                vec![diff("/workspace/file", Some("before"), "changed")],
            )),
        );
        let second_after_update = store.snapshot("second-session");
        assert_eq!(second_after_update.generation, first.generation);
        assert!(second_after_update.revision > first.revision);
    }

    #[test]
    fn reactivated_session_stays_truncated_after_global_eviction() {
        let mut store =
            SessionChangesStore::with_limits("generation", SessionChangeLimits::default());
        store.capacity_reached = true;
        store.revision = 7;

        store.apply(
            "evicted-session",
            &SessionUpdate::ToolCall(tool_call(
                "call",
                vec![diff("/workspace/file", Some("before"), "after")],
            )),
        );

        let snapshot = store.snapshot("evicted-session");
        assert!(snapshot.truncated);
        assert_eq!(snapshot.revision, 8);
    }

    #[test]
    fn count_and_byte_limits_evict_whole_calls_and_stay_truncated() {
        let limits = SessionChangeLimits {
            max_session_bytes: 550,
            max_total_bytes: 1_000_000,
            max_tool_calls_per_session: 1,
        };
        let mut store = SessionChangesStore::with_limits("generation", limits);
        store.apply(
            "session",
            &SessionUpdate::ToolCall(tool_call(
                "first",
                vec![diff("/workspace/first", Some("before"), "after")],
            )),
        );
        store.apply(
            "session",
            &SessionUpdate::ToolCall(tool_call(
                "second",
                vec![diff("/workspace/second", Some("before"), "after")],
            )),
        );
        let snapshot = store.snapshot("session");
        assert!(snapshot.truncated);
        assert_eq!(snapshot.tool_calls.len(), 1);
        assert_eq!(snapshot.tool_calls[0].tool_call_id.as_ref(), "second");

        store.apply(
            "session",
            &SessionUpdate::ToolCall(tool_call(
                "oversized",
                vec![diff("/workspace/large", Some("before"), &"x".repeat(1_000))],
            )),
        );
        let snapshot = store.snapshot("session");
        assert!(snapshot.truncated);
        assert!(snapshot.tool_calls.is_empty());
    }

    #[test]
    fn global_limit_clears_least_recent_session_data() {
        let limits = SessionChangeLimits {
            max_session_bytes: 1_500,
            max_total_bytes: usize::MAX,
            max_tool_calls_per_session: 10,
        };
        let mut store = SessionChangesStore::with_limits("generation", limits);
        store.apply(
            "older",
            &SessionUpdate::ToolCall(tool_call(
                "older-call",
                vec![diff("/workspace/older", Some("a"), &"b".repeat(300))],
            )),
        );
        store.apply(
            "newer",
            &SessionUpdate::ToolCall(tool_call(
                "newer-call",
                vec![diff("/workspace/newer", Some("a"), &"c".repeat(300))],
            )),
        );

        set_limit_to_retain_oldest_tombstone(&mut store, "older");
        store.enforce_global_limit();

        let tombstone = store.sessions.get("older").expect("retained tombstone");
        assert_eq!(tombstone.tool_calls.capacity(), 0);
        assert_eq!(store.retained_bytes, store.recomputed_retained_bytes());
        let older = store.snapshot("older");
        let newer = store.snapshot("newer");
        assert!(older.truncated);
        assert!(older.tool_calls.is_empty());
        assert_eq!(newer.tool_calls.len(), 1);
    }

    #[test]
    fn retained_tombstone_eviction_does_not_mark_new_sessions_truncated() {
        let limits = SessionChangeLimits {
            max_session_bytes: 1_500,
            max_total_bytes: usize::MAX,
            max_tool_calls_per_session: 10,
        };
        let mut store = SessionChangesStore::with_limits("generation", limits);
        store.apply(
            "older",
            &SessionUpdate::ToolCall(tool_call(
                "older-call",
                vec![diff("/workspace/older", Some("a"), &"b".repeat(300))],
            )),
        );
        store.apply(
            "newer",
            &SessionUpdate::ToolCall(tool_call(
                "newer-call",
                vec![diff("/workspace/newer", Some("a"), &"c".repeat(300))],
            )),
        );
        set_limit_to_retain_oldest_tombstone(&mut store, "older");
        store.enforce_global_limit();
        // "older" was evicted down to a tombstone whose session id is still
        // tracked, so sessions the store has never seen keep a clean slate.
        assert!(store.snapshot("older").truncated);

        let fresh = store.snapshot("brand-new");
        assert!(!fresh.truncated);
        assert_eq!(fresh.revision, 0);
        assert!(fresh.tool_calls.is_empty());
    }

    #[test]
    fn metadata_round_trips_compactly_and_is_accounted() {
        let mut meta = Meta::new();
        meta.insert("secret".to_owned(), serde_json::json!("TOKEN=preserved"));
        meta.insert(
            "nested".to_owned(),
            serde_json::json!({"z": [1, {"b": true, "a": "value"}], "a": null}),
        );
        let content = ToolCallContent::Diff(
            Diff::new("/workspace/.env", "TOKEN=new")
                .old_text("TOKEN=old")
                .meta(meta.clone()),
        );
        let mut store = SessionChangesStore::with_limits(
            "generation",
            SessionChangeLimits {
                max_session_bytes: 10_000,
                max_total_bytes: 1_000_000,
                max_tool_calls_per_session: 10,
            },
        );

        store.apply(
            "session",
            &SessionUpdate::ToolCall(tool_call("call", vec![content])),
        );

        let value = serde_json::to_value(store.snapshot("session")).expect("snapshot JSON");
        assert_eq!(value["tool_calls"][0]["content"][0]["oldText"], "TOKEN=old");
        assert_eq!(value["tool_calls"][0]["content"][0]["newText"], "TOKEN=new");
        assert_eq!(
            value["tool_calls"][0]["content"][0]["_meta"],
            serde_json::Value::Object(meta.clone())
        );
        let meta_wire_bytes = serde_json::to_vec(&meta).expect("metadata JSON").len() as u128;
        let bucket = store.sessions.get("session").expect("session bucket");
        let call = bucket.tool_calls.get("call").expect("captured call");
        assert!(call.retained_bytes > meta_wire_bytes);
        assert_eq!(store.retained_bytes, store.recomputed_retained_bytes());
    }

    #[test]
    fn cached_response_wire_size_matches_actual_envelope() {
        let mut meta = Meta::new();
        meta.insert("source".to_owned(), serde_json::json!({"secret": "exact"}));
        let mut store = SessionChangesStore::with_limits(
            "generation",
            SessionChangeLimits {
                max_session_bytes: 10_000,
                max_total_bytes: 1_000_000,
                max_tool_calls_per_session: 10,
            },
        );
        store.apply(
            "session",
            &SessionUpdate::ToolCall(tool_call(
                "call",
                vec![ToolCallContent::Diff(
                    Diff::new("/workspace/quoted file", "new\n\"text\"")
                        .old_text("old\ntext")
                        .meta(meta),
                )],
            )),
        );

        let bucket = store.sessions.get("session").expect("session bucket");
        let empty_bytes = empty_envelope_wire_bytes(
            "session",
            &store.generation,
            bucket.revision,
            bucket.truncated,
        );
        let cached = bucket.response_wire_bytes(empty_bytes);
        let snapshot = SessionChangesSnapshot {
            session_id: "session".to_owned(),
            generation: store.generation.clone(),
            revision: bucket.revision,
            truncated: bucket.truncated,
            tool_calls: bucket.visible_tool_calls(),
        };
        let actual = serde_json::to_vec(&ApiSuccess::new(snapshot))
            .expect("snapshot envelope JSON")
            .len() as u128;
        assert_eq!(cached, actual);
    }

    #[test]
    fn per_session_eviction_removes_one_sorted_batch_and_releases_capacity() {
        let mut store = SessionChangesStore::with_limits(
            "generation",
            SessionChangeLimits {
                max_session_bytes: 10_000,
                max_total_bytes: 1_000_000,
                max_tool_calls_per_session: 10,
            },
        );
        for id in ["first", "second", "third", "fourth", "fifth", "sixth"] {
            store.apply(
                "session",
                &SessionUpdate::ToolCall(tool_call(
                    id,
                    vec![diff(&format!("/workspace/{id}"), Some("before"), "after")],
                )),
            );
        }
        let capacity_before = store
            .sessions
            .get("session")
            .expect("session bucket")
            .tool_calls
            .capacity();
        store.limits.max_tool_calls_per_session = 2;
        store.apply(
            "session",
            &SessionUpdate::ToolCall(tool_call(
                "seventh",
                vec![diff("/workspace/seventh", Some("before"), "after")],
            )),
        );

        let bucket = store.sessions.get("session").expect("session bucket");
        let mut remaining = bucket
            .tool_calls
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>();
        remaining.sort_unstable();
        assert_eq!(remaining, ["seventh", "sixth"]);
        assert!(bucket.truncated);
        assert!(bucket.tool_calls.capacity() < capacity_before);
        assert!(bucket.tool_calls.capacity() <= bucket.tool_calls.len().saturating_mul(2));
        assert_eq!(store.retained_bytes, store.recomputed_retained_bytes());
    }

    #[test]
    fn global_eviction_releases_inner_and_session_table_capacity() {
        let limits = SessionChangeLimits {
            max_session_bytes: 10_000,
            max_total_bytes: usize::MAX,
            max_tool_calls_per_session: 10,
        };
        let mut store = SessionChangesStore::with_limits("generation", limits);
        for index in 0..64 {
            let session_id = format!("session-{index}");
            store.apply(
                &session_id,
                &SessionUpdate::ToolCall(tool_call(
                    "call",
                    vec![diff(
                        &format!("/workspace/{index}"),
                        Some("before"),
                        &"x".repeat(128),
                    )],
                )),
            );
        }
        assert!(store.sessions.capacity() >= store.sessions.len());
        let empty_store = SessionChangesStore::with_limits("generation", limits);
        store.limits.max_total_bytes = usize::try_from(empty_store.retained_bytes + 1)
            .expect("empty retained size fits usize");

        store.enforce_global_limit();

        assert!(store.sessions.is_empty());
        assert_eq!(store.sessions.capacity(), 0);
        assert!(store.capacity_reached);
        assert_eq!(store.retained_bytes, store.recomputed_retained_bytes());
        assert!(store.retained_bytes <= store.limits.max_total_bytes as u128);
    }

    fn set_limit_to_retain_oldest_tombstone(store: &mut SessionChangesStore, session_id: &str) {
        store.sessions.shrink_to_fit();
        store.refresh_structural_retained_bytes();
        let current = store.retained_bytes;
        let existing = store
            .sessions
            .get(session_id)
            .expect("session to evict")
            .retained_bytes;
        let session_key = session_id.to_owned();
        let mut tombstone = SessionChangesBucket {
            truncated: true,
            ..SessionChangesBucket::default()
        };
        tombstone.refresh_cached_sizes(&session_key);
        let target = current
            .saturating_sub(existing)
            .saturating_add(tombstone.retained_bytes);
        store.limits.max_total_bytes = usize::try_from(target).expect("test limit fits usize");
    }
}
