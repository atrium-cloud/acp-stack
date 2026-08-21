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

/// ACP `_meta` passed through unmodified; the shape is the agent's, not ours.
#[derive(Clone, Debug, schemars::JsonSchema)]
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

#[derive(Clone, Debug, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum CapturedToolCallContent {
    /// `oldText`, `newText`, and `_meta` mirror ACP's `ToolCallContent::Diff`
    /// wire shape verbatim; the camelCase spelling is intentional fidelity to
    /// the protocol, not drift from this API's snake_case convention.
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

#[derive(Clone, Debug, Serialize, schemars::JsonSchema)]
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

#[derive(Clone, Debug, PartialEq, Serialize, schemars::JsonSchema)]
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
mod tests;
