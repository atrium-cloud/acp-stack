use acp_stack::config::{LocalSessionAuth, load_config_from_str};

mod common;
use common::config::VALID_CONFIG;

#[test]
fn parses_legacy_auth_section_for_migration() {
    let with_auth = VALID_CONFIG.replace(
        "[security.http]",
        r#"[auth]
session_key_ref = "ACP_STACK_SESSION_KEY"
admin_key_ref = "ACP_STACK_ADMIN_KEY"

[security.http]"#,
    );
    let config = load_config_from_str(&with_auth).expect("legacy auth section should parse");
    let canonical = config.to_canonical_toml().expect("canonical toml");
    assert!(!canonical.contains("[auth]"));
    assert!(!canonical.contains("session_key_ref"));
    assert!(!canonical.contains("admin_key_ref"));
}

#[test]
fn rejects_invalid_legacy_auth_ref() {
    let with_auth = VALID_CONFIG.replace(
        "[security.http]",
        r#"[auth]
session_key_ref = "not allowed"
admin_key_ref = "ACP_STACK_ADMIN_KEY"

[security.http]"#,
    );
    let error = load_config_from_str(&with_auth).expect_err("legacy auth ref should validate");
    assert!(
        error.to_string().contains("auth.session_key_ref"),
        "{error}"
    );
}

#[test]
fn canonical_export_omits_auth_section() {
    let config = load_config_from_str(VALID_CONFIG).expect("valid config");
    let canonical = config.to_canonical_toml().expect("canonical toml");
    assert!(!canonical.contains("[auth]"));
    assert!(!canonical.contains("session_key_ref"));
    assert!(!canonical.contains("admin_key_ref"));
}

#[test]
fn allows_secret_ref_named_like_old_session_key_ref() {
    let updated = VALID_CONFIG.replace(
        r#"env = ["OPENCODE_API_KEY"]"#,
        r#"env = ["ACP_STACK_SESSION_KEY"]"#,
    );
    let config = load_config_from_str(&updated).expect("old auth ref names are no longer reserved");
    assert_eq!(config.agent.env, ["ACP_STACK_SESSION_KEY"]);
}

#[test]
fn permissions_timeout_action_defaults_to_deny() {
    let config = load_config_from_str(VALID_CONFIG).expect("valid config");
    assert!(matches!(
        config.permissions.effective_timeout_action(),
        acp_stack::config::PermissionTimeoutAction::Deny
    ));
    assert_eq!(
        config.permissions.effective_request_timeout(),
        std::time::Duration::from_secs(300)
    );
}

#[test]
fn rejects_invalid_permissions_timeout_action() {
    let bad = VALID_CONFIG.replace(
        "[agent]",
        "[permissions]\nmode = \"auto\"\ntimeout_action = \"foo\"\n\n[agent]",
    );
    let error = load_config_from_str(&bad).expect_err("invalid timeout_action must fail");
    assert!(
        error
            .to_string()
            .contains("permissions.timeout_action must be one of deny, approve"),
        "got: {error}",
    );
    // The sentence must also reach remote operators: `/v1/config/validate` and
    // `/v1/config/import` send `public_message`, not `Display`.
    assert_eq!(
        error.public_message(),
        "permissions.timeout_action must be one of deny, approve"
    );
    assert_eq!(error.error_code(), "config.invalid");
}

#[test]
fn rejects_invalid_permissions_request_timeout() {
    let bad = VALID_CONFIG.replace(
        "[agent]",
        "[permissions]\nmode = \"auto\"\nrequest_timeout = \"\"\n\n[agent]",
    );
    let error = load_config_from_str(&bad).expect_err("invalid request_timeout must fail");
    assert!(
        error
            .to_string()
            .contains("permissions.request_timeout must be a duration"),
        "got: {error}",
    );
}

#[test]
fn accepts_explicit_permissions_timeout() {
    let updated = VALID_CONFIG.replace(
        "[agent]",
        "[permissions]\nmode = \"auto\"\nrequest_timeout = \"30s\"\ntimeout_action = \"approve\"\n\n[agent]",
    );
    let config = load_config_from_str(&updated).expect("valid permissions section");
    assert_eq!(
        config.permissions.effective_request_timeout(),
        std::time::Duration::from_secs(30)
    );
    assert!(matches!(
        config.permissions.effective_timeout_action(),
        acp_stack::config::PermissionTimeoutAction::Approve
    ));
    // The stored value must serialize back to its lowercase wire spelling, or
    // a canonical export would no longer reload.
    let canonical = config.to_canonical_toml().expect("canonical toml");
    assert!(
        canonical.contains(r#"timeout_action = "approve""#),
        "canonical toml = {canonical}"
    );
    load_config_from_str(&canonical).expect("canonical toml reloads");
}

#[test]
fn accepts_trusted_proxies() {
    let updated = VALID_CONFIG.replace(
        "trust_proxy_headers = false",
        "trust_proxy_headers = true\ntrusted_proxies = [\"127.0.0.1\", \"10.0.0.1\"]",
    );
    let config = load_config_from_str(&updated).expect("trusted proxies must parse");
    assert_eq!(config.security.http.trusted_proxies.len(), 2);
}

#[test]
fn rejects_invalid_trusted_proxy() {
    let updated = VALID_CONFIG.replace(
        "trust_proxy_headers = false",
        "trust_proxy_headers = true\ntrusted_proxies = [\"not-an-ip\"]",
    );
    let error = load_config_from_str(&updated).expect_err("must reject");
    assert!(
        error
            .to_string()
            .contains("security.http.trusted_proxies entry"),
        "got: {error}",
    );
}

#[test]
fn allows_secret_ref_named_like_old_admin_key_ref() {
    let updated = VALID_CONFIG.replace(
        r#"env = ["OPENCODE_API_KEY"]"#,
        r#"env = ["ACP_STACK_ADMIN_KEY"]"#,
    );
    let config = load_config_from_str(&updated).expect("old auth ref names are no longer reserved");
    assert_eq!(config.agent.env, ["ACP_STACK_ADMIN_KEY"]);
}

#[test]
fn rejects_duplicate_secret_ref_across_categories() {
    let updated = VALID_CONFIG.replace(
        r#"api_key_ref = "SUPABASE_SECRET_KEY""#,
        r#"api_key_ref = "OPENCODE_API_KEY""#,
    );
    let error = load_config_from_str(&updated).expect_err("duplicate refs must be rejected");
    assert!(
        error.to_string().contains("declared more than once"),
        "got: {error}",
    );
}

#[test]
fn rejects_local_socket_path_relative() {
    let input = format!("{VALID_CONFIG}\n[local]\nsocket_path = \"relative/path.sock\"\n");
    let error = load_config_from_str(&input).expect_err("relative path should be rejected");
    assert!(
        error.to_string().contains("local.socket_path") && error.to_string().contains("absolute"),
        "got: {error}"
    );
}

#[test]
fn rejects_local_socket_path_with_dot_dot() {
    let input = format!("{VALID_CONFIG}\n[local]\nsocket_path = \"/tmp/../etc/passwd.sock\"\n");
    let error = load_config_from_str(&input).expect_err("dot dot path should be rejected");
    assert!(
        error.to_string().contains("local.socket_path") && error.to_string().contains(".."),
        "got: {error}"
    );
}

#[test]
fn allows_local_socket_path_absolute() {
    let input = format!("{VALID_CONFIG}\n[local]\nsocket_path = \"/tmp/acps-local.sock\"\n");
    let config = load_config_from_str(&input).expect("absolute path should be accepted");
    assert_eq!(
        config.local.socket_path.as_deref(),
        Some("/tmp/acps-local.sock")
    );
}

#[test]
fn local_session_auth_defaults_to_session_key() {
    let config = load_config_from_str(VALID_CONFIG).expect("valid config");
    assert_eq!(config.local.session_auth, LocalSessionAuth::SessionKey);
    let canonical = config.to_canonical_toml().expect("canonical");
    assert!(!canonical.contains("session_auth"));
}

#[test]
fn local_session_auth_accepts_keyless_and_exports() {
    let input = format!("{VALID_CONFIG}\n[local]\nsession_auth = \"keyless\"\n");
    let config = load_config_from_str(&input).expect("keyless local session auth should parse");
    assert_eq!(config.local.session_auth, LocalSessionAuth::Keyless);
    let canonical = config.to_canonical_toml().expect("canonical");
    assert!(canonical.contains("session_auth = \"keyless\""));
}

#[test]
fn local_session_auth_rejects_invalid_values() {
    let input = format!("{VALID_CONFIG}\n[local]\nsession_auth = \"admin\"\n");
    let error = load_config_from_str(&input).expect_err("invalid local session auth should reject");
    assert!(
        error.to_string().contains("session_auth") && error.to_string().contains("admin"),
        "{error}"
    );
}

#[test]
fn rejects_secret_ref_looking_like_hex_value() {
    let hex_ref = "a".repeat(50);
    assert!(hex_ref.chars().all(|c| c.is_ascii_hexdigit()));
    let input = VALID_CONFIG.replace(
        r#"env = ["OPENCODE_API_KEY"]"#,
        &format!(r#"env = ["{hex_ref}"]"#),
    );
    let error = load_config_from_str(&input).expect_err("hex-only secret ref should be rejected");
    assert!(
        error
            .to_string()
            .contains("looks like an inline secret value"),
        "got: {error}"
    );
}

#[test]
fn rejects_secret_ref_longer_than_128_chars() {
    let long_ref = format!("A{}", "B".repeat(128));
    let input = VALID_CONFIG.replace(
        r#"env = ["OPENCODE_API_KEY"]"#,
        &format!(r#"env = ["{long_ref}"]"#),
    );
    let error = load_config_from_str(&input).expect_err("very long secret ref should be rejected");
    assert!(
        error
            .to_string()
            .contains("looks like an inline secret value"),
        "got: {error}"
    );
}

#[test]
fn allows_normal_secret_ref_like_opencode_api_key() {
    let config = load_config_from_str(VALID_CONFIG).expect("OPENCODE_API_KEY should be allowed");
    assert_eq!(config.agent.env, vec!["OPENCODE_API_KEY"]);
}

#[test]
fn rejects_secret_ref_with_known_token_prefix() {
    let token_ref = "sk-proj-exampleinlinevalue";
    let input = VALID_CONFIG.replace(
        r#"env = ["OPENCODE_API_KEY"]"#,
        &format!(r#"env = ["{token_ref}"]"#),
    );
    let error = load_config_from_str(&input).expect_err("inline token ref should be rejected");
    assert!(
        error
            .to_string()
            .contains("looks like an inline secret value"),
        "got: {error}"
    );
}

#[test]
fn rejects_secret_ref_looking_like_jwt_value() {
    let jwt_ref = "aaaaaaaaaa.bbbbbbbbbb.cccccccccc";
    let input = VALID_CONFIG.replace(
        r#"env = ["OPENCODE_API_KEY"]"#,
        &format!(r#"env = ["{jwt_ref}"]"#),
    );
    let error = load_config_from_str(&input).expect_err("JWT-shaped ref should be rejected");
    assert!(
        error
            .to_string()
            .contains("looks like an inline secret value"),
        "got: {error}"
    );
}
