use acp_stack::config::{
    AgentAdapterConfig, Config, CustomProviderApi, DEFAULT_COMMAND_PROGRESS_INTERVAL,
    DEFAULT_CUSTOM_MODEL_CONTEXT, DEFAULT_CUSTOM_MODEL_OUTPUT_MAX_TOKENS, default_config_path,
    load_config_from_str,
};

const VALID_CONFIG: &str = include_str!("fixtures/valid-opencode-stack.toml");

#[test]
fn default_config_path_uses_acps_config_toml() {
    let path = default_config_path().expect("default config path");

    assert!(path.ends_with(".config/acp-stack/acps-config.toml"));
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
fn canonical_export_keeps_secret_refs_and_omits_secret_values() {
    let config = load_config_from_str(
        &VALID_CONFIG
            .replace(
                r#"env = ["OPENCODE_API_KEY"]"#,
                r#"env = ["OPENAI_API_KEY"]"#,
            )
            .replace(
                r#"api_key_ref = "SUPABASE_SECRET_KEY""#,
                r#"api_key_ref = "SUPABASE_KEY_REF""#,
            ),
    )
    .expect("config with refs should parse");

    let canonical = config
        .to_canonical_toml()
        .expect("canonical TOML should serialize");
    assert!(canonical.contains("OPENAI_API_KEY"));
    assert!(canonical.contains("SUPABASE_KEY_REF"));
    for secret_value in [
        "sk-proj-exampleinlinevalue",
        "github_pat_exampleinlinevalue",
        "acps_exampleinlinevalue",
    ] {
        assert!(
            !canonical.contains(secret_value),
            "canonical export leaked {secret_value}"
        );
    }
}

#[test]
fn parses_custom_provider_defaults() {
    let config_text = format!(
        "{VALID_CONFIG}\n\
         [agent.provider]\n\
         id = \"myprovider\"\n\
         model = \"my-model\"\n\
         api_key_ref = \"CUSTOM_API_KEY\"\n\n\
         [agent.provider.custom]\n\
         name = \"My Provider\"\n\
         base_url = \"https://api.myprovider.example/v1\"\n"
    );

    let config = load_config_from_str(&config_text).expect("custom provider config should parse");
    let custom = config
        .agent
        .provider
        .as_ref()
        .and_then(|provider| provider.custom.as_ref())
        .expect("custom provider should load");

    assert_eq!(custom.api, CustomProviderApi::ChatCompletions);
    assert_eq!(custom.context, DEFAULT_CUSTOM_MODEL_CONTEXT);
    assert_eq!(
        custom.output_max_tokens,
        DEFAULT_CUSTOM_MODEL_OUTPUT_MAX_TOKENS
    );
}

#[test]
fn parses_subagent_provider_config() {
    let config_text = format!(
        "{VALID_CONFIG}\n\
         [agent.subagent.provider]\n\
         id = \"opencode-go\"\n\
         model = \"opencode-go/deepseek-v4-flash\"\n\
         api_key_ref = \"OPENCODE_API_KEY\"\n"
    );

    let config = load_config_from_str(&config_text).expect("subagent provider config should parse");
    let provider = config
        .agent
        .subagent
        .as_ref()
        .and_then(|subagent| subagent.provider.as_ref())
        .expect("subagent provider should load");

    assert_eq!(provider.id, "opencode-go");
    assert_eq!(
        provider.model.as_deref(),
        Some("opencode-go/deepseek-v4-flash")
    );
    assert_eq!(provider.api_key_ref.as_deref(), Some("OPENCODE_API_KEY"));
}

#[test]
fn parses_subagent_custom_provider_config() {
    let config_text = format!(
        "{VALID_CONFIG}\n\
         [agent.subagent.provider]\n\
         id = \"myprovider\"\n\
         model = \"my-model\"\n\
         api_key_ref = \"CUSTOM_API_KEY\"\n\n\
         [agent.subagent.provider.custom]\n\
         name = \"My Provider\"\n\
         base_url = \"https://api.myprovider.example/v1\"\n"
    );

    let config =
        load_config_from_str(&config_text).expect("subagent custom provider config should parse");
    let custom = config
        .agent
        .subagent
        .as_ref()
        .and_then(|subagent| subagent.provider.as_ref())
        .and_then(|provider| provider.custom.as_ref())
        .expect("subagent custom provider should load");

    assert_eq!(custom.api, CustomProviderApi::ChatCompletions);
    assert_eq!(custom.context, DEFAULT_CUSTOM_MODEL_CONTEXT);
    assert_eq!(
        custom.output_max_tokens,
        DEFAULT_CUSTOM_MODEL_OUTPUT_MAX_TOKENS
    );
}

#[test]
fn rejects_custom_provider_without_api_key_ref() {
    let config_text = format!(
        "{VALID_CONFIG}\n\
         [agent.provider]\n\
         id = \"myprovider\"\n\
         model = \"my-model\"\n\n\
         [agent.provider.custom]\n\
         name = \"My Provider\"\n\
         base_url = \"https://api.myprovider.example/v1\"\n"
    );

    let error =
        load_config_from_str(&config_text).expect_err("custom provider without ref should fail");

    assert!(
        error
            .to_string()
            .contains("agent.provider.api_key_ref is required")
    );
}

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
fn rejects_operator_written_agent_adapter() {
    // [agent.adapter] is runtime-populated from the embedded registry, not
    // operator-written. A config carrying it over from the pre-rework shape
    // should fail with a clear unknown-field error rather than silently
    // shadowing what the registry would have resolved.
    let config = VALID_CONFIG.replace(
        r#"restart = "on-crash""#,
        r#"restart = "on-crash"

[agent.adapter]
id = "codex-acp"
name = "Codex ACP Adapter"
upstream_agent = "codex-cli"
source_url = "https://github.com/zed-industries/codex-acp""#,
    );
    let error =
        load_config_from_str(&config).expect_err("operator-written adapter must be rejected");
    let message = error.to_string();
    assert!(
        message.contains("unknown field") && message.contains("adapter"),
        "{error}"
    );
}

#[test]
fn canonical_export_omits_runtime_adapter_metadata() {
    let mut config = load_config_from_str(VALID_CONFIG).expect("valid config should parse");
    config.agent.adapter = Some(AgentAdapterConfig {
        id: "codex-acp".to_owned(),
        name: "Codex".to_owned(),
        upstream_agent: "codex-cli".to_owned(),
        source_url: Some("https://github.com/zed-industries/codex-acp".to_owned()),
    });

    let canonical = config
        .to_canonical_toml()
        .expect("canonical TOML should serialize");
    assert!(!canonical.contains("[agent.adapter]"));
    let round_tripped: Config =
        toml::from_str(&canonical).expect("canonical TOML should parse as config");
    assert!(round_tripped.agent.adapter.is_none());
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
fn rejects_blank_agent_mode() {
    let error = load_config_from_str(&VALID_CONFIG.replace(
        r#"restart = "on-crash""#,
        "restart = \"on-crash\"\nmode = \" \"",
    ))
    .expect_err("config should be invalid");

    assert!(error.to_string().contains("agent.mode is required"));
}

#[test]
fn rejects_blank_agent_model() {
    let error = load_config_from_str(&VALID_CONFIG.replace(
        r#"restart = "on-crash""#,
        "restart = \"on-crash\"\nmodel = \" \"",
    ))
    .expect_err("config should be invalid");

    assert!(error.to_string().contains("agent.model is required"));
}

#[test]
fn rejects_root_model_when_provider_model_is_set() {
    let config = VALID_CONFIG.replace(
        r#"restart = "on-crash""#,
        "restart = \"on-crash\"\nmodel = \"root-model\"",
    ) + "\n[agent.provider]\nid = \"openai\"\nmodel = \"openai/gpt-5.5\"\napi_key_ref = \"OPENAI_API_KEY\"\n";

    let error = load_config_from_str(&config).expect_err("dual model config should fail");
    assert!(
        error
            .to_string()
            .contains("must be omitted when agent.provider.model is set")
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
fn rejects_removed_startup_section() {
    let updated = VALID_CONFIG.replace(
        "[agent]",
        "[[startup.scripts]]\nname = \"bootstrap\"\nscript = \"echo ready\"\nshell = \"/bin/sh\"\n\n[agent]",
    );
    let error = load_config_from_str(&updated).expect_err("startup scripts must be rejected");

    assert!(
        error.to_string().contains("`[startup]` was removed"),
        "got: {error}",
    );
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

fn enable_supabase(input: &str) -> String {
    input.replace("enabled = false", "enabled = true")
}

#[test]
fn supabase_disabled_skips_url_check() {
    // VALID_CONFIG ships with enabled = false, so even a non-https url must
    // parse cleanly until external logging is actually turned on.
    let updated = VALID_CONFIG.replace(
        r#"url = "https://example.supabase.co""#,
        r#"url = "http://insecure.example""#,
    );
    let config = load_config_from_str(&updated).expect("disabled-supabase must parse");
    assert_eq!(
        config.logging.supabase.as_ref().map(|s| s.url.as_str()),
        Some("http://insecure.example")
    );
}

#[test]
fn supabase_enabled_requires_https() {
    let mut updated = enable_supabase(VALID_CONFIG);
    updated = updated.replace(
        r#"url = "https://example.supabase.co""#,
        r#"url = "http://example.supabase.co""#,
    );
    let error = load_config_from_str(&updated).expect_err("non-https supabase url must fail");
    assert!(
        error.to_string().contains("must start with `https://`"),
        "got: {error}",
    );
}

#[test]
fn supabase_enabled_schema_must_be_safe_identifier() {
    let updated = enable_supabase(VALID_CONFIG)
        .replace(r#"schema = "acp_stack""#, r#"schema = "drop tables;""#);
    let error = load_config_from_str(&updated).expect_err("unsafe schema must fail");
    assert!(
        error.to_string().contains("safe Postgres identifier"),
        "got: {error}",
    );
}

#[test]
fn supabase_enabled_with_clean_schema_and_https_passes() {
    let updated = enable_supabase(VALID_CONFIG);
    let config = load_config_from_str(&updated).expect("enabled-supabase happy path");
    let supabase = config.logging.supabase.expect("supabase set");
    assert!(supabase.enabled);
    assert_eq!(supabase.schema, "acp_stack");
}

#[test]
fn supabase_legacy_config_defaults_to_postgrest_backend() {
    let updated = enable_supabase(VALID_CONFIG);
    let config = load_config_from_str(&updated).expect("legacy supabase config parses");
    let supabase = config.logging.supabase.expect("supabase set");
    assert_eq!(
        supabase.backend,
        acp_stack::config::SupabaseLoggingBackend::Postgrest
    );
    assert_eq!(supabase.table_prefix, "");
    assert!(supabase.db_url_ref.is_none());
}

#[test]
fn supabase_postgres_backend_requires_db_url_ref() {
    let updated = enable_supabase(VALID_CONFIG).replace(
        "[logging.supabase]",
        "[logging.supabase]\nbackend = \"postgres\"",
    );
    let error = load_config_from_str(&updated).expect_err("postgres backend needs db url ref");
    assert!(
        error.to_string().contains("logging.supabase.db_url_ref"),
        "got: {error}",
    );
}

#[test]
fn supabase_postgres_backend_accepts_prefixed_public_tables() {
    let updated = enable_supabase(VALID_CONFIG).replace(
        "[logging.supabase]",
        "[logging.supabase]\nbackend = \"postgres\"\ntable_prefix = \"acp_stack_\"\ndb_url_ref = \"SUPABASE_LOG_DB_URL\"",
    ).replace(r#"schema = "acp_stack""#, r#"schema = "public""#);
    let config = load_config_from_str(&updated).expect("postgres supabase config parses");
    let supabase = config.logging.supabase.expect("supabase set");
    assert_eq!(
        supabase.backend,
        acp_stack::config::SupabaseLoggingBackend::Postgres
    );
    assert_eq!(supabase.table_prefix, "acp_stack_");
    assert_eq!(supabase.db_url_ref.as_deref(), Some("SUPABASE_LOG_DB_URL"));
}

#[test]
fn parses_config_with_explicit_version() {
    let input = format!("config_version = 1\n{VALID_CONFIG}");
    let config = load_config_from_str(&input).expect("explicit version 1 should parse");
    assert_eq!(config.config_version, 1);
}

#[test]
fn accepts_missing_config_version_as_version_1() {
    let config = load_config_from_str(VALID_CONFIG).expect("missing version should parse");
    assert_eq!(config.config_version, 1);
}

#[test]
fn rejects_unsupported_config_version() {
    let input = format!("config_version = 99\n{VALID_CONFIG}");
    let error = load_config_from_str(&input).expect_err("unsupported version should be rejected");
    assert!(
        error.to_string().contains("unsupported config version"),
        "got: {error}"
    );
}

#[test]
fn export_includes_config_version() {
    let config = load_config_from_str(VALID_CONFIG).expect("valid config");
    let canonical = config.to_canonical_toml().expect("canonical");
    assert!(canonical.starts_with("config_version = 1\n"));
    assert_eq!(canonical.matches("config_version = 1").count(), 1);
}

#[test]
fn rejects_acpctl_socket_path_relative() {
    let input = format!("{VALID_CONFIG}\n[acpctl]\nsocket_path = \"relative/path.sock\"\n");
    let error = load_config_from_str(&input).expect_err("relative path should be rejected");
    assert!(
        error.to_string().contains("acpctl.socket_path") && error.to_string().contains("absolute"),
        "got: {error}"
    );
}

#[test]
fn rejects_acpctl_socket_path_with_dot_dot() {
    let input = format!("{VALID_CONFIG}\n[acpctl]\nsocket_path = \"/tmp/../etc/passwd.sock\"\n");
    let error = load_config_from_str(&input).expect_err("dot dot path should be rejected");
    assert!(
        error.to_string().contains("acpctl.socket_path") && error.to_string().contains(".."),
        "got: {error}"
    );
}

#[test]
fn allows_acpctl_socket_path_absolute() {
    let input = format!("{VALID_CONFIG}\n[acpctl]\nsocket_path = \"/tmp/acpctl.sock\"\n");
    let config = load_config_from_str(&input).expect("absolute path should be accepted");
    assert_eq!(
        config.acpctl.socket_path.as_deref(),
        Some("/tmp/acpctl.sock")
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

#[test]
fn commands_progress_interval_defaults_and_overrides() {
    let config = load_config_from_str(VALID_CONFIG).expect("default config should parse");
    assert_eq!(
        config.commands.progress_interval,
        DEFAULT_COMMAND_PROGRESS_INTERVAL
    );

    let config_text = format!(
        "{VALID_CONFIG}\n\
         [commands]\n\
         default_timeout = \"10m\"\n\
         cancel_grace = \"5s\"\n\
         progress_interval = \"250ms\"\n\
         env_allowlist = []\n\
         max_output_bytes = 1048576\n"
    );
    let config = load_config_from_str(&config_text).expect("commands override should parse");
    assert_eq!(config.commands.progress_interval, "250ms");
}

#[test]
fn rejects_commands_with_invalid_progress_interval() {
    let config_text = format!(
        "{VALID_CONFIG}\n\
         [commands]\n\
         default_timeout = \"10m\"\n\
         cancel_grace = \"5s\"\n\
         progress_interval = \"0s\"\n\
         env_allowlist = []\n\
         max_output_bytes = 1048576\n"
    );
    let err = load_config_from_str(&config_text)
        .expect_err("zero commands.progress_interval must be rejected");
    assert!(
        err.to_string().contains("commands.progress_interval"),
        "got: {err}"
    );
}

#[test]
fn parses_prompts_block_with_overrides() {
    let config_text = format!(
        "{VALID_CONFIG}\n\
         [prompts]\n\
         stale_threshold = \"10m\"\n\
         sweep_interval = \"45s\"\n"
    );
    let config = load_config_from_str(&config_text).expect("config with [prompts] should parse");
    assert_eq!(config.prompts.stale_threshold, "10m");
    assert_eq!(config.prompts.sweep_interval, "45s");
    assert_eq!(
        config.prompts.effective_stale_threshold(),
        std::time::Duration::from_secs(600)
    );
    assert_eq!(
        config.prompts.effective_sweep_interval(),
        std::time::Duration::from_secs(45)
    );
}

#[test]
fn omitted_prompts_block_falls_back_to_defaults() {
    let config = load_config_from_str(VALID_CONFIG).expect("default config should parse");
    assert_eq!(config.prompts.stale_threshold, "5m");
    assert_eq!(config.prompts.sweep_interval, "30s");
    assert_eq!(
        config.prompts.effective_stale_threshold(),
        std::time::Duration::from_secs(300)
    );
    assert_eq!(
        config.prompts.effective_sweep_interval(),
        std::time::Duration::from_secs(30)
    );
}

#[test]
fn rejects_prompts_with_zero_duration() {
    let config_text = format!(
        "{VALID_CONFIG}\n\
         [prompts]\n\
         stale_threshold = \"0s\"\n\
         sweep_interval = \"30s\"\n"
    );
    let err =
        load_config_from_str(&config_text).expect_err("zero stale_threshold must be rejected");
    assert!(
        err.to_string().contains("prompts.stale_threshold"),
        "got: {err}"
    );
}

#[test]
fn rejects_prompts_with_unparsable_duration() {
    let config_text = format!(
        "{VALID_CONFIG}\n\
         [prompts]\n\
         stale_threshold = \"not-a-duration\"\n\
         sweep_interval = \"30s\"\n"
    );
    let err =
        load_config_from_str(&config_text).expect_err("garbage stale_threshold must be rejected");
    assert!(
        err.to_string().contains("prompts.stale_threshold"),
        "got: {err}"
    );
}
