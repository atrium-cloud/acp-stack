use acp_stack::auth::{
    API_KEY_ENTROPY_BYTES, API_KEY_PREFIX, AuthFailureReason, KeyKind, constant_time_eq,
    generate_api_key, record_auth_failure,
};
use acp_stack::state::{AuthFailureFilter, StateStore};
use base64::Engine;

#[test]
fn api_key_format_is_acps_prefix_plus_base64url_no_pad() {
    let key = generate_api_key();
    assert!(key.starts_with(API_KEY_PREFIX));
    let body = &key[API_KEY_PREFIX.len()..];
    assert_eq!(
        body.len(),
        43,
        "32 bytes encoded as base64url-no-pad is 43 chars"
    );
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(body)
        .expect("base64url body decodes");
    assert_eq!(decoded.len(), API_KEY_ENTROPY_BYTES);
}

#[test]
fn generated_keys_are_distinct_across_calls() {
    let a = generate_api_key();
    let b = generate_api_key();
    let c = generate_api_key();
    assert_ne!(a, b);
    assert_ne!(b, c);
    assert_ne!(a, c);
}

#[test]
fn constant_time_eq_returns_true_for_equal_slices() {
    assert!(constant_time_eq(b"same-slice", b"same-slice"));
    assert!(constant_time_eq(b"", b""));
}

#[test]
fn constant_time_eq_returns_false_for_unequal_or_different_length() {
    assert!(!constant_time_eq(b"different", b"slices"));
    assert!(!constant_time_eq(b"short", b"shorter"));
    assert!(!constant_time_eq(b"a", b""));
}

fn open_store() -> (tempfile::TempDir, StateStore) {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state opens");
    store.migrate().expect("migrate");
    (tempdir, store)
}

#[test]
fn record_auth_failure_persists_row_with_kind_and_reason() {
    let (_dir, store) = open_store();
    let failure = record_auth_failure(
        &store,
        KeyKind::Session,
        AuthFailureReason::Invalid,
        Some("127.0.0.1"),
        Some("/v1/sessions"),
    )
    .expect("record");
    assert_eq!(failure.key_kind, "session");
    assert_eq!(failure.reason, "invalid");
    assert_eq!(failure.client_ip.as_deref(), Some("127.0.0.1"));
    assert_eq!(failure.route.as_deref(), Some("/v1/sessions"));
    let payload: serde_json::Value =
        serde_json::from_str(&failure.payload_json).expect("payload parses");
    assert_eq!(payload["key_kind"], "session");
    assert_eq!(payload["reason"], "invalid");

    let rows = store
        .query_auth_failures(AuthFailureFilter { limit: 10 })
        .expect("query");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, failure.id);
}

#[test]
fn record_auth_failure_supports_null_client_and_route() {
    let (_dir, store) = open_store();
    let failure = record_auth_failure(
        &store,
        KeyKind::Admin,
        AuthFailureReason::Missing,
        None,
        None,
    )
    .expect("record");
    assert!(failure.client_ip.is_none());
    assert!(failure.route.is_none());
    assert_eq!(failure.key_kind, "admin");
    assert_eq!(failure.reason, "missing");
}

#[test]
fn auth_failure_rows_query_newest_first() {
    let (_dir, store) = open_store();
    for reason in [
        AuthFailureReason::Missing,
        AuthFailureReason::Invalid,
        AuthFailureReason::WrongKind,
        AuthFailureReason::MalformedHeader,
    ] {
        record_auth_failure(&store, KeyKind::Session, reason, None, None).expect("record");
    }

    let rows = store
        .query_auth_failures(AuthFailureFilter { limit: 10 })
        .expect("query");
    assert_eq!(rows.len(), 4);
    // Lexicographic id order matches chronological order; reverse-iterating
    // gives the appended order.
    assert_eq!(rows[3].reason, "missing");
    assert_eq!(rows[2].reason, "invalid");
    assert_eq!(rows[1].reason, "wrong_kind");
    assert_eq!(rows[0].reason, "malformed_header");
}
