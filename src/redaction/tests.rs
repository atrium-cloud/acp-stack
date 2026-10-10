use super::*;
use serde_json::json;

#[test]
fn registry_replaces_overlapping_values_longest_first() {
    register_secret_values(["RegShort-7f3aQ", "RegShort-7f3aQ-tail99"]);
    let text = "a RegShort-7f3aQ-tail99 b RegShort-7f3aQ c";
    assert_eq!(redact_text(text), "a [redacted] b [redacted] c");
}

#[test]
fn registry_skips_values_below_the_minimum_length() {
    register_secret_values(["q7Zr5", "w8Yt4k"]);
    assert_eq!(redact_text("q7Zr5 w8Yt4k"), "q7Zr5 [redacted]");
}

#[test]
fn every_credential_prefix_is_redacted() {
    for entry in CREDENTIAL_PREFIXES {
        let token = format!("{}AbCdEf123456", entry.prefix);
        let text = format!("key is {token} ok");
        assert_eq!(
            redact_text(&text),
            "key is [redacted] ok",
            "{}",
            entry.prefix
        );
        let upper = format!("key is {} ok", token.to_ascii_uppercase());
        assert_eq!(
            redact_text(&upper),
            "key is [redacted] ok",
            "{}",
            entry.prefix
        );
    }
}

#[test]
fn credential_prefix_needs_a_credential_sized_body() {
    assert_eq!(
        redact_text("installed sk-learn ok"),
        "installed sk-learn ok"
    );
}

#[test]
fn credential_tokens_are_found_between_separators() {
    assert_eq!(
        redact_text("url=https://host/v1/sk-AbCdEf123456?x=1"),
        "url=https://host/v1/[redacted]?x=1"
    );
    assert_eq!(
        redact_text(r#"{\"key\":\"ghp_AbCdEf1234567890\"}"#),
        r#"{\"key\":\"[redacted]\"}"#
    );
}

#[test]
fn jwt_shaped_tokens_are_redacted_without_a_trailing_period() {
    let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV";
    assert_eq!(redact_text(&format!("token {jwt}.")), "token [redacted].");
}

#[test]
fn bearer_and_basic_credentials_are_redacted_after_the_scheme() {
    assert_eq!(
        redact_text("Authorization: Bearer abc.def/ghi=="),
        "Authorization: Bearer [redacted]"
    );
    assert_eq!(
        redact_text("sent bearer opaqueValue42 upstream"),
        "sent bearer [redacted] upstream"
    );
    assert_eq!(redact_text("invalid bearer token"), "invalid bearer token");
    assert_eq!(
        redact_text("Authorization: Bearer abc"),
        "Authorization: Bearer abc"
    );
    assert_eq!(
        redact_text("proxy-authorization: Basic dXNlcjpwYXNz"),
        "proxy-authorization: Basic [redacted]"
    );
}

#[test]
fn sensitive_name_value_pairs_are_redacted() {
    assert_eq!(
        redact_text(r#"{"x-api-key": "plainvalue123", "model": "gpt"}"#),
        r#"{"x-api-key": "[redacted]", "model": "gpt"}"#
    );
    assert_eq!(
        redact_text("OPENAI_API_KEY=plainvalue123 other=1"),
        "OPENAI_API_KEY=[redacted] other=1"
    );
    assert_eq!(
        redact_text("--client-secret=abc/def+ghi="),
        "--client-secret=[redacted]"
    );
}

#[test]
fn escaped_json_values_are_redacted() {
    assert_eq!(
        redact_text(r#"{\"api_key\":\"plainvalue123\",\"model\":\"gpt\"}"#),
        r#"{\"api_key\":\"[redacted]\",\"model\":\"gpt\"}"#
    );
}

#[test]
fn header_values_run_to_the_end_of_the_line() {
    assert_eq!(
        redact_text("Cookie: a=sess1; b=sess2\nHost: example.test"),
        "Cookie: [redacted]\nHost: example.test"
    );
    assert_eq!(
        redact_text("set-cookie: id=abc; Path=/; HttpOnly  "),
        "set-cookie: [redacted]  "
    );
}

#[test]
fn quoted_values_run_to_the_closing_quote() {
    assert_eq!(
        redact_text(r#"password: "two words" next"#),
        r#"password: "[redacted]" next"#
    );
    assert_eq!(
        redact_text("client_secret='a b c' rest"),
        "client_secret='[redacted]' rest"
    );
    assert_eq!(
        redact_text(r#"{"authorization": "Bearer abc def ghi jkl"}"#),
        r#"{"authorization": "Bearer [redacted]"}"#
    );
    assert_eq!(redact_text(r#"password: "" next"#), r#"password: "" next"#);
}

#[test]
fn digests_and_ordinary_diagnostics_are_kept() {
    let kept = [
        "sha256 9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
        "commit 2dd266d1c0a5e2b7f0a4e6c3d9b8a7f6e5d4c3b2",
        "max_tokens: 1000",
        "missing key: model",
        "login: failed for user",
        "version 1.2.3 of pkg",
        "token: unexpected end of input",
    ];
    for text in kept {
        assert_eq!(redact_text(text), text);
        assert!(matches!(redact_text(text), Cow::Borrowed(_)));
    }
}

#[test]
fn json_string_leaves_are_redacted_and_keys_kept() {
    register_secret_values(["Js0nLeafSecret"]);
    let mut value = json!({
        "sk-AbCdEf123456": "plain",
        "nested": ["ok", "Js0nLeafSecret", {"deep": "carries sk-AbCdEf123456"}],
        "count": 3,
    });
    redact_json(&mut value);
    assert_eq!(
        value,
        json!({
            "sk-AbCdEf123456": "plain",
            "nested": ["ok", "[redacted]", {"deep": "carries [redacted]"}],
            "count": 3,
        })
    );
}

#[test]
fn bounded_cuts_on_a_char_boundary() {
    assert!(matches!(bounded("short", 16), Cow::Borrowed("short")));
    assert_eq!(bounded("ééé", 3), "é [truncated 4 bytes]");
    assert_eq!(bounded("abcdef", 4), "abcd [truncated 2 bytes]");
}

#[test]
fn redact_then_bound_leaves_no_secret_prefix() {
    register_secret_values(["BoundSecret-91x7Q2"]);
    let text = "header BoundSecret-91x7Q2 trailer";
    let redacted = redact_text(text);
    let cut = bounded(&redacted, 12);
    assert!(!cut.contains("BoundSec"), "{cut}");
    assert!(cut.starts_with("header [reda"), "{cut}");
}

#[test]
fn screen_subsets_of_the_prefix_table_are_pinned() {
    let config: Vec<&str> = CREDENTIAL_PREFIXES
        .iter()
        .filter(|entry| entry.config_screened)
        .map(|entry| entry.prefix)
        .collect();
    assert_eq!(
        config,
        [
            "acps_",
            "sk-",
            "ghp_",
            "github_pat_",
            "xoxb-",
            "xoxp-",
            "xoxa-"
        ]
    );
    let native_import: Vec<&str> = CREDENTIAL_PREFIXES
        .iter()
        .filter(|entry| entry.native_import_screened)
        .map(|entry| entry.prefix)
        .collect();
    assert_eq!(
        native_import,
        [
            "sk-",
            "pk-",
            "rk-",
            "ghp_",
            "gho_",
            "ghu_",
            "ghs_",
            "ghr_",
            "github_pat_",
            "glpat-",
            "xoxb-",
            "xoxp-",
            "xoxa-",
            "xoxs-",
        ]
    );
}

#[test]
fn redact_values_scrubs_full_and_straddling_secret_values() {
    let secrets = vec![
        "sk-supersecretkey-ABCDEF".to_owned(),
        "on".to_owned(),
        "https://api.example.test/v1".to_owned(),
    ];

    let mut text = "I wrote the key sk-supersecretkey-ABCDEF to the file.".to_owned();
    redact_values(&mut text, &secrets, false);
    assert!(!text.contains("sk-supersecretkey-ABCDEF"));
    assert!(text.contains(REDACTION_PLACEHOLDER));

    let mut short = "mode is on now".to_owned();
    redact_values(&mut short, &secrets, true);
    assert_eq!(short, "mode is on now");

    let mut straddled = "retkey-ABCDEF was the tail".to_owned();
    redact_values(&mut straddled, &secrets, true);
    assert!(straddled.starts_with(REDACTION_PLACEHOLDER));
    assert!(straddled.ends_with(" was the tail"));
    assert!(!straddled.contains("retkey-ABCDEF"));

    let mut untruncated = "retkey-ABCDEF was the tail".to_owned();
    redact_values(&mut untruncated, &secrets, false);
    assert_eq!(untruncated, "retkey-ABCDEF was the tail");

    let mut clean = "created report.txt with the requested summary".to_owned();
    redact_values(&mut clean, &secrets, true);
    assert_eq!(clean, "created report.txt with the requested summary");
}

#[test]
fn redact_values_replaces_a_nesting_value_before_the_nested_one() {
    let secrets = vec![
        "inner-value".to_owned(),
        "prefix-inner-value-tail".to_owned(),
    ];

    let mut text = "token prefix-inner-value-tail leaked".to_owned();
    redact_values(&mut text, &secrets, false);

    assert_eq!(text, format!("token {REDACTION_PLACEHOLDER} leaked"));
}
