//! Per-agent native configuration inspectors.

use super::*;

pub(super) fn inspect_claude(content: &str, revision: String) -> Result<InspectedNativeConfig> {
    let mut root = parse_json_object(content)?;
    let mut builder = InspectionBuilder::new(
        "claude-code",
        NativeConfigFormat::Json,
        revision,
        content.len(),
    );

    if let Some(value) = root.remove("model") {
        builder.add_string_candidate("model", "model", ManagedFieldKind::Model, value, |value| {
            Some(CandidateValue::Model {
                value: value.to_owned(),
                provider_hint: None,
            })
        });
    }
    for key in CLAUDE_CODE_CREDENTIAL_ROOTS {
        if root.remove(*key).is_some() {
            builder.block(*key, BlockedReason::Credentials);
        }
    }
    for key in CLAUDE_CODE_AUTH_ROOTS {
        if root.remove(*key).is_some() {
            builder.block(*key, BlockedReason::AuthenticationState);
        }
    }
    for key in CLAUDE_CODE_PERMISSION_ROOTS {
        if root.remove(*key).is_some() {
            let reason = if *key == "sandbox" {
                BlockedReason::Sandbox
            } else {
                BlockedReason::Permissions
            };
            builder.block(*key, reason);
        }
    }
    for key in CLAUDE_CODE_POLICY_ROOTS {
        if root.remove(*key).is_some() {
            builder.block(*key, BlockedReason::AcpsPolicy);
        }
    }
    for key in CLAUDE_CODE_MANAGED_UNSUPPORTED_ROOTS {
        if root.remove(*key).is_some() {
            builder.block(*key, BlockedReason::ManagedUnsupported);
        }
    }

    if let Some(env) = root.get_mut("env").and_then(JsonValue::as_object_mut) {
        let mut remove = Vec::new();
        for key in CLAUDE_CODE_MANAGED_ENV_KEYS {
            if let Some(value) = env.get(*key) {
                if *key == "ANTHROPIC_MODEL" && !builder.has_candidate("model") {
                    builder.add_string_candidate(
                        "model",
                        "env.ANTHROPIC_MODEL",
                        ManagedFieldKind::Model,
                        value.clone(),
                        |value| {
                            Some(CandidateValue::Model {
                                value: value.to_owned(),
                                provider_hint: None,
                            })
                        },
                    );
                } else {
                    let reason = if key.contains("TOKEN") || key.contains("API_KEY") {
                        BlockedReason::Credentials
                    } else {
                        BlockedReason::ManagedUnsupported
                    };
                    builder.block(format!("env.{key}"), reason);
                }
                remove.push(*key);
            }
        }
        for key in remove {
            env.remove(key);
        }
        for key in CLAUDE_CODE_CREDENTIAL_ENV_KEYS {
            if env.remove(*key).is_some() {
                builder.block(format!("env.{key}"), BlockedReason::Credentials);
            }
        }
        for key in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"] {
            let contains_credentials = env
                .get(key)
                .and_then(JsonValue::as_str)
                .is_some_and(url_contains_userinfo);
            if contains_credentials {
                env.remove(key);
                builder.block(format!("env.{key}"), BlockedReason::Credentials);
            }
        }
        let sensitive = env
            .keys()
            .filter_map(|key| sensitive_field_reason(key).map(|reason| (key.clone(), reason)))
            .collect::<Vec<_>>();
        for (key, reason) in sensitive {
            env.remove(&key);
            builder.block(format!("env.{key}"), reason);
        }
        if env.keys().any(|key| executable_environment_key(key)) {
            builder.executable(ExecutableCategory::CommandHelpers);
        }
        if env.is_empty() {
            root.remove("env");
        }
    }

    if let Some(mcp) = root.remove("mcpServers") {
        classify_json_mcp(&mut builder, "mcpServers", mcp, JsonMcpDialect::Claude);
    }
    if root.contains_key("hooks") {
        builder.executable(ExecutableCategory::Hooks);
    }
    for key in CLAUDE_CODE_EXECUTABLE_COMMAND_ROOTS {
        if root.contains_key(*key) {
            builder.executable(ExecutableCategory::CommandHelpers);
        }
    }
    if root.contains_key("enabledPlugins") || root.contains_key("extraKnownMarketplaces") {
        builder.executable(ExecutableCategory::Plugins);
    }

    sanitize_sensitive_json_object(&mut root, "", &mut builder);
    let residual = json_bytes(root)?;
    builder.finish_json(residual)
}

pub(super) fn inspect_opencode(
    content: &str,
    filename: Option<&str>,
    revision: String,
) -> Result<InspectedNativeConfig> {
    let filename_jsonc = filename.is_some_and(|name| name.to_ascii_lowercase().ends_with(".jsonc"));
    let (mut root, normalized_jsonc) = match parse_json_object(content) {
        Ok(root) => (root, filename_jsonc),
        Err(_) => (parse_jsonc_object(content)?, true),
    };
    let format = if normalized_jsonc {
        NativeConfigFormat::Jsonc
    } else {
        NativeConfigFormat::Json
    };
    let mut builder = InspectionBuilder::new("opencode", format, revision, content.len());
    if normalized_jsonc {
        builder.warn("jsonc-normalized");
    }

    if let Some(value) = root.remove("model") {
        if let Some(model) = value.as_str() {
            let (provider, _) = split_opencode_model(model);
            let canonical_provider = provider.and_then(|provider| {
                canonical_provider_id_for_agent_native_id("opencode", provider)
            });
            if let Some(provider) = provider {
                builder.add_candidate(
                    "provider",
                    "model",
                    ManagedFieldKind::Provider,
                    canonical_provider.is_some(),
                    CandidateValue::Provider(canonical_provider.unwrap_or(provider).to_owned()),
                );
            }
            builder.add_candidate(
                "model",
                "model",
                ManagedFieldKind::Model,
                !model.trim().is_empty() && (provider.is_none() || canonical_provider.is_some()),
                CandidateValue::Model {
                    value: model.to_owned(),
                    provider_hint: provider.map(str::to_owned),
                },
            );
        } else {
            builder.incompatible("model", "model", ManagedFieldKind::Model);
        }
    }
    for key in OPENCODE_MANAGED_UNSUPPORTED_ROOTS {
        if root.remove(*key).is_some() {
            builder.block(*key, BlockedReason::ManagedUnsupported);
        }
    }
    for key in OPENCODE_PERMISSION_ROOTS {
        if root.remove(*key).is_some() {
            let reason = if *key == "sandbox" {
                BlockedReason::Sandbox
            } else {
                BlockedReason::Permissions
            };
            builder.block(*key, reason);
        }
    }
    for key in OPENCODE_POLICY_ROOTS {
        if root.remove(*key).is_some() {
            builder.block(*key, BlockedReason::AcpsPolicy);
        }
    }
    if let Some(mcp) = root.remove("mcp") {
        classify_json_mcp(&mut builder, "mcp", mcp, JsonMcpDialect::OpenCode);
    }
    if root.contains_key("plugin") {
        builder.executable(ExecutableCategory::Plugins);
    }
    if root.contains_key("command") {
        builder.executable(ExecutableCategory::CommandHelpers);
    }
    if root.contains_key("formatter") {
        builder.executable(ExecutableCategory::Formatters);
    }
    if root.contains_key("lsp") {
        builder.executable(ExecutableCategory::CommandHelpers);
    }
    sanitize_sensitive_json_object(&mut root, "", &mut builder);
    let residual = json_bytes(root)?;
    builder.finish_json(residual)
}

pub(super) fn inspect_codex(content: &str, revision: String) -> Result<InspectedNativeConfig> {
    let mut root = parse_toml_table(content)?;
    let mut builder =
        InspectionBuilder::new("codex", NativeConfigFormat::Toml, revision, content.len());
    if let Some(value) = root.remove("model") {
        builder.add_toml_string_candidate(
            "model",
            "model",
            ManagedFieldKind::Model,
            value,
            |value| {
                Some(CandidateValue::Model {
                    value: value.to_owned(),
                    provider_hint: None,
                })
            },
        );
    }
    if let Some(value) = root.remove("model_provider") {
        builder.add_toml_string_candidate(
            "provider",
            "model_provider",
            ManagedFieldKind::Provider,
            value,
            |value| {
                canonical_provider_id_for_agent_native_id("codex", value)
                    .map(|provider| CandidateValue::Provider(provider.to_owned()))
            },
        );
    }
    if root.remove("model_providers").is_some() {
        builder.block("model_providers", BlockedReason::ManagedUnsupported);
    }
    if let Some(mcp) = root.remove("mcp_servers") {
        classify_toml_mcp(&mut builder, mcp);
    }
    for key in CODEX_PERMISSION_ROOTS {
        if root.remove(*key).is_some() {
            let reason = if *key == "sandbox_mode" || *key == "sandbox_workspace_write" {
                BlockedReason::Sandbox
            } else {
                BlockedReason::Permissions
            };
            builder.block(*key, reason);
        }
    }
    for key in CODEX_AUTH_ROOTS {
        if root.remove(*key).is_some() {
            builder.block(*key, BlockedReason::AuthenticationState);
        }
    }
    for key in CODEX_MANAGED_UNSUPPORTED_ROOTS {
        if root.remove(*key).is_some() {
            builder.block(*key, BlockedReason::ManagedUnsupported);
        }
    }
    if root.contains_key("hooks") {
        builder.executable(ExecutableCategory::Hooks);
    }
    if root.contains_key("notify") {
        builder.executable(ExecutableCategory::Notifications);
    }
    sanitize_sensitive_toml_table(&mut root, "", &mut builder);
    let residual = toml_bytes(root)?;
    builder.finish_toml(residual)
}

pub(super) fn inspect_amp(content: &str, revision: String) -> Result<InspectedNativeConfig> {
    let mut root = parse_json_object(content)?;
    // Amp is provider-opaque (set_provider=false) and keeps its model in ACP
    // session config rather than settings, so `settings.json` yields only
    // MCP-server candidates. Its keys are flat dotted strings
    // (`"amp.mcpServers"`), matched as literal object keys.
    let mut builder =
        InspectionBuilder::new("amp", NativeConfigFormat::Json, revision, content.len());
    if let Some(mcp) = root.remove("amp.mcpServers") {
        classify_json_mcp(&mut builder, "amp.mcpServers", mcp, JsonMcpDialect::Amp);
    }
    for key in AMP_PERMISSION_ROOTS {
        if root.remove(*key).is_some() {
            builder.block(*key, BlockedReason::Permissions);
        }
    }
    for key in AMP_POLICY_ROOTS {
        if root.remove(*key).is_some() {
            builder.block(*key, BlockedReason::AcpsPolicy);
        }
    }
    sanitize_sensitive_json_object(&mut root, "", &mut builder);
    let residual = json_bytes(root)?;
    builder.finish_json(residual)
}

pub(super) fn inspect_pi(content: &str, revision: String) -> Result<InspectedNativeConfig> {
    let mut root = parse_json_object(content)?;
    // Pi documents `settings.json` as strict JSON (no JSONC), so parse it the
    // same way as amp. Pi is provider-selecting (`defaultProvider`) with
    // a bare `defaultModel` id, so both a provider and a model candidate can be
    // extracted. Pi has no first-class MCP in its settings file (adapter-only),
    // so there are no MCP candidates.
    let mut builder =
        InspectionBuilder::new("pi", NativeConfigFormat::Json, revision, content.len());

    if let Some(value) = root.remove("defaultProvider") {
        builder.add_string_candidate(
            "provider",
            "defaultProvider",
            ManagedFieldKind::Provider,
            value,
            |value| {
                canonical_provider_id_for_agent_native_id("pi", value)
                    .map(|provider| CandidateValue::Provider(provider.to_owned()))
            },
        );
    }
    if let Some(value) = root.remove("defaultModel") {
        builder.add_string_candidate(
            "model",
            "defaultModel",
            ManagedFieldKind::Model,
            value,
            |value| {
                Some(CandidateValue::Model {
                    value: value.to_owned(),
                    provider_hint: None,
                })
            },
        );
    }
    for key in PI_PERMISSION_ROOTS {
        if root.remove(*key).is_some() {
            builder.block(*key, BlockedReason::Permissions);
        }
    }
    // `httpProxy` is a bare host:port for benign routing, but a proxy URL can
    // embed `user:pass@` credentials. Block it as credentials only when it
    // carries userinfo, mirroring how the Claude env-proxy keys are handled;
    // otherwise it survives into the residual.
    if root
        .get("httpProxy")
        .and_then(JsonValue::as_str)
        .is_some_and(url_contains_userinfo)
    {
        root.remove("httpProxy");
        builder.block("httpProxy", BlockedReason::Credentials);
    }
    for key in PI_EXECUTABLE_COMMAND_ROOTS {
        if root.contains_key(*key) {
            builder.executable(ExecutableCategory::CommandHelpers);
        }
    }
    for key in PI_EXECUTABLE_PLUGIN_ROOTS {
        if root.contains_key(*key) {
            builder.executable(ExecutableCategory::Plugins);
        }
    }

    sanitize_sensitive_json_object(&mut root, "", &mut builder);
    let residual = json_bytes(root)?;
    builder.finish_json(residual)
}

pub(super) fn inspect_goose(content: &str, revision: String) -> Result<InspectedNativeConfig> {
    // Goose `config.yaml` root is a mapping of UPPERCASE `GOOSE_*` env-style
    // keys (provider/model/mode/tuning) plus a lowercase `extensions:` map. It
    // is parsed as a JSON `Map` after a YAML→JSON conversion that rejects
    // non-string keys, so the whole sanitize/paths pipeline shared with the
    // JSON harnesses applies unchanged; the residual is re-serialized as YAML.
    let mut root = parse_goose_root(content)?;
    let mut builder =
        InspectionBuilder::new("goose", NativeConfigFormat::Yaml, revision, content.len());

    if let Some(value) = root.remove("GOOSE_PROVIDER") {
        builder.add_string_candidate(
            "provider",
            "GOOSE_PROVIDER",
            ManagedFieldKind::Provider,
            value,
            |value| {
                canonical_provider_id_for_agent_native_id("goose", value)
                    .map(|provider| CandidateValue::Provider(provider.to_owned()))
            },
        );
    }
    // Goose `GOOSE_MODEL` is a bare model id; pair it with the provider named by
    // `GOOSE_PROVIDER` (mirroring how Codex pairs `model` with `model_provider`)
    // so the apply step can reject a model that does not belong to the selected
    // provider lane.
    if let Some(value) = root.remove("GOOSE_MODEL") {
        let provider_hint = goose_provider_hint(&builder);
        builder.add_string_candidate(
            "model",
            "GOOSE_MODEL",
            ManagedFieldKind::Model,
            value,
            move |value| {
                Some(CandidateValue::Model {
                    value: value.to_owned(),
                    provider_hint,
                })
            },
        );
    }
    if let Some(extensions) = root.remove("extensions") {
        classify_goose_extensions(&mut builder, extensions);
    }
    for key in GOOSE_PERMISSION_ROOTS {
        if root.remove(*key).is_some() {
            builder.block(*key, BlockedReason::Permissions);
        }
    }
    for key in GOOSE_MANAGED_UNSUPPORTED_ROOTS {
        if root.remove(*key).is_some() {
            builder.block(*key, BlockedReason::ManagedUnsupported);
        }
    }

    sanitize_sensitive_json_object(&mut root, "", &mut builder);
    let residual = goose_yaml_bytes(root)?;
    builder.finish_yaml(residual)
}

/// Resolve the provider hint for a `GOOSE_MODEL` candidate from a
/// `GOOSE_PROVIDER` candidate already recorded on the builder. The hint is the
/// agent-native provider id (what a `GOOSE_PROVIDER` value would read as), so
/// the apply step can compare it against the effective provider's native id.
fn goose_provider_hint(builder: &InspectionBuilder) -> Option<String> {
    match builder.candidate("provider")? {
        CandidateValue::Provider(provider) => agent_provider_id_for_provider_id("goose", provider)
            .map(str::to_owned)
            .or_else(|| Some(provider.clone())),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
