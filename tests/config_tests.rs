use acp_stack::config::{Config, load_config_from_str};

const VALID_CONFIG: &str = include_str!("fixtures/valid-acp-stack.toml");

fn with_codex_adapter(input: &str) -> String {
    input.replace(
        r#"restart = "on-crash""#,
        r#"restart = "on-crash"

[agent.adapter]
id = "codex-acp"
name = "Codex ACP Adapter"
upstream_agent = "codex-cli"
source_url = "https://github.com/zed-industries/codex-acp""#,
    )
}

fn with_registry_install(input: &str) -> String {
    input.replace(
        r#"[agent.install]
type = "shell"
shell = "curl -fsSL https://opencode.ai/install | bash"
creates = "opencode""#,
        r#"[agent.install]
type = "registry"
id = "opencode"
creates = "opencode""#,
    )
}

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
    assert!(round_tripped.agent.adapter.is_none());
    assert!(canonical.contains("[security.http]"));
    assert!(!canonical.contains("[agent.adapter]"));
    assert!(canonical.contains("[agent.install]"));
}

#[test]
fn parses_agent_adapter_metadata_and_exports_canonical_toml() {
    let config = load_config_from_str(&with_codex_adapter(VALID_CONFIG))
        .expect("adapter config should parse");

    let canonical = config
        .to_canonical_toml()
        .expect("canonical TOML should serialize");
    let round_tripped: Config =
        toml::from_str(&canonical).expect("canonical TOML should parse as config");

    assert_eq!(
        round_tripped
            .agent
            .adapter
            .as_ref()
            .expect("adapter metadata")
            .id,
        "codex-acp"
    );
    assert!(canonical.contains("[agent.adapter]"));
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
fn parses_workspace_max_file_bytes() {
    let config = load_config_from_str(VALID_CONFIG).expect("valid config should parse");
    assert_eq!(config.workspace.max_file_bytes, 8_388_608);
}

#[test]
fn rejects_zero_workspace_max_file_bytes() {
    let error = load_config_from_str(
        &VALID_CONFIG.replace("max_file_bytes = 8388608", "max_file_bytes = 0"),
    )
    .expect_err("zero max_file_bytes should fail");

    assert!(
        error
            .to_string()
            .contains("workspace.max_file_bytes must be greater than zero"),
        "got: {error}",
    );
}

#[test]
fn rejects_missing_workspace_max_file_bytes() {
    let error = load_config_from_str(&VALID_CONFIG.replace("max_file_bytes = 8388608\n", ""))
        .expect_err("missing max_file_bytes should fail");

    assert!(error.to_string().contains("max_file_bytes"), "got: {error}",);
}

#[test]
fn rejects_uploads_with_parent_dir_segments() {
    // Lexical starts_with passes for this, but the resolved path escapes.
    let error = load_config_from_str(&VALID_CONFIG.replace(
        r#"uploads = "/workspace/uploads""#,
        r#"uploads = "/workspace/../etc/uploads""#,
    ))
    .expect_err("uploads with `..` should fail");

    assert!(
        error
            .to_string()
            .contains("workspace.uploads must not contain `..` segments"),
        "got: {error}",
    );
}

#[test]
fn rejects_uploads_outside_workspace_root() {
    let error = load_config_from_str(&VALID_CONFIG.replace(
        r#"uploads = "/workspace/uploads""#,
        r#"uploads = "/etc/dropbox""#,
    ))
    .expect_err("uploads outside root should fail");

    assert!(
        error
            .to_string()
            .contains("workspace.uploads must be inside workspace.root"),
        "got: {error}",
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
fn parses_native_agent_without_adapter_metadata() {
    let parsed = load_config_from_str(VALID_CONFIG).expect("native agent config should parse");
    assert!(parsed.agent.adapter.is_none());
}

#[test]
fn rejects_empty_agent_adapter_id() {
    let config = with_codex_adapter(VALID_CONFIG).replace(r#"id = "codex-acp""#, r#"id = """#);
    let error = load_config_from_str(&config).expect_err("empty adapter id should fail");
    assert!(
        error.to_string().contains("agent.adapter.id is required"),
        "{error}"
    );
}

#[test]
fn rejects_empty_agent_adapter_name() {
    let config =
        with_codex_adapter(VALID_CONFIG).replace(r#"name = "Codex ACP Adapter""#, r#"name = """#);
    let error = load_config_from_str(&config).expect_err("empty adapter name should fail");
    assert!(
        error.to_string().contains("agent.adapter.name is required"),
        "{error}"
    );
}

#[test]
fn rejects_empty_agent_adapter_upstream_agent() {
    let config = with_codex_adapter(VALID_CONFIG)
        .replace(r#"upstream_agent = "codex-cli""#, r#"upstream_agent = """#);
    let error = load_config_from_str(&config).expect_err("empty upstream agent should fail");
    assert!(
        error
            .to_string()
            .contains("agent.adapter.upstream_agent is required"),
        "{error}"
    );
}

#[test]
fn rejects_non_http_agent_adapter_source_url() {
    let config = with_codex_adapter(VALID_CONFIG).replace(
        r#"source_url = "https://github.com/zed-industries/codex-acp""#,
        r#"source_url = "git@github.com:zed-industries/codex-acp.git""#,
    );
    let error = load_config_from_str(&config).expect_err("non-http source url should fail");
    assert!(
        error
            .to_string()
            .contains("agent.adapter.source_url must start with http:// or https://"),
        "{error}"
    );
}

#[test]
fn parses_registry_agent_install() {
    let parsed = load_config_from_str(&with_registry_install(VALID_CONFIG))
        .expect("registry install config should parse");
    let install = parsed.agent.install.expect("install config");
    assert_eq!(install.install_type, "registry");
    assert_eq!(install.id.as_deref(), Some("opencode"));
    assert!(install.shell.is_none());
}

#[test]
fn rejects_registry_agent_install_with_shell() {
    let config = with_registry_install(VALID_CONFIG).replace(
        r#"creates = "opencode""#,
        r#"creates = "opencode"
shell = "curl -fsSL https://example.com/install | sh""#,
    );
    let error = load_config_from_str(&config).expect_err("registry shell should fail");
    assert!(
        error
            .to_string()
            .contains("agent.install.shell is not valid when agent.install.type is registry"),
        "{error}"
    );
}

#[test]
fn rejects_registry_agent_install_without_id() {
    let config = with_registry_install(VALID_CONFIG).replace(
        r#"[agent.install]
type = "registry"
id = "opencode"
creates = "opencode""#,
        r#"[agent.install]
type = "registry"
creates = "opencode""#,
    );
    let error = load_config_from_str(&config).expect_err("registry id should be required");
    assert!(
        error.to_string().contains("agent.install.id is required"),
        "{error}"
    );
}

#[test]
fn rejects_shell_agent_install_without_shell() {
    let config = VALID_CONFIG.replace(
        r#"shell = "curl -fsSL https://opencode.ai/install | bash"
"#,
        "",
    );
    let error = load_config_from_str(&config).expect_err("shell install should require shell");
    assert!(
        error
            .to_string()
            .contains("agent.install.shell is required"),
        "{error}"
    );
}

#[test]
fn rejects_non_https_registry_url() {
    let config = with_registry_install(VALID_CONFIG).replace(
        r#"creates = "opencode""#,
        r#"creates = "opencode"
registry_url = "http://localhost:8080/registry.json""#,
    );
    let error = load_config_from_str(&config).expect_err("registry url should require https");
    assert!(
        error
            .to_string()
            .contains("agent.install.registry_url must start with https://"),
        "{error}"
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
            .contains("permissions.timeout_action must be one of"),
        "got: {error}",
    );
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
fn rejects_secret_ref_colliding_with_session_key() {
    let updated = VALID_CONFIG.replace(
        r#"env = ["OPENCODE_API_KEY"]"#,
        r#"env = ["ACP_STACK_SESSION_KEY"]"#,
    );
    let error = load_config_from_str(&updated).expect_err("ref aliasing auth must be rejected");
    assert!(
        error
            .to_string()
            .contains("collides with the configured auth key ref"),
        "got: {error}",
    );
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
fn accepts_dependencies_section() {
    let updated = VALID_CONFIG.replace(
        "[agent]",
        "[dependencies]\ncommands = [{ name = \"git\", required = true }]\n\n[agent]",
    );
    let config = load_config_from_str(&updated).expect("dependencies parse");
    assert_eq!(config.dependencies.commands.len(), 1);
    assert_eq!(config.dependencies.commands[0].name, "git");
    assert!(config.dependencies.commands[0].required);
}

#[test]
fn rejects_duplicate_dependency_names() {
    let updated = VALID_CONFIG.replace(
        "[agent]",
        "[dependencies]\ncommands = [{ name = \"git\" }, { name = \"git\" }]\n\n[agent]",
    );
    let error = load_config_from_str(&updated).expect_err("duplicate must fail");
    assert!(
        error
            .to_string()
            .contains("dependencies.commands contains duplicate"),
        "got: {error}",
    );
}

#[test]
fn accepts_stdio_mcp_server() {
    let updated = VALID_CONFIG.replace(
        "[agent]",
        "[[mcp.servers]]\ntype = \"stdio\"\nname = \"slack\"\ncommand = \"slack-mcp\"\nenv = [\"SLACK_BOT_TOKEN\"]\n\n[agent]",
    );
    let config = load_config_from_str(&updated).expect("stdio mcp parses");
    assert_eq!(config.mcp.servers.len(), 1);
    assert_eq!(config.mcp.servers[0].name(), "slack");
}

#[test]
fn rejects_duplicate_mcp_server_names() {
    let updated = VALID_CONFIG.replace(
        "[agent]",
        "[[mcp.servers]]\ntype = \"stdio\"\nname = \"slack\"\ncommand = \"a\"\n\n[[mcp.servers]]\ntype = \"stdio\"\nname = \"slack\"\ncommand = \"b\"\n\n[agent]",
    );
    let error = load_config_from_str(&updated).expect_err("duplicate names must fail");
    assert!(error.to_string().contains("duplicate name"), "got: {error}",);
}

#[test]
fn rejects_duplicate_mcp_server_names_across_kinds() {
    // Cross-transport name collisions (stdio + http with the same `name`) must
    // also be rejected: the agent identifies servers by name regardless of
    // transport, so allowing duplicates would silently overwrite the first
    // entry's wiring.
    let updated = VALID_CONFIG.replace(
        "[agent]",
        concat!(
            "[[mcp.servers]]\ntype = \"stdio\"\nname = \"shared\"\ncommand = \"a\"\n\n",
            "[[mcp.servers]]\ntype = \"http\"\nname = \"shared\"\nurl = \"https://example/x\"\n\n",
            "[agent]"
        ),
    );
    let error = load_config_from_str(&updated).expect_err("cross-kind duplicates must fail");
    assert!(error.to_string().contains("duplicate name"), "got: {error}",);
}

#[test]
fn rejects_http_mcp_with_bad_url() {
    let updated = VALID_CONFIG.replace(
        "[agent]",
        "[[mcp.servers]]\ntype = \"http\"\nname = \"linear\"\nurl = \"ftp://x\"\n\n[agent]",
    );
    let error = load_config_from_str(&updated).expect_err("bad url must fail");
    assert!(
        error
            .to_string()
            .contains("must start with http:// or https://"),
        "got: {error}",
    );
}
