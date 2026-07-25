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
    let mut store = SessionChangesStore::with_limits("generation", SessionChangeLimits::default());
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
    let mut store = SessionChangesStore::with_limits("generation", SessionChangeLimits::default());
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
    let mut store = SessionChangesStore::with_limits("generation", SessionChangeLimits::default());
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
    let mut store = SessionChangesStore::with_limits("generation", SessionChangeLimits::default());
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
    let mut store = SessionChangesStore::with_limits("generation", SessionChangeLimits::default());
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
    let mut store = SessionChangesStore::with_limits("generation", SessionChangeLimits::default());
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
    store.limits.max_total_bytes =
        usize::try_from(empty_store.retained_bytes + 1).expect("empty retained size fits usize");

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
