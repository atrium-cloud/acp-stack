use acp_stack::config::{Config, load_config_from_str};

const VALID_CONFIG: &str = include_str!("fixtures/valid-acp-stack.toml");

#[test]
fn parses_valid_config_and_exports_canonical_toml() {
    let config = load_config_from_str(VALID_CONFIG).expect("valid config should parse");

    assert_eq!(config.api.bind, "127.0.0.1:7700");
    assert_eq!(config.workspace.root, "/workspace");
    assert_eq!(config.agent.restart, "on-crash");

    let canonical = config
        .to_canonical_toml()
        .expect("canonical TOML should serialize");
    let round_tripped: Config =
        toml::from_str(&canonical).expect("canonical TOML should parse as config");

    assert_eq!(round_tripped.agent.id, "opencode");
    assert!(canonical.contains("[security.http]"));
    assert!(canonical.contains("[agent.install]"));
}

#[test]
fn rejects_malformed_toml() {
    let error = load_config_from_str("[api]\nbind = ").expect_err("config should be invalid");

    assert!(
        error.to_string().contains("config TOML is invalid"),
        "{error}"
    );
}

#[test]
fn rejects_missing_required_sections() {
    let error = load_config_from_str("").expect_err("config should be invalid");

    assert!(error.to_string().contains("missing required section"));
}

#[test]
fn rejects_bad_bind_address() {
    let error = load_config_from_str(
        &VALID_CONFIG.replace(r#"bind = "127.0.0.1:7700""#, r#"bind = "not a socket""#),
    )
    .expect_err("config should be invalid");

    assert!(
        error
            .to_string()
            .contains("api.bind must be a socket address")
    );
}

#[test]
fn rejects_relative_workspace_paths() {
    let error = load_config_from_str(
        &VALID_CONFIG.replace(r#"root = "/workspace""#, r#"root = "workspace""#),
    )
    .expect_err("config should be invalid");

    assert!(
        error
            .to_string()
            .contains("workspace.root must be absolute")
    );
}

#[test]
fn rejects_relative_workspace_default_shell() {
    let error = load_config_from_str(&VALID_CONFIG.replace(
        r#"default_shell = "/bin/bash""#,
        r#"default_shell = "bash""#,
    ))
    .expect_err("config should be invalid");

    assert!(
        error
            .to_string()
            .contains("workspace.default_shell must be absolute")
    );
}

#[test]
fn rejects_relative_workspace_source_dest() {
    let config = VALID_CONFIG.replace(
        r#"[workspace.source]
type = "none""#,
        r#"[workspace.source]
type = "git"
repo = "https://github.com/example/project.git"
branch = "main"
dest = "project""#,
    );
    let error = load_config_from_str(&config).expect_err("config should be invalid");

    assert!(
        error
            .to_string()
            .contains("workspace.source.dest must be absolute")
    );
}

#[test]
fn rejects_missing_workspace_source() {
    let config = VALID_CONFIG.replace(
        r#"
[workspace.source]
type = "none""#,
        "",
    );
    let error = load_config_from_str(&config).expect_err("config should be invalid");

    assert!(
        error
            .to_string()
            .contains("missing required section `workspace.source`")
    );
}

#[test]
fn rejects_invalid_workspace_source_type() {
    let error = load_config_from_str(&VALID_CONFIG.replace(r#"type = "none""#, r#"type = "ftp""#))
        .expect_err("config should be invalid");

    assert!(
        error
            .to_string()
            .contains("workspace.source.type must be one of none, git, s3")
    );
}

#[test]
fn rejects_none_workspace_source_with_git_fields() {
    let config = VALID_CONFIG.replace(
        r#"[workspace.source]
type = "none""#,
        r#"[workspace.source]
type = "none"
repo = "https://github.com/example/project.git""#,
    );
    let error = load_config_from_str(&config).expect_err("config should be invalid");

    assert!(
        error
            .to_string()
            .contains("workspace.source.repo is not valid when workspace.source.type is none")
    );
}

#[test]
fn rejects_git_workspace_source_without_repo() {
    let config = VALID_CONFIG.replace(
        r#"[workspace.source]
type = "none""#,
        r#"[workspace.source]
type = "git"
dest = "/workspace/project""#,
    );
    let error = load_config_from_str(&config).expect_err("config should be invalid");

    assert!(
        error
            .to_string()
            .contains("workspace.source.repo is required")
    );
}

#[test]
fn rejects_git_workspace_source_with_s3_fields() {
    let config = VALID_CONFIG.replace(
        r#"[workspace.source]
type = "none""#,
        r#"[workspace.source]
type = "git"
repo = "https://github.com/example/project.git"
dest = "/workspace/project"
bucket = "data""#,
    );
    let error = load_config_from_str(&config).expect_err("config should be invalid");

    assert!(
        error
            .to_string()
            .contains("workspace.source.bucket is not valid when workspace.source.type is git")
    );
}

#[test]
fn rejects_s3_workspace_source_without_bucket() {
    let config = VALID_CONFIG.replace(
        r#"[workspace.source]
type = "none""#,
        r#"[workspace.source]
type = "s3"
dest = "/workspace/data"
access_key_ref = "AWS_ACCESS_KEY_ID"
secret_key_ref = "AWS_SECRET_ACCESS_KEY"
region = "us-east-1""#,
    );
    let error = load_config_from_str(&config).expect_err("config should be invalid");

    assert!(
        error
            .to_string()
            .contains("workspace.source.bucket is required")
    );
}

#[test]
fn rejects_s3_workspace_source_with_git_fields() {
    let config = VALID_CONFIG.replace(
        r#"[workspace.source]
type = "none""#,
        r#"[workspace.source]
type = "s3"
bucket = "data"
dest = "/workspace/data"
access_key_ref = "AWS_ACCESS_KEY_ID"
secret_key_ref = "AWS_SECRET_ACCESS_KEY"
region = "us-east-1"
repo = "https://github.com/example/project.git""#,
    );
    let error = load_config_from_str(&config).expect_err("config should be invalid");

    assert!(
        error
            .to_string()
            .contains("workspace.source.repo is not valid when workspace.source.type is s3")
    );
}

#[test]
fn rejects_unknown_config_fields() {
    let config = VALID_CONFIG.replace(
        r#"root = "/workspace""#,
        r#"root = "/workspace"
roooot = "/typo""#,
    );
    let error = load_config_from_str(&config).expect_err("config should be invalid");

    assert!(error.to_string().contains("unknown field"));
}

#[test]
fn rejects_invalid_agent_restart_policy() {
    let error = load_config_from_str(
        &VALID_CONFIG.replace(r#"restart = "on-crash""#, r#"restart = "always""#),
    )
    .expect_err("config should be invalid");

    assert!(
        error
            .to_string()
            .contains("agent.restart must be one of never, on-crash")
    );
}

#[test]
fn rejects_empty_expected_sha256() {
    let config = VALID_CONFIG.replace(
        r#"restart = "on-crash""#,
        "expected_sha256 = \"\"\nrestart = \"on-crash\"",
    );

    let error = load_config_from_str(&config).expect_err("empty expected_sha256 should fail");

    assert!(
        error
            .to_string()
            .contains("agent.expected_sha256 must be exactly 64 lowercase hex characters")
    );
}

#[test]
fn rejects_uppercase_expected_sha256() {
    let valid_hash = "a".repeat(64);
    let upper_hash = "A".repeat(64);
    let config = VALID_CONFIG.replace(
        r#"restart = "on-crash""#,
        &format!("expected_sha256 = \"{upper_hash}\"\nrestart = \"on-crash\""),
    );

    let error = load_config_from_str(&config).expect_err("uppercase hex should fail");
    assert!(
        error
            .to_string()
            .contains("agent.expected_sha256 must be exactly 64 lowercase hex characters")
    );

    // sanity: lowercase form parses fine
    let ok = VALID_CONFIG.replace(
        r#"restart = "on-crash""#,
        &format!("expected_sha256 = \"{valid_hash}\"\nrestart = \"on-crash\""),
    );
    let parsed = load_config_from_str(&ok).expect("lowercase 64-hex should parse");
    assert_eq!(
        parsed.agent.expected_sha256.as_deref(),
        Some(valid_hash.as_str())
    );
}

#[test]
fn rejects_non_hex_expected_sha256() {
    let bad = "z".repeat(64);
    let config = VALID_CONFIG.replace(
        r#"restart = "on-crash""#,
        &format!("expected_sha256 = \"{bad}\"\nrestart = \"on-crash\""),
    );

    let error = load_config_from_str(&config).expect_err("non-hex chars should fail");
    assert!(
        error
            .to_string()
            .contains("agent.expected_sha256 must be exactly 64 lowercase hex characters")
    );
}

#[test]
fn rejects_short_expected_sha256() {
    let short = "a".repeat(63);
    let config = VALID_CONFIG.replace(
        r#"restart = "on-crash""#,
        &format!("expected_sha256 = \"{short}\"\nrestart = \"on-crash\""),
    );

    let error = load_config_from_str(&config).expect_err("63-char hex should fail");
    assert!(
        error
            .to_string()
            .contains("agent.expected_sha256 must be exactly 64 lowercase hex characters")
    );
}

#[test]
fn rejects_aliased_auth_refs() {
    // session and admin key references must be distinct; aliasing collapses
    // the session/admin boundary because regenerate-session-key would also
    // rotate whatever is stored under the admin name.
    let aliased = VALID_CONFIG.replace(
        r#"admin_key_ref = "ACP_STACK_ADMIN_KEY""#,
        r#"admin_key_ref = "ACP_STACK_SESSION_KEY""#,
    );
    let error = load_config_from_str(&aliased).expect_err("aliased refs must be rejected");
    assert!(
        error
            .to_string()
            .contains("auth.session_key_ref and auth.admin_key_ref must be different names"),
        "got: {error}",
    );
}

#[test]
fn rejects_empty_auth_session_key_ref() {
    let blank = VALID_CONFIG.replace(
        r#"session_key_ref = "ACP_STACK_SESSION_KEY""#,
        r#"session_key_ref = """#,
    );
    let error = load_config_from_str(&blank).expect_err("empty session ref must be rejected");
    assert!(
        error
            .to_string()
            .contains("auth.session_key_ref is required"),
        "got: {error}",
    );
}

#[test]
fn rejects_empty_auth_admin_key_ref() {
    let blank = VALID_CONFIG.replace(
        r#"admin_key_ref = "ACP_STACK_ADMIN_KEY""#,
        r#"admin_key_ref = """#,
    );
    let error = load_config_from_str(&blank).expect_err("empty admin ref must be rejected");
    assert!(
        error.to_string().contains("auth.admin_key_ref is required"),
        "got: {error}",
    );
}
