use acp_stack::config::load_config_from_str;

mod common;
use common::config::VALID_CONFIG;

#[test]
fn parses_generated_cloudflare_edge_config() {
    let config_text = VALID_CONFIG.replace(
        "[workspace]",
        r#"[edge.cloudflare]
enabled = true
mode = "generated"
exposure = "tunnel"
hostname = "agent.example.com"
tunnel_name = "acp-stack"
cloudflared_deployment = "host"

[workspace]"#,
    );
    let config = load_config_from_str(&config_text).expect("cloudflare config should parse");
    let cloudflare = config.edge.cloudflare.as_ref().expect("cloudflare block");
    assert!(cloudflare.enabled);
    assert_eq!(cloudflare.hostname, "agent.example.com");

    let canonical = config.to_canonical_toml().expect("canonical");
    assert!(canonical.contains("[edge.cloudflare]"));
    assert!(canonical.contains("cloudflared_deployment = \"host\""));
}

#[test]
fn parses_managed_cloudflare_edge_config() {
    let config_text = VALID_CONFIG.replace(
        "[workspace]",
        r#"[edge.cloudflare]
enabled = true
mode = "managed"
exposure = "tunnel"
hostname = "agent.example.com"
api_token_ref = "CLOUDFLARE_API_TOKEN"
account_id_ref = "CLOUDFLARE_ACCOUNT_ID"

[workspace]"#,
    );
    let config = load_config_from_str(&config_text).expect("managed mode should parse");
    let cloudflare = config.edge.cloudflare.as_ref().expect("cloudflare block");
    assert_eq!(cloudflare.mode, "managed");
    assert_eq!(
        cloudflare.api_token_ref.as_deref(),
        Some("CLOUDFLARE_API_TOKEN")
    );
    assert_eq!(
        cloudflare.account_id_ref.as_deref(),
        Some("CLOUDFLARE_ACCOUNT_ID")
    );
}

#[test]
fn rejects_managed_cloudflare_without_credential_refs() {
    let config_text = VALID_CONFIG.replace(
        "[workspace]",
        r#"[edge.cloudflare]
enabled = true
mode = "managed"
exposure = "tunnel"
hostname = "agent.example.com"

[workspace]"#,
    );
    let error = load_config_from_str(&config_text).expect_err("managed mode needs refs");
    assert!(error.to_string().contains("api_token_ref"), "got: {error}");

    let config_text = VALID_CONFIG.replace(
        "[workspace]",
        r#"[edge.cloudflare]
enabled = true
mode = "managed"
exposure = "tunnel"
hostname = "agent.example.com"
api_token_ref = "CLOUDFLARE_API_TOKEN"

[workspace]"#,
    );
    let error = load_config_from_str(&config_text).expect_err("managed mode needs account ref");
    assert!(error.to_string().contains("account_id_ref"), "got: {error}");
}

#[test]
fn rejects_invalid_managed_cloudflare_credential_refs() {
    let config_text = VALID_CONFIG.replace(
        "[workspace]",
        r#"[edge.cloudflare]
enabled = true
mode = "managed"
exposure = "tunnel"
hostname = "agent.example.com"
api_token_ref = "sk-proj-exampleinlinevalue"
account_id_ref = "CLOUDFLARE_ACCOUNT_ID"

[workspace]"#,
    );
    let error = load_config_from_str(&config_text).expect_err("managed mode rejects inline token");
    assert!(error.to_string().contains("api_token_ref"), "got: {error}");

    let config_text = VALID_CONFIG.replace(
        "[workspace]",
        r#"[edge.cloudflare]
enabled = true
mode = "managed"
exposure = "tunnel"
hostname = "agent.example.com"
api_token_ref = "CLOUDFLARE_API_TOKEN"
account_id_ref = "bad ref"

[workspace]"#,
    );
    let error =
        load_config_from_str(&config_text).expect_err("managed mode rejects invalid account ref");
    assert!(error.to_string().contains("account_id_ref"), "got: {error}");
}

#[test]
fn rejects_invalid_cloudflare_hostname_and_deployment() {
    let bad_hostname = VALID_CONFIG.replace(
        "[workspace]",
        r#"[edge.cloudflare]
enabled = true
mode = "generated"
exposure = "tunnel"
hostname = "https://agent.example.com"

[workspace]"#,
    );
    let error = load_config_from_str(&bad_hostname).expect_err("hostname should be rejected");
    assert!(error.to_string().contains("bare hostname"), "got: {error}");

    let bad_deployment = VALID_CONFIG.replace(
        "[workspace]",
        r#"[edge.cloudflare]
enabled = true
mode = "generated"
exposure = "tunnel"
hostname = "agent.example.com"
cloudflared_deployment = "sidecar"

[workspace]"#,
    );
    let error = load_config_from_str(&bad_deployment).expect_err("deployment should be rejected");
    assert!(
        error.to_string().contains("cloudflared_deployment"),
        "got: {error}"
    );
}

#[test]
fn rejects_unsafe_cloudflare_tunnel_artifact_identifiers() {
    let bad_tunnel_name = VALID_CONFIG.replace(
        "[workspace]",
        r#"[edge.cloudflare]
enabled = true
mode = "generated"
exposure = "tunnel"
hostname = "agent.example.com"
tunnel_name = "bad\nname"

[workspace]"#,
    );
    let error =
        load_config_from_str(&bad_tunnel_name).expect_err("unsafe tunnel name should be rejected");
    assert!(error.to_string().contains("tunnel_name"), "got: {error}");

    let bad_tunnel_id = VALID_CONFIG.replace(
        "[workspace]",
        r#"[edge.cloudflare]
enabled = true
mode = "generated"
exposure = "tunnel"
hostname = "agent.example.com"
tunnel_id = "../credentials"

[workspace]"#,
    );
    let error =
        load_config_from_str(&bad_tunnel_id).expect_err("unsafe tunnel id should be rejected");
    assert!(error.to_string().contains("tunnel_id"), "got: {error}");
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
fn accepts_empty_workspace_sources() {
    // The default starter config declares no code or data sources; loading
    // must succeed because Phase 4 lanes are optional.
    load_config_from_str(VALID_CONFIG).expect("starter config without sources should load");
}

#[test]
fn accepts_git_code_source() {
    let config = VALID_CONFIG.replace(
        "[logging]",
        r#"[[workspace.code_sources]]
type = "git"
repo = "https://github.com/example/project.git"
branch = "main"

[logging]"#,
    );
    load_config_from_str(&config).expect("git code source should validate");
}

#[test]
fn rejects_unknown_code_source_type() {
    let config = VALID_CONFIG.replace(
        "[logging]",
        r#"[[workspace.code_sources]]
type = "svn"
repo = "https://svn.example.com/trunk"

[logging]"#,
    );
    let error = load_config_from_str(&config).expect_err("unknown code-source type rejected");
    assert!(
        error
            .to_string()
            .contains("workspace.code_sources[0]: type must be `git`"),
        "error was: {error}"
    );
}

#[test]
fn rejects_code_source_without_repo() {
    let config = VALID_CONFIG.replace(
        "[logging]",
        r#"[[workspace.code_sources]]
type = "git"

[logging]"#,
    );
    let error = load_config_from_str(&config).expect_err("missing repo rejected");
    assert!(
        error.to_string().contains("workspace.code_sources[0]"),
        "error was: {error}"
    );
}

#[test]
fn rejects_code_source_with_unsupported_scheme() {
    let config = VALID_CONFIG.replace(
        "[logging]",
        r#"[[workspace.code_sources]]
type = "git"
repo = "ftp://example.com/project.git"

[logging]"#,
    );
    let error = load_config_from_str(&config).expect_err("ftp scheme rejected");
    assert!(
        error.to_string().contains("workspace.code_sources[0]"),
        "error was: {error}"
    );
}

#[test]
fn rejects_duplicate_code_source_destinations() {
    let config = VALID_CONFIG.replace(
        "[logging]",
        r#"[[workspace.code_sources]]
type = "git"
repo = "https://github.com/example/project.git"

[[workspace.code_sources]]
type = "git"
repo = "https://github.com/another/project.git"

[logging]"#,
    );
    let error = load_config_from_str(&config).expect_err("duplicate names rejected");
    assert!(
        error.to_string().contains("duplicate destination name"),
        "error was: {error}"
    );
}

#[test]
fn accepts_https_data_source() {
    let config = VALID_CONFIG.replace(
        "[logging]",
        r#"[[workspace.data_sources]]
type = "https"
url = "https://example.com/dataset.tar.gz"

[logging]"#,
    );
    load_config_from_str(&config).expect("https data source should validate");
}

#[test]
fn rejects_http_data_source() {
    let config = VALID_CONFIG.replace(
        "[logging]",
        r#"[[workspace.data_sources]]
type = "https"
url = "http://example.com/dataset.tar.gz"

[logging]"#,
    );
    let error = load_config_from_str(&config).expect_err("http rejected");
    assert!(
        error
            .to_string()
            .contains("workspace.data_sources[0]: url must start with https://"),
        "error was: {error}"
    );
}

#[test]
fn rejects_data_source_with_mixed_fields() {
    let config = VALID_CONFIG.replace(
        "[logging]",
        r#"[[workspace.data_sources]]
type = "local"
path = "/srv/example/data"
bucket = "extra"

[logging]"#,
    );
    let error = load_config_from_str(&config).expect_err("mixed fields rejected");
    assert!(
        error
            .to_string()
            .contains("workspace.data_sources[0]: bucket is not valid when type is local"),
        "error was: {error}"
    );
}

#[test]
fn rejects_relative_local_data_source_path() {
    let config = VALID_CONFIG.replace(
        "[logging]",
        r#"[[workspace.data_sources]]
type = "local"
path = "relative/path"

[logging]"#,
    );
    let error = load_config_from_str(&config).expect_err("relative path rejected");
    assert!(
        error
            .to_string()
            .contains("workspace.data_sources[0]: path `relative/path` must be absolute"),
        "error was: {error}"
    );
}

#[test]
fn rejects_s3_data_source_without_credentials() {
    let config = VALID_CONFIG.replace(
        "[logging]",
        r#"[[workspace.data_sources]]
type = "s3"
bucket = "example"
region = "us-east-1"

[logging]"#,
    );
    let error = load_config_from_str(&config).expect_err("missing creds rejected");
    assert!(
        error
            .to_string()
            .contains("workspace.data_sources[0]: access_key_ref is required"),
        "error was: {error}"
    );
}

#[test]
fn rejects_local_data_source_with_download_cap() {
    let config = VALID_CONFIG.replace(
        "[logging]",
        r#"[[workspace.data_sources]]
type = "local"
path = "/srv/example/data"
max_download_bytes = 1048576

[logging]"#,
    );
    let error = load_config_from_str(&config).expect_err("cap not valid for local");
    assert!(
        error
            .to_string()
            .contains("max_download_bytes is not valid when type is local"),
        "error was: {error}"
    );
}

#[test]
fn rejects_zero_max_download_bytes_on_https() {
    let config = VALID_CONFIG.replace(
        "[logging]",
        r#"[[workspace.data_sources]]
type = "https"
url = "https://example.com/dataset.tar.gz"
max_download_bytes = 0

[logging]"#,
    );
    let error = load_config_from_str(&config).expect_err("zero cap rejected");
    assert!(
        error
            .to_string()
            .contains("max_download_bytes must be greater than zero"),
        "error was: {error}"
    );
}

#[test]
fn rejects_s3_data_source_with_extracted_cap() {
    let config = VALID_CONFIG.replace(
        "[logging]",
        r#"[[workspace.data_sources]]
type = "s3"
bucket = "example"
region = "us-east-1"
access_key_ref = "AWS_ACCESS_KEY_ID"
secret_key_ref = "AWS_SECRET_ACCESS_KEY"
max_extracted_bytes = 1048576

[logging]"#,
    );
    let error = load_config_from_str(&config).expect_err("extracted cap not valid for s3");
    assert!(
        error
            .to_string()
            .contains("max_extracted_bytes is not valid when type is s3"),
        "error was: {error}"
    );
}

#[test]
fn accepts_fully_specified_s3_data_source() {
    let config = VALID_CONFIG.replace(
        "[logging]",
        r#"[[workspace.data_sources]]
type = "s3"
bucket = "example"
prefix = "datasets/"
region = "us-east-1"
access_key_ref = "AWS_ACCESS_KEY_ID"
secret_key_ref = "AWS_SECRET_ACCESS_KEY"

[logging]"#,
    );
    load_config_from_str(&config).expect("complete s3 data source should validate");
}

#[test]
fn rejects_legacy_workspace_source_with_migration_hint() {
    let config = VALID_CONFIG.replace(
        "[logging]",
        r#"[workspace.source]
type = "git"
repo = "https://github.com/example/project.git"
dest = "/workspace/project"

[logging]"#,
    );
    let error = load_config_from_str(&config).expect_err("legacy source rejected");
    let message = error.to_string();
    assert!(
        message.contains("workspace.source") && message.contains("code_sources"),
        "error did not direct operator to the new shape: {message}"
    );
}

#[test]
fn rejects_unknown_data_source_type() {
    let config = VALID_CONFIG.replace(
        "[logging]",
        r#"[[workspace.data_sources]]
type = "ftp"
url = "ftp://example.com/data"

[logging]"#,
    );
    let error = load_config_from_str(&config).expect_err("unknown type rejected");
    assert!(
        error
            .to_string()
            .contains("workspace.data_sources[0]: type must be one of local, https, s3"),
        "error was: {error}"
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
