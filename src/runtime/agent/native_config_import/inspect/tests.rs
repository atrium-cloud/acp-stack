use super::*;

#[test]
fn claude_strips_managed_and_blocked_fields_but_keeps_unmanaged() {
    let inspected = inspect_native_config(
        "claude-code",
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
        "claude-code",
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
        "claude-code",
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
fn rejects_auth_state_and_project_scope_filenames() {
    for (harness, filename) in [
        ("claude-code", ".claude.json"),
        ("claude-code", "settings.local.json"),
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
        "claude-code",
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
