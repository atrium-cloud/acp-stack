use super::*;

/// `CandidateValue` carries no `Debug` or `PartialEq` by design, so tests read it apart.
fn provider_candidate(inspected: &InspectedNativeConfig) -> Option<String> {
    match inspected.candidates.get("provider") {
        Some(CandidateValue::Provider(provider)) => Some(provider.clone()),
        _ => None,
    }
}

fn model_candidate(inspected: &InspectedNativeConfig) -> Option<(String, Option<String>)> {
    match inspected.candidates.get("model") {
        Some(CandidateValue::Model {
            value,
            provider_hint,
        }) => Some((value.clone(), provider_hint.clone())),
        _ => None,
    }
}

#[test]
fn claude_strips_managed_and_blocked_fields_but_keeps_unmanaged() {
    let inspected = inspect_native_config(
        "claude",
        Some("settings.json"),
        r#"{
              "model":"claude-sonnet",
              "apiKeyHelper":"printenv SECRET",
              "env":{"ANTHROPIC_API_KEY":"literal","KEEP_ME":"yes"},
              "permissions":{"allow":["Bash(*)"]},
              "hooks":{"Stop":[{"hooks":[{"command":"notify"}]}]},
              "theme":"dark"
            }"#,
    )
    .expect("inspect");
    let manifest = inspected.inspection();
    assert!(
        manifest
            .managed_fields
            .iter()
            .any(|field| field.id == "model")
    );
    assert!(
        manifest
            .blocked_fields
            .iter()
            .any(|field| field.path == "apiKeyHelper")
    );
    assert!(
        manifest
            .blocked_fields
            .iter()
            .any(|field| field.path == "env.ANTHROPIC_API_KEY")
    );
    assert_eq!(
        manifest.executable_categories,
        vec![ExecutableCategory::Hooks]
    );
    let residual: JsonValue = serde_json::from_slice(inspected.residual()).expect("json");
    assert_eq!(residual["env"]["KEEP_ME"], "yes");
    assert_eq!(residual["theme"], "dark");
    assert!(residual.get("model").is_none());
    assert!(residual.get("apiKeyHelper").is_none());
    assert!(residual.get("permissions").is_none());
}

#[test]
fn claude_blocks_security_and_credential_controls_and_flags_command_helpers() {
    let inspected = inspect_native_config(
        "claude",
        Some("settings.json"),
        r#"{
              "defaultMode":"bypassPermissions",
              "skipDangerousModePermissionPrompt":true,
              "forceLoginMethod":"claudeai",
              "awsCredentialExport":"/tmp/export-creds",
              "policyHelper":{"path":"/tmp/policy"},
              "agent":"reviewer",
              "fileSuggestion":{"type":"command","command":"/tmp/suggest"},
              "theme":"dark"
            }"#,
    )
    .expect("inspect");
    let manifest = inspected.inspection();
    for path in [
        "defaultMode",
        "skipDangerousModePermissionPrompt",
        "forceLoginMethod",
        "awsCredentialExport",
        "policyHelper",
        "agent",
    ] {
        assert!(
            manifest
                .blocked_fields
                .iter()
                .any(|field| field.path == path),
            "missing blocked path {path}"
        );
    }
    assert!(
        manifest
            .executable_categories
            .contains(&ExecutableCategory::CommandHelpers)
    );
    let residual: JsonValue = serde_json::from_slice(inspected.residual()).expect("json");
    assert!(residual.get("defaultMode").is_none());
    assert!(residual.get("forceLoginMethod").is_none());
    assert!(residual.get("awsCredentialExport").is_none());
    assert_eq!(residual["fileSuggestion"]["command"], "/tmp/suggest");
    assert_eq!(residual["theme"], "dark");
}

#[test]
fn claude_blocks_literal_telemetry_credentials_and_flags_otel_helper() {
    let inspected = inspect_native_config(
        "claude",
        Some("settings.json"),
        r#"{
              "env": {
                "OTEL_EXPORTER_OTLP_HEADERS":"Authorization=Bearer literal",
                "HTTPS_PROXY":"https://user:password@example.com",
                "LANG":"en_US.UTF-8"
              },
              "otelHeadersHelper":"/tmp/headers-helper"
            }"#,
    )
    .expect("inspect");
    assert!(
        inspected
            .inspection()
            .blocked_fields
            .iter()
            .any(|field| field.path == "env.OTEL_EXPORTER_OTLP_HEADERS")
    );
    assert!(
        inspected
            .inspection()
            .blocked_fields
            .iter()
            .any(|field| field.path == "env.HTTPS_PROXY")
    );
    assert!(
        inspected
            .inspection()
            .executable_categories
            .contains(&ExecutableCategory::CommandHelpers)
    );
    let residual: JsonValue = serde_json::from_slice(inspected.residual()).expect("json");
    assert_eq!(residual["env"]["LANG"], "en_US.UTF-8");
    assert!(residual["env"].get("HTTPS_PROXY").is_none());
}

#[test]
fn codex_classifies_provider_model_and_simple_mcp() {
    let inspected = inspect_native_config(
        "codex",
        Some("config.toml"),
        r#"
model = "gpt-5.5"
model_provider = "openai"
approval_policy = "never"
notify = ["notify-send"]

[mcp_servers.local]
command = "npx"
args = ["-y", "server"]
env_vars = ["MCP_TOKEN"]

[features]
web_search = true
"#,
    )
    .expect("inspect");
    let manifest = inspected.inspection();
    assert!(
        manifest
            .managed_fields
            .iter()
            .any(|field| field.id == "provider")
    );
    assert!(
        manifest
            .managed_fields
            .iter()
            .any(|field| field.id == "model")
    );
    assert!(
        manifest
            .managed_fields
            .iter()
            .any(|field| field.id == "mcp:local")
    );
    assert!(
        manifest
            .blocked_fields
            .iter()
            .any(|field| field.path == "approval_policy")
    );
    assert!(
        manifest
            .executable_categories
            .contains(&ExecutableCategory::Notifications)
    );
    assert!(
        manifest
            .executable_categories
            .contains(&ExecutableCategory::CommandHelpers)
    );
    let residual = std::str::from_utf8(inspected.residual()).expect("utf8");
    assert!(residual.contains("[features]"));
    assert!(!residual.contains("model_provider"));
    assert!(!residual.contains("mcp_servers"));
}

#[test]
fn credential_scans_catch_value_style_and_non_suffix_shapes() {
    fn owned(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_owned()).collect()
    }
    assert!(command_args_contain_literal_credentials(&owned(&[
        "-H",
        "Authorization: Bearer sk-live-1234"
    ])));
    assert!(command_args_contain_literal_credentials(&owned(&[
        "--header",
        "x-api-key: sk-live-1234"
    ])));
    assert!(command_args_contain_literal_credentials(&owned(&[
        "Bearer sk-live-1234"
    ])));
    assert!(!command_args_contain_literal_credentials(&owned(&[
        "--url",
        "https://example.com/mcp"
    ])));
    assert!(!command_args_contain_literal_credentials(&owned(&[
        "-H",
        "content-type: application/json"
    ])));

    assert_eq!(
        sensitive_field_reason("apiKeyId"),
        Some(BlockedReason::Credentials)
    );
    assert_eq!(
        sensitive_field_reason("tokenValue"),
        Some(BlockedReason::Credentials)
    );
    assert_eq!(
        sensitive_field_reason("secretRef"),
        Some(BlockedReason::Credentials)
    );
    assert_eq!(
        sensitive_field_reason("clientSecretId"),
        Some(BlockedReason::Credentials)
    );
    assert_eq!(
        sensitive_field_reason("authToken"),
        Some(BlockedReason::Credentials)
    );
    assert_eq!(sensitive_field_reason("model_max_output_tokens"), None);
    assert_eq!(sensitive_field_reason("max_tokens"), None);
    assert_eq!(
        sensitive_field_reason("model_auto_compact_token_limit"),
        None
    );
    assert_eq!(sensitive_field_reason("tool_output_token_limit"), None);
    assert_eq!(sensitive_field_reason("prefill_token_weight"), None);
    assert_eq!(sensitive_field_reason("includeCoAuthoredBy"), None);
    assert_eq!(sensitive_field_reason("keybinds"), None);
    assert_eq!(sensitive_field_reason("theme"), None);

    assert!(!mcp_http_url_is_credential_free(
        "https://example.com/mcp/sk-live-abcdef123456"
    ));
    assert!(!mcp_http_url_is_credential_free(
        "https://example.com/t/ghp_16charsofpayload"
    ));
    assert!(mcp_http_url_is_credential_free(
        "https://example.com/mcp/sk-learn"
    ));
    assert!(mcp_http_url_is_credential_free("https://example.com/mcp"));
}

#[test]
fn neutral_fields_with_literal_credentials_are_removed_across_formats() {
    let json = inspect_native_config(
        "opencode",
        Some("opencode.json"),
        r#"{"metadata":{"label":"safe","value":"ghp_16charsofpayload"},"theme":"sk-learn"}"#,
    )
    .expect("json inspect");
    let json_residual: JsonValue = serde_json::from_slice(json.residual()).expect("json residual");
    assert!(json_residual.get("metadata").is_none());
    assert_eq!(json_residual["theme"], "sk-learn");
    assert!(
        json.inspection().blocked_fields.iter().any(|field| {
            field.path == "metadata" && field.reason == BlockedReason::Credentials
        })
    );

    let toml = inspect_native_config(
        "codex",
        Some("config.toml"),
        "theme = 'sk-learn'\n[metadata]\nvalue = 'Bearer ghp_16charsofpayload'\n",
    )
    .expect("toml inspect");
    let toml_residual: TomlValue =
        toml::from_str(std::str::from_utf8(toml.residual()).expect("toml utf8"))
            .expect("toml residual");
    assert!(toml_residual.get("metadata").is_none());
    assert_eq!(
        toml_residual.get("theme").and_then(TomlValue::as_str),
        Some("sk-learn")
    );

    let yaml = inspect_native_config(
        "goose",
        Some("config.yaml"),
        "metadata:\n  value: https://user:password@example.com/mcp\ntheme: sk-learn\n",
    )
    .expect("yaml inspect");
    let yaml_residual: YamlValue =
        serde_norway::from_slice(yaml.residual()).expect("yaml residual");
    let yaml_residual = yaml_residual.as_mapping().expect("yaml mapping");
    assert!(
        yaml_residual
            .get(YamlValue::String("metadata".to_owned()))
            .is_none()
    );
    assert_eq!(
        yaml_residual.get(YamlValue::String("theme".to_owned())),
        Some(&YamlValue::String("sk-learn".to_owned()))
    );
}

#[test]
fn codex_mcp_maps_real_stdio_schema_and_blocks_unrepresentable_servers() {
    // Field shapes from developers.openai.com/codex/config-sample.
    let inspected = inspect_native_config(
        "codex",
        Some("config.toml"),
        r#"
[mcp_servers.tuned]
command = "npx"
args = ["-y", "server"]
env_vars = ["MCP_TOKEN", { name = "OTHER_TOKEN", source = "keychain" }]
required = true
startup_timeout_sec = 120
startup_timeout_ms = 120000
tool_timeout_sec = 300
tool_timeout_ms = 300000

[mcp_servers.filtered]
command = "npx"
args = ["-y", "server"]
disabled_tools = ["delete_everything"]

[mcp_servers.literal_env]
command = "npx"
args = ["-y", "server"]
env = { SOME_FLAG = "1" }

[mcp_servers.literal_secret_env]
command = "npx"
args = ["-y", "server"]
env = { WIDGET_API_KEY = "sk-live-1234" }

[mcp_servers.scoped]
command = "npx"
args = ["-y", "server"]
cwd = "/srv/mcp"
"#,
    )
    .expect("inspect");
    let manifest = inspected.inspection();
    assert!(
        manifest
            .managed_fields
            .iter()
            .any(|field| field.id == "mcp:tuned")
    );
    assert!(manifest.blocked_fields.iter().any(|field| {
        field.path == "mcp_servers.literal_env" && field.reason == BlockedReason::McpUnmappable
    }));
    assert!(manifest.blocked_fields.iter().any(|field| {
        field.path == "mcp_servers.literal_secret_env" && field.reason == BlockedReason::Credentials
    }));
    assert!(manifest.blocked_fields.iter().any(|field| {
        field.path == "mcp_servers.scoped" && field.reason == BlockedReason::McpUnmappable
    }));
    assert!(manifest.blocked_fields.iter().any(|field| {
        field.path == "mcp_servers.filtered" && field.reason == BlockedReason::McpUnmappable
    }));
    let manifest_json = serde_json::to_string(manifest).expect("manifest json");
    assert!(!manifest_json.contains("sk-live-1234"));
    let residual = std::str::from_utf8(inspected.residual()).expect("utf8");
    assert!(!residual.contains("sk-live-1234"));
    assert!(!residual.contains("mcp_servers"));

    let tuned: TomlValue = toml::from_str(
        r#"
command = "npx"
env_vars = ["MCP_TOKEN", { name = "OTHER_TOKEN", source = "keychain" }]
"#,
    )
    .expect("toml");
    let McpServerConfig::Stdio(stdio) = toml_mcp_server("tuned", &tuned).expect("mappable") else {
        panic!("stdio server expected");
    };
    assert_eq!(
        stdio.env,
        vec!["MCP_TOKEN".to_owned(), "OTHER_TOKEN".to_owned()]
    );
}

#[test]
fn amp_maps_dotted_mcp_and_blocks_permission_and_policy_keys() {
    // Field shapes from ampcode.com/manual.
    let inspected = inspect_native_config(
        "amp",
        Some("settings.json"),
        r#"{
              "amp.mcpServers": {
                "playwright": {"command": "npx", "args": ["-y", "@playwright/mcp@latest"]},
                "linear": {"url": "https://mcp.linear.app/sse"},
                "with_secret_env": {"command": "npx", "env": {"WIDGET_API_KEY": "sk-live-1234"}}
              },
              "amp.commands.allowlist": ["git status", "npm run build"],
              "amp.commands.strict": false,
              "amp.dangerouslyAllowAll": false,
              "amp.tools.disable": ["browser_navigate"],
              "amp.notifications.enabled": true
            }"#,
    )
    .expect("inspect");
    let manifest = inspected.inspection();
    for id in ["mcp:playwright", "mcp:linear"] {
        assert!(
            manifest.managed_fields.iter().any(|field| field.id == id),
            "missing managed field {id}"
        );
    }
    assert!(manifest.blocked_fields.iter().any(|field| {
        field.path == "amp.mcpServers.with_secret_env" && field.reason == BlockedReason::Credentials
    }));
    for path in [
        "amp.commands.allowlist",
        "amp.commands.strict",
        "amp.dangerouslyAllowAll",
    ] {
        assert!(
            manifest
                .blocked_fields
                .iter()
                .any(|field| { field.path == path && field.reason == BlockedReason::Permissions }),
            "missing permission block {path}"
        );
    }
    assert!(manifest.blocked_fields.iter().any(|field| {
        field.path == "amp.tools.disable" && field.reason == BlockedReason::AcpsPolicy
    }));
    let manifest_json = serde_json::to_string(manifest).expect("manifest json");
    assert!(!manifest_json.contains("sk-live-1234"));
    let residual: JsonValue = serde_json::from_slice(inspected.residual()).expect("json");
    assert_eq!(residual["amp.notifications.enabled"], true);
    assert!(residual.get("amp.mcpServers").is_none());
    assert!(residual.get("amp.commands.allowlist").is_none());
    assert!(residual.get("amp.tools.disable").is_none());
    assert!(
        !std::str::from_utf8(inspected.residual())
            .expect("utf8")
            .contains("sk-live-1234")
    );
}

#[test]
fn pi_maps_provider_model_and_blocks_exec_permission_credential_keys() {
    // Field shapes from earendil-works/pi settings.md.
    let inspected = inspect_native_config(
        "pi",
        Some("settings.json"),
        r#"{
              "defaultProvider": "anthropic",
              "defaultModel": "claude-sonnet-4-20250514",
              "shellPath": "/bin/zsh",
              "npmCommand": ["pnpm", "install"],
              "packages": ["@acme/pkg"],
              "skills": ["/home/user/skills/foo"],
              "defaultProjectTrust": "trusted",
              "httpProxy": "http://user:pass@proxy.internal:8080",
              "trackingId": "sk-live-1234",
              "defaultThinkingLevel": "high",
              "theme": "dark"
            }"#,
    )
    .expect("inspect");
    let manifest = inspected.inspection();
    let provider = manifest
        .managed_fields
        .iter()
        .find(|field| field.id == "provider")
        .expect("provider candidate");
    assert_eq!(provider.kind, ManagedFieldKind::Provider);
    assert!(provider.compatible);
    let model = manifest
        .managed_fields
        .iter()
        .find(|field| field.id == "model")
        .expect("model candidate");
    assert_eq!(model.kind, ManagedFieldKind::Model);
    assert!(model.compatible);
    assert!(manifest.blocked_fields.iter().any(|field| {
        field.path == "defaultProjectTrust" && field.reason == BlockedReason::Permissions
    }));
    assert!(
        manifest
            .blocked_fields
            .iter()
            .any(|field| field.path == "httpProxy" && field.reason == BlockedReason::Credentials)
    );
    // `trackingId` ends in `id` but Pi documents it as an analytics id, not a
    // credential, so it survives while its literal value must not leak.
    for category in [
        ExecutableCategory::CommandHelpers,
        ExecutableCategory::Plugins,
    ] {
        assert!(
            manifest.executable_categories.contains(&category),
            "missing executable category {category:?}"
        );
    }
    let manifest_json = serde_json::to_string(manifest).expect("manifest json");
    assert!(!manifest_json.contains("anthropic"));
    assert!(!manifest_json.contains("claude-sonnet-4-20250514"));
    assert!(!manifest_json.contains("pass@proxy"));
    let residual: JsonValue = serde_json::from_slice(inspected.residual()).expect("json");
    assert_eq!(residual["defaultThinkingLevel"], "high");
    assert_eq!(residual["theme"], "dark");
    assert!(residual.get("defaultProvider").is_none());
    assert!(residual.get("defaultModel").is_none());
    assert!(residual.get("defaultProjectTrust").is_none());
    assert!(residual.get("httpProxy").is_none());
    // Executable roots stay in the residual but are flagged so selection
    // requires acknowledgement.
    assert!(residual.get("packages").is_some());
}

#[test]
fn pi_unmappable_provider_is_incompatible_candidate() {
    let inspected = inspect_native_config(
        "pi",
        Some("settings.json"),
        r#"{"defaultProvider": "totally-unknown-provider"}"#,
    )
    .expect("inspect");
    let provider = inspected
        .inspection()
        .managed_fields
        .iter()
        .find(|field| field.id == "provider")
        .expect("provider candidate");
    assert!(!provider.compatible);
}

#[test]
fn goose_maps_provider_model_extensions_and_blocks_mode_planner_credentials() {
    // Field shapes from block/goose config-files.md and extension.rs.
    let inspected = inspect_native_config(
        "goose",
        Some("config.yaml"),
        r#"
GOOSE_PROVIDER: anthropic
GOOSE_MODEL: claude-sonnet-4-5
GOOSE_MODE: auto
GOOSE_ALLOWLIST: https://example.com/allowlist.yaml
GOOSE_PLANNER_MODEL: gpt-5.5
GOOSE_TEMPERATURE: 0.2
GOOSE_CONTEXT_STRATEGY: summarize
extensions:
  fetcher:
    type: stdio
    cmd: uvx
    args: ["mcp-server-fetch"]
    env_keys: ["FETCH_PROXY"]
    timeout: 300
    bundled: false
    enabled: true
  literal_env:
    type: stdio
    cmd: run
    envs:
      OPENAI_API_KEY: sk-live-abc
  remote:
    type: streamable_http
    uri: https://mcp.example.com/sse
  builtin_dev:
    type: builtin
    name: developer
  disabled:
    type: stdio
    cmd: off
    enabled: false
"#,
    )
    .expect("inspect");
    let manifest = inspected.inspection();
    assert_eq!(manifest.format, NativeConfigFormat::Yaml);

    let provider = manifest
        .managed_fields
        .iter()
        .find(|field| field.id == "provider")
        .expect("provider candidate");
    assert_eq!(provider.kind, ManagedFieldKind::Provider);
    assert!(provider.compatible);
    let model = manifest
        .managed_fields
        .iter()
        .find(|field| field.id == "model")
        .expect("model candidate");
    assert_eq!(model.kind, ManagedFieldKind::Model);

    assert!(
        manifest
            .managed_fields
            .iter()
            .any(|field| field.id == "mcp:fetcher" && field.compatible)
    );
    assert!(
        manifest
            .managed_fields
            .iter()
            .any(|field| field.id == "mcp:remote")
    );
    assert!(manifest.blocked_fields.iter().any(|field| {
        field.path == "extensions.literal_env" && field.reason == BlockedReason::Credentials
    }));
    assert!(manifest.blocked_fields.iter().any(|field| {
        field.path == "extensions.builtin_dev" && field.reason == BlockedReason::McpUnmappable
    }));
    assert!(manifest.blocked_fields.iter().any(|field| {
        field.path == "extensions.disabled" && field.reason == BlockedReason::McpUnmappable
    }));
    assert!(
        manifest
            .blocked_fields
            .iter()
            .any(|field| field.path == "GOOSE_MODE" && field.reason == BlockedReason::Permissions)
    );
    assert!(manifest.blocked_fields.iter().any(|field| {
        field.path == "GOOSE_ALLOWLIST" && field.reason == BlockedReason::Permissions
    }));
    assert!(manifest.blocked_fields.iter().any(|field| {
        field.path == "GOOSE_PLANNER_MODEL" && field.reason == BlockedReason::ManagedUnsupported
    }));
    assert!(
        manifest
            .executable_categories
            .contains(&ExecutableCategory::CommandHelpers)
    );

    // The manifest must stay value-free: no provider/model/secret literals.
    let manifest_json = serde_json::to_string(manifest).expect("manifest json");
    assert!(!manifest_json.contains("anthropic"));
    assert!(!manifest_json.contains("claude-sonnet-4-5"));
    assert!(!manifest_json.contains("sk-live-abc"));

    let residual: YamlValue =
        serde_norway::from_str(std::str::from_utf8(inspected.residual()).expect("utf8"))
            .expect("residual yaml parses");
    let residual = residual.as_mapping().expect("residual mapping");
    assert_eq!(
        residual.get(YamlValue::String("GOOSE_TEMPERATURE".to_owned())),
        Some(&YamlValue::Number(serde_norway::Number::from(0.2)))
    );
    assert_eq!(
        residual.get(YamlValue::String("GOOSE_CONTEXT_STRATEGY".to_owned())),
        Some(&YamlValue::String("summarize".to_owned()))
    );
    for key in [
        "GOOSE_PROVIDER",
        "GOOSE_MODEL",
        "GOOSE_MODE",
        "GOOSE_ALLOWLIST",
        "GOOSE_PLANNER_MODEL",
        "extensions",
    ] {
        assert!(
            residual.get(YamlValue::String(key.to_owned())).is_none(),
            "residual leaked {key}"
        );
    }
}

#[test]
fn kimi_classifies_alias_chain_credentials_permissions_and_hooks() {
    // Field shapes from the Kimi Code config-file docs.
    let inspected = inspect_native_config(
        "kimi",
        Some("config.toml"),
        r#"
default_model = "router"
default_permission_mode = "yolo"
telemetry = false

[models.router]
provider = "or"
model = "moonshotai/kimi-k3"
max_context_size = 262144

[models.spare]
provider = "or"
model = "openai/gpt-5.5"
max_context_size = 200000

[secondary_model]
default_model = "spare"

[providers.or]
type = "openai"
base_url = "https://openrouter.ai/api/v1"
api_key = "sk-or-live-abc"

[services.moonshot_search]
api_key = "sk-svc-abc"

[permission]
dangerous_command_guard = true

[hooks.session_start]
command = "notify-send"

[thinking]
enabled = true
"#,
    )
    .expect("inspect");
    let manifest = inspected.inspection();
    assert_eq!(manifest.format, NativeConfigFormat::Toml);
    assert_eq!(manifest.harness, "kimi");

    let provider = manifest
        .managed_fields
        .iter()
        .find(|field| field.id == "provider")
        .expect("provider candidate");
    assert_eq!(provider.path, "default_model");
    assert!(provider.compatible);
    assert_eq!(
        provider_candidate(&inspected),
        Some("openrouter".to_owned())
    );
    let model = manifest
        .managed_fields
        .iter()
        .find(|field| field.id == "model")
        .expect("model candidate");
    assert_eq!(model.path, "models.router.model");
    assert!(model.compatible);
    assert_eq!(
        model_candidate(&inspected),
        Some((
            "moonshotai/kimi-k3".to_owned(),
            Some("openrouter".to_owned())
        ))
    );
    for (path, reason) in [
        ("providers", BlockedReason::Credentials),
        ("services", BlockedReason::Credentials),
        ("default_permission_mode", BlockedReason::Permissions),
        ("permission", BlockedReason::Permissions),
        // `[models.spare]` and the secondary selector have no acps counterpart.
        ("models", BlockedReason::ManagedUnsupported),
        ("secondary_model", BlockedReason::ManagedUnsupported),
    ] {
        assert!(
            manifest
                .blocked_fields
                .iter()
                .any(|field| field.path == path && field.reason == reason),
            "{path} blocked as {reason:?}"
        );
    }
    assert!(
        manifest
            .executable_categories
            .contains(&ExecutableCategory::Hooks)
    );
    assert!(inspected.residual_has_executable);

    let manifest_json = serde_json::to_string(manifest).expect("manifest json");
    assert!(!manifest_json.contains("sk-or-live-abc"));
    assert!(!manifest_json.contains("sk-svc-abc"));
    assert!(!manifest_json.contains("moonshotai/kimi-k3"));

    let residual = std::str::from_utf8(inspected.residual()).expect("utf8");
    assert!(residual.contains("[thinking]"));
    assert!(residual.contains("telemetry = false"));
    assert!(residual.contains("[hooks.session_start]"));
    for key in [
        "default_model",
        "[models",
        "secondary_model",
        "[providers",
        "[services",
        "[permission]",
        "sk-or-live-abc",
    ] {
        assert!(!residual.contains(key), "residual leaked {key}");
    }
}

#[test]
fn kimi_provider_rows_resolve_by_wire_and_catalog_base_only() {
    let inspect = |providers: &str| {
        inspect_native_config(
            "kimi",
            Some("config.toml"),
            &format!(
                r#"
default_model = "main"

[models.main]
provider = "p"
model = "some-model"
max_context_size = 262144

[providers.p]
{providers}
"#
            ),
        )
        .expect("inspect")
    };
    let provider_of = |inspected: &InspectedNativeConfig| {
        inspected
            .inspection()
            .managed_fields
            .iter()
            .find(|field| field.id == "provider")
            .map(|field| field.compatible)
    };
    let model_of = |inspected: &InspectedNativeConfig| {
        inspected
            .inspection()
            .managed_fields
            .iter()
            .find(|field| field.id == "model")
            .map(|field| field.compatible)
    };

    let coding = inspect("type = \"kimi\"\nbase_url = \"https://api.kimi.com/coding/v1\"");
    assert_eq!(provider_of(&coding), Some(true));
    assert!(
        !coding
            .inspection()
            .blocked_fields
            .iter()
            .any(|field| field.path == "models"),
        "a single selected alias is consumed, not reported as unsupported"
    );
    assert_eq!(provider_candidate(&coding), Some("kimi-coding".to_owned()));
    assert_eq!(
        model_candidate(&coding),
        Some(("some-model".to_owned(), Some("kimi-code".to_owned())))
    );

    let anthropic = inspect("type = \"anthropic\"\nbase_url = \"https://api.anthropic.com\"");
    assert_eq!(provider_of(&anthropic), Some(true));
    assert_eq!(provider_candidate(&anthropic), Some("anthropic".to_owned()));

    // The row's declared wire must match: OpenRouter is an `openai` row.
    let wire_mismatch =
        inspect("type = \"anthropic\"\nbase_url = \"https://openrouter.ai/api/v1\"");
    assert_eq!(provider_of(&wire_mismatch), Some(false));
    assert_eq!(model_of(&wire_mismatch), Some(false));

    // A bare type resolves nothing; the vendor default is not assumed.
    let no_base = inspect("type = \"anthropic\"");
    assert_eq!(provider_of(&no_base), Some(false));
    assert_eq!(model_of(&no_base), Some(false));

    let unknown_base = inspect("type = \"openai\"\nbase_url = \"https://llm.internal.example/v1\"");
    assert_eq!(provider_of(&unknown_base), Some(false));

    let google = inspect(
        "type = \"google-genai\"\nbase_url = \"https://generativelanguage.googleapis.com/v1beta\"",
    );
    assert_eq!(provider_of(&google), Some(false));

    // A dangling alias yields incompatible candidates, not a parse failure.
    let dangling = inspect_native_config(
        "kimi",
        Some("config.toml"),
        "default_model = \"missing\"\n[thinking]\nenabled = true\n",
    )
    .expect("inspect");
    assert_eq!(provider_of(&dangling), Some(false));
    assert_eq!(model_of(&dangling), Some(false));
    assert!(dangling.candidates.is_empty());
}

#[test]
fn hermes_maps_model_and_provider_and_blocks_credentials_sandbox_and_ignored_roots() {
    // Field shapes from hermes_cli/config.py and cli-config.yaml.example.
    let inspected = inspect_native_config(
        "hermes",
        Some("config.yaml"),
        r#"
model:
  provider: openrouter
  default: anthropic/claude-sonnet-5
  base_url: https://openrouter.ai/api/v1
  api_key: sk-or-live-abc
  auth_mode: api-key
  temperature: 0.4
providers:
  acps-managed:
    name: Managed
    base_url: http://127.0.0.1:3129/api/v1
    key_env: OPENROUTER_API_KEY
    transport: chat_completions
  work:
    base_url: https://llm.work.example/v1
    api_key: sk-work-abc
  home:
    base_url: https://llm.home.example/v1
    key_env: HOME_LLM_KEY
    api_mode: chat_completions
secrets:
  command:
    command: pass show llm
terminal:
  backend: docker
  sudo_password: hunter2
toolsets:
  - web
mcp_servers:
  fetch:
    command: uvx
    args: ["mcp-server-fetch"]
  notion:
    url: https://mcp.notion.example/mcp
    headers:
      Authorization: Bearer ntn_abc
  sampled:
    command: node
    args: ["server.js"]
    sampling: true
  plain_env:
    command: node
    args: ["server.js"]
    env:
      LOG_LEVEL: debug
agent:
  max_turns: 40
compression:
  enabled: true
"#,
    )
    .expect("inspect");
    let manifest = inspected.inspection();
    assert_eq!(manifest.format, NativeConfigFormat::Yaml);
    assert_eq!(manifest.harness, "hermes");

    let provider = manifest
        .managed_fields
        .iter()
        .find(|field| field.id == "provider")
        .expect("provider candidate");
    assert_eq!(provider.path, "model.provider");
    assert!(provider.compatible);
    assert_eq!(
        provider_candidate(&inspected),
        Some("openrouter".to_owned())
    );
    let model = manifest
        .managed_fields
        .iter()
        .find(|field| field.id == "model")
        .expect("model candidate");
    assert_eq!(model.path, "model.default");
    assert!(model.compatible);
    assert_eq!(
        model_candidate(&inspected),
        Some((
            "anthropic/claude-sonnet-5".to_owned(),
            Some("openrouter".to_owned())
        ))
    );
    for (path, reason) in [
        ("model.api_key", BlockedReason::Credentials),
        ("model.auth_mode", BlockedReason::AuthenticationState),
        ("providers.work", BlockedReason::Credentials),
        ("secrets", BlockedReason::Credentials),
        ("terminal.sudo_password", BlockedReason::Credentials),
        ("terminal", BlockedReason::Sandbox),
    ] {
        assert!(
            manifest
                .blocked_fields
                .iter()
                .any(|field| field.path == path && field.reason == reason),
            "{path} blocked as {reason:?}: {:?}",
            manifest.blocked_fields
        );
    }
    assert!(
        manifest
            .warnings
            .contains(&"hermes-toolsets-ignored".to_owned())
    );
    // Hermes reads `mcp_servers` from its own config file gateway-wide, so the table is
    // unmanaged: no candidates, no blocked entries, and the residual keeps it verbatim.
    assert!(
        !manifest
            .managed_fields
            .iter()
            .any(|field| field.id.starts_with("mcp:"))
    );
    assert!(
        !manifest
            .blocked_fields
            .iter()
            .any(|field| field.path.starts_with("mcp_servers"))
    );
    assert!(!inspected.residual_has_executable);

    let manifest_json = serde_json::to_string(manifest).expect("manifest json");
    for literal in [
        "sk-or-live-abc",
        "sk-work-abc",
        "ntn_abc",
        "hunter2",
        "pass show llm",
    ] {
        assert!(
            !manifest_json.contains(literal),
            "manifest leaked {literal}"
        );
    }

    let residual: YamlValue =
        serde_norway::from_str(std::str::from_utf8(inspected.residual()).expect("utf8"))
            .expect("residual yaml parses");
    let residual = residual.as_mapping().expect("residual mapping");
    let model = residual
        .get(YamlValue::String("model".to_owned()))
        .and_then(YamlValue::as_mapping)
        .expect("model mapping survives with its user-owned keys");
    assert_eq!(
        model.get(YamlValue::String("temperature".to_owned())),
        Some(&YamlValue::Number(serde_norway::Number::from(0.4)))
    );
    for key in ["provider", "default", "base_url", "api_key", "auth_mode"] {
        assert!(
            model.get(YamlValue::String(key.to_owned())).is_none(),
            "model residual leaked {key}"
        );
    }
    let providers = residual
        .get(YamlValue::String("providers".to_owned()))
        .and_then(YamlValue::as_mapping)
        .expect("providers mapping keeps the keyless entry");
    assert!(providers.contains_key(YamlValue::String("home".to_owned())));
    for key in ["acps-managed", "work"] {
        assert!(
            !providers.contains_key(YamlValue::String(key.to_owned())),
            "providers residual leaked {key}"
        );
    }
    for key in ["secrets", "terminal", "toolsets"] {
        assert!(
            residual.get(YamlValue::String(key.to_owned())).is_none(),
            "residual leaked {key}"
        );
    }
    let mcp_servers = residual
        .get(YamlValue::String("mcp_servers".to_owned()))
        .and_then(YamlValue::as_mapping)
        .expect("mcp_servers survives the residual rewrite");
    for name in ["fetch", "notion", "sampled", "plain_env"] {
        assert!(
            mcp_servers.contains_key(YamlValue::String(name.to_owned())),
            "mcp_servers lost {name}"
        );
    }
    let residual_text = std::str::from_utf8(inspected.residual()).expect("utf8");
    assert!(
        residual_text.contains("Bearer ntn_abc"),
        "the gateway-wide header value must survive verbatim"
    );
    assert!(residual.contains_key(YamlValue::String("agent".to_owned())));
    assert!(residual.contains_key(YamlValue::String("compression".to_owned())));
}

#[test]
fn hermes_model_aliases_managed_ref_and_custom_lanes() {
    let provider_field = |inspected: &InspectedNativeConfig| {
        inspected
            .inspection()
            .managed_fields
            .iter()
            .find(|field| field.id == "provider")
            .map(|field| (field.path.clone(), field.compatible))
    };

    // A dict-valued `default` flattens and its provider overrides an outer `auto`; the field
    // path names the nested key the winner came from.
    let flattened = inspect_native_config(
        "hermes",
        Some("config.yaml"),
        "model:\n  provider: auto\n  default:\n    provider: deepseek\n    model: deepseek-v4-pro\n",
    )
    .expect("inspect");
    assert_eq!(
        provider_field(&flattened),
        Some(("model.default.provider".to_owned(), true))
    );
    assert_eq!(provider_candidate(&flattened), Some("deepseek".to_owned()));
    assert_eq!(
        model_candidate(&flattened),
        Some(("deepseek-v4-pro".to_owned(), Some("deepseek".to_owned())))
    );

    // `default` outranks `model` and `name`; the losers are consumed, not persisted.
    let precedence = inspect_native_config(
        "hermes",
        Some("config.yaml"),
        "model:\n  provider: kimi\n  name: from-name\n  model: from-model\n  default: from-default\n",
    )
    .expect("inspect");
    assert_eq!(
        model_candidate(&precedence),
        Some(("from-default".to_owned(), Some("kimi".to_owned())))
    );
    assert_eq!(provider_candidate(&precedence), Some("kimi".to_owned()));
    assert!(
        !std::str::from_utf8(precedence.residual())
            .expect("utf8")
            .contains("from-")
    );

    // acps's own provisioning ref is dropped with a warning; the model still imports alone.
    let managed = inspect_native_config(
        "hermes",
        Some("config.yaml"),
        "model:\n  provider: custom:acps-managed\n  default: some-model\n",
    )
    .expect("inspect");
    assert_eq!(provider_field(&managed), None);
    assert!(
        managed
            .inspection()
            .warnings
            .contains(&"hermes-managed-provider-dropped".to_owned())
    );
    assert_eq!(
        model_candidate(&managed),
        Some(("some-model".to_owned(), None))
    );

    for provider in [
        "auto",
        "custom",
        "custom:work",
        "ollama",
        "vllm",
        "llamacpp",
        "nope",
    ] {
        let inspected = inspect_native_config(
            "hermes",
            Some("config.yaml"),
            &format!("model:\n  provider: {provider}\n  default: some-model\n"),
        )
        .expect("inspect");
        assert_eq!(
            provider_field(&inspected),
            Some(("model.provider".to_owned(), false)),
            "{provider}"
        );
        assert!(
            !inspected
                .inspection()
                .managed_fields
                .iter()
                .any(|field| field.id == "model" && field.compatible),
            "{provider}: a model on an unresolved provider must not import"
        );
    }

    // A scalar `model:` has nothing to consume.
    let scalar =
        inspect_native_config("hermes", Some("config.yaml"), "model: gpt-5.5\n").expect("inspect");
    assert_eq!(provider_field(&scalar), Some(("model".to_owned(), false)));
    assert!(scalar.candidates.is_empty());
}

#[test]
fn hermes_model_fallback_after_a_malformed_key_stays_selectable() {
    let fields = |inspected: &InspectedNativeConfig, id: &str| {
        inspected
            .inspection()
            .managed_fields
            .iter()
            .filter(|field| field.id == id)
            .map(|field| (field.path.clone(), field.compatible))
            .collect::<Vec<_>>()
    };

    // A malformed higher-precedence key yields exactly one field: the compatible fallback.
    // A duplicate incompatible entry would win the first-wins selection lookup and make the
    // fallback unselectable.
    let fallback = inspect_native_config(
        "hermes",
        Some("config.yaml"),
        "model:\n  provider: openrouter\n  default: 42\n  model: from-model\n",
    )
    .expect("inspect");
    assert_eq!(
        fields(&fallback, "model"),
        vec![("model.model".to_owned(), true)]
    );
    assert_eq!(
        model_candidate(&fallback),
        Some(("from-model".to_owned(), Some("openrouter".to_owned())))
    );

    // A malformed outer provider with a valid nested provider keeps the nested candidate.
    let nested = inspect_native_config(
        "hermes",
        Some("config.yaml"),
        "model:\n  provider: 42\n  default:\n    provider: deepseek\n    model: deepseek-v4-pro\n",
    )
    .expect("inspect");
    assert_eq!(
        fields(&nested, "provider"),
        vec![("model.default.provider".to_owned(), true)]
    );
    assert_eq!(provider_candidate(&nested), Some("deepseek".to_owned()));

    // Without a fallback the malformed key is the incompatible field.
    let malformed_only =
        inspect_native_config("hermes", Some("config.yaml"), "model:\n  default: 42\n")
            .expect("inspect");
    assert_eq!(
        fields(&malformed_only, "model"),
        vec![("model.default".to_owned(), false)]
    );
    let malformed_provider = inspect_native_config(
        "hermes",
        Some("config.yaml"),
        "model:\n  provider: 42\n  default: some-model\n",
    )
    .expect("inspect");
    assert_eq!(
        fields(&malformed_provider, "provider"),
        vec![("model.provider".to_owned(), false)]
    );
}

#[test]
fn goose_unmappable_provider_is_incompatible_candidate() {
    let inspected = inspect_native_config(
        "goose",
        Some("config.yaml"),
        "GOOSE_PROVIDER: totally-unknown-provider\n",
    )
    .expect("inspect");
    let provider = inspected
        .inspection()
        .managed_fields
        .iter()
        .find(|field| field.id == "provider")
        .expect("provider candidate");
    assert!(!provider.compatible);
}

#[test]
fn goose_invalid_yaml_and_non_string_keys_are_redacted_errors() {
    let error = inspect_native_config(
        "goose",
        Some("config.yaml"),
        "GOOSE_PROVIDER: [unterminated",
    )
    .err()
    .expect("invalid yaml rejected");
    assert_eq!(error.error_code(), "agent.native_config_invalid");

    // A non-string mapping key has no JSON representation; rejecting it must not
    // echo the sensitive value that follows.
    let error = inspect_native_config(
        "goose",
        Some("config.yaml"),
        "123: sk-live-should-not-leak\n",
    )
    .err()
    .expect("non-string key rejected");
    assert_eq!(error.error_code(), "agent.native_config_invalid");
    assert!(!error.public_message().contains("sk-live-should-not-leak"));
}

#[test]
fn opencode_accepts_jsonc_and_normalizes_to_json() {
    let inspected = inspect_native_config(
        "opencode",
        Some("opencode.jsonc"),
        r#"{
              // selection
              "model": "openai/gpt-5.5",
              "permission": "allow",
              "plugin": ["file:///workspace/plugin.js"],
              "theme": "dark",
            }"#,
    )
    .expect("inspect");
    assert_eq!(inspected.inspection().format, NativeConfigFormat::Jsonc);
    assert!(
        inspected
            .inspection()
            .warnings
            .contains(&"jsonc-normalized".to_owned())
    );
    assert!(
        inspected
            .inspection()
            .managed_fields
            .iter()
            .any(|field| field.id == "provider")
    );
    assert!(
        inspected
            .inspection()
            .managed_fields
            .iter()
            .any(|field| field.id == "model")
    );
    assert!(
        inspected
            .inspection()
            .blocked_fields
            .iter()
            .any(|field| field.path == "permission")
    );
    assert!(
        inspected
            .inspection()
            .executable_categories
            .contains(&ExecutableCategory::Plugins)
    );
    let residual: JsonValue = serde_json::from_slice(inspected.residual()).expect("json");
    assert_eq!(residual["theme"], "dark");
    assert!(residual.get("model").is_none());
}

#[test]
fn invalid_jsonc_and_oversize_inputs_are_redacted_errors() {
    let error = match inspect_native_config("opencode", Some("opencode.jsonc"), "{/* secret") {
        Ok(_) => panic!("invalid config was accepted"),
        Err(error) => error,
    };
    assert_eq!(error.error_code(), "agent.native_config_invalid");
    assert!(!error.public_message().contains("secret"));
    let oversized = "x".repeat(IMPORT_SIZE_LIMIT + 1);
    let error = match inspect_native_config("codex", Some("config.toml"), &oversized) {
        Ok(_) => panic!("oversized config was accepted"),
        Err(error) => error,
    };
    assert_eq!(error.error_code(), "agent.native_config_too_large");
}

#[test]
fn claude_settings_local_json_imports_as_user_scope_settings() {
    let inspected = inspect_native_config(
        "claude",
        Some("settings.local.json"),
        r#"{"model":"claude-sonnet-5","theme":"dark"}"#,
    )
    .expect("settings.local.json inspects");
    assert!(
        inspected
            .inspection()
            .managed_fields
            .iter()
            .any(|field| field.id == "model")
    );
    assert_eq!(
        native_config_path("claude", Path::new("/home/u")).expect("path"),
        Path::new("/home/u/.claude/settings.json")
    );
}

#[test]
fn rejects_auth_state_and_project_scope_filenames() {
    for (harness, filename) in [
        ("claude", ".claude.json"),
        ("claude", ".mcp.json"),
        ("codex", "auth.json"),
        ("codex", ".codex/config.toml"),
        ("opencode", "auth.json"),
        // Pi accepts only `settings.json`.
        ("pi", "models.json"),
        ("pi", "auth.json"),
        ("pi", "trust.json"),
        ("pi", "mcp.json"),
        // Goose accepts only `config.yaml`; `secrets.yaml` holds API keys and
        // `permission.yaml` per-tool approvals, so neither may import.
        ("goose", "secrets.yaml"),
        ("goose", "permission.yaml"),
        // Kimi's `mcp.json` and Hermes' example file have no native destination.
        ("kimi", "mcp.json"),
        ("hermes", "cli-config.yaml.example"),
    ] {
        let error = inspect_native_config(harness, Some(filename), "{}")
            .err()
            .expect("filename rejected");
        assert_eq!(
            error.error_code(),
            "agent.native_config_filename_unsupported"
        );
    }
    let error = inspect_native_config("codex", None, "model = 'gpt'")
        .err()
        .expect("filename required");
    assert_eq!(error.error_code(), "agent.native_config_filename_required");
}

#[test]
fn strips_unknown_credentials_and_managed_agent_controls() {
    let inspected = inspect_native_config(
        "claude",
        Some("settings.json"),
        r#"{
                "env": {
                    "OTHER_TOKEN": "literal-secret",
                    "NODE_OPTIONS": "--require ./loader.js",
                    "KEEP": "ok"
                },
                "agents": {"reviewer": {"prompt": "ignore policy"}},
                "theme": "dark"
            }"#,
    )
    .expect("inspect");
    assert!(inspected.inspection().blocked_fields.iter().any(|field| {
        field.path == "env.OTHER_TOKEN" && field.reason == BlockedReason::Credentials
    }));
    assert!(
        inspected
            .inspection()
            .blocked_fields
            .iter()
            .any(|field| field.path == "agents")
    );
    assert!(
        inspected
            .inspection()
            .executable_categories
            .contains(&ExecutableCategory::CommandHelpers)
    );
    let residual: JsonValue = serde_json::from_slice(inspected.residual()).expect("json");
    assert!(residual["env"].get("OTHER_TOKEN").is_none());
    assert_eq!(residual["env"]["KEEP"], "ok");
    assert!(residual.get("agents").is_none());

    let inspected = inspect_native_config(
        "opencode",
        Some("opencode.json"),
        r#"{"agent":{"review":{"tools":{"bash":true},"prompt":"unsafe"}},"theme":"dark"}"#,
    )
    .expect("inspect");
    assert!(
        inspected
            .inspection()
            .blocked_fields
            .iter()
            .any(|field| field.path == "agent")
    );
    let residual: JsonValue = serde_json::from_slice(inspected.residual()).expect("json");
    assert!(residual.get("agent").is_none());
}

#[test]
fn jsonc_normalization_preserves_unicode() {
    let inspected = inspect_native_config(
        "opencode",
        Some("opencode.jsonc"),
        "{ // comment\n \"theme\": \"暗色\",\n}",
    )
    .expect("inspect");
    let residual: JsonValue = serde_json::from_slice(inspected.residual()).expect("json");
    assert_eq!(residual["theme"], "暗色");
}
