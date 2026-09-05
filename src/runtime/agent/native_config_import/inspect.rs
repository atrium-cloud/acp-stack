//! Per-agent native configuration inspectors.

use super::*;

pub(super) fn inspect_claude(content: &str, revision: String) -> Result<InspectedNativeConfig> {
    let mut root = parse_json_object(content)?;
    let mut builder =
        InspectionBuilder::new("claude", NativeConfigFormat::Json, revision, content.len());

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
    // Amp is provider-opaque and keeps its model in ACP session config, so
    // `settings.json` yields only MCP candidates under flat dotted keys.
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
    // Pi documents `settings.json` as strict JSON, is provider-selecting via
    // `defaultProvider`/`defaultModel`, and has no first-class MCP there.
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
    // A proxy URL can embed `user:pass@`, so block `httpProxy` as credentials only
    // when it carries userinfo; a bare host:port survives into the residual.
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
    // Parsed as a JSON `Map` via a YAML→JSON conversion that rejects non-string
    // keys, so the sanitize pipeline shared with the JSON harnesses applies
    // unchanged; the residual is re-serialized as YAML.
    let mut root = parse_yaml_root(content)?;
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
    // `GOOSE_MODEL` is a bare model id; pair it with `GOOSE_PROVIDER` so apply can
    // reject a model that does not belong to the selected provider lane.
    if let Some(value) = root.remove("GOOSE_MODEL") {
        let provider_hint = native_provider_hint(&builder, "goose");
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
    let residual = yaml_bytes(root)?;
    builder.finish_yaml(residual)
}

pub(super) fn inspect_kimi(content: &str, revision: String) -> Result<InspectedNativeConfig> {
    let mut root = parse_toml_table(content)?;
    let mut builder = InspectionBuilder::new(
        KIMI_CODE_AGENT_ID,
        NativeConfigFormat::Toml,
        revision,
        content.len(),
    );

    // The alias chain reads `[providers]` before that table is stripped as credentials.
    if let Some(selection) = kimi_model_selection(&root) {
        match selection.provider {
            Some(provider) => builder.add_candidate(
                "provider",
                "default_model",
                ManagedFieldKind::Provider,
                true,
                CandidateValue::Provider(provider.to_owned()),
            ),
            None => builder.incompatible("provider", "default_model", ManagedFieldKind::Provider),
        }
        let model_path = format!("models.{}.model", selection.alias);
        match selection.model {
            Some(model) => {
                let provider_hint = native_provider_hint(&builder, KIMI_CODE_AGENT_ID);
                builder.add_candidate(
                    "model",
                    model_path,
                    ManagedFieldKind::Model,
                    !model.trim().is_empty() && selection.provider.is_some(),
                    CandidateValue::Model {
                        value: model,
                        provider_hint,
                    },
                );
            }
            None => builder.incompatible("model", model_path, ManagedFieldKind::Model),
        }
    } else if root.contains_key("default_model") {
        builder.incompatible("provider", "default_model", ManagedFieldKind::Provider);
        builder.incompatible("model", "default_model", ManagedFieldKind::Model);
    }
    // Aliases beyond the selected one and the secondary-model selector have no acps
    // counterpart, so their removal is reported rather than silent.
    let selected_alias = root
        .get("default_model")
        .and_then(TomlValue::as_str)
        .map(str::to_owned);
    let extra_aliases = root
        .get("models")
        .and_then(TomlValue::as_table)
        .is_some_and(|models| {
            models
                .keys()
                .any(|alias| Some(alias.as_str()) != selected_alias.as_deref())
        });
    if extra_aliases {
        builder.block("models", BlockedReason::ManagedUnsupported);
    }
    if root.contains_key("secondary_model") {
        builder.block("secondary_model", BlockedReason::ManagedUnsupported);
    }
    for key in KIMI_MANAGED_ROOTS {
        root.remove(*key);
    }
    for key in KIMI_CREDENTIAL_ROOTS {
        if root.remove(*key).is_some() {
            builder.block(*key, BlockedReason::Credentials);
        }
    }
    for key in KIMI_PERMISSION_ROOTS {
        if root.remove(*key).is_some() {
            builder.block(*key, BlockedReason::Permissions);
        }
    }
    if root.contains_key("hooks") {
        builder.executable(ExecutableCategory::Hooks);
    }
    sanitize_sensitive_toml_table(&mut root, "", &mut builder);
    let residual = toml_bytes(root)?;
    builder.finish_toml(residual)
}

struct KimiModelSelection {
    alias: String,
    model: Option<String>,
    provider: Option<&'static str>,
}

/// Follows `default_model` → `[models.<alias>]` → `[providers.<name>]` and resolves the provider
/// row by `(type, base_url)` against the catalog rows Kimi runs.
fn kimi_model_selection(root: &TomlMap<String, TomlValue>) -> Option<KimiModelSelection> {
    let alias = root.get("default_model")?.as_str()?;
    let entry = root
        .get("models")
        .and_then(TomlValue::as_table)
        .and_then(|models| models.get(alias))
        .and_then(TomlValue::as_table);
    let model = entry
        .and_then(|entry| entry.get("model"))
        .and_then(TomlValue::as_str)
        .map(str::to_owned);
    let provider = entry
        .and_then(|entry| entry.get("provider"))
        .and_then(TomlValue::as_str)
        .and_then(|name| root.get("providers")?.as_table()?.get(name)?.as_table())
        .and_then(kimi_catalog_provider_for_row);
    Some(KimiModelSelection {
        alias: alias.to_owned(),
        model,
        provider,
    })
}

/// A Kimi `[providers.<name>]` row maps to a catalog row only when its `base_url` is a base Kimi
/// runs and the row's declared wire matches the row's `type`.
fn kimi_catalog_provider_for_row(table: &TomlMap<String, TomlValue>) -> Option<&'static str> {
    let provider_type = table.get("type")?.as_str()?;
    let base_url = table.get("base_url")?.as_str()?;
    let provider_id = provider_id_for_agent_vendor_base_url(KIMI_CODE_AGENT_ID, base_url)?;
    (kimi_profile_for_provider_id(provider_id)?.provider_type == provider_type)
        .then_some(provider_id)
}

pub(super) fn inspect_hermes(content: &str, revision: String) -> Result<InspectedNativeConfig> {
    let mut root = parse_yaml_root(content)?;
    let mut builder = InspectionBuilder::new(
        HERMES_AGENT_ID,
        NativeConfigFormat::Yaml,
        revision,
        content.len(),
    );

    match root.remove(HERMES_MODEL_KEY) {
        Some(JsonValue::Object(mut model)) => {
            inspect_hermes_model(&mut builder, &mut model);
            if !model.is_empty() {
                root.insert(HERMES_MODEL_KEY.to_owned(), JsonValue::Object(model));
            }
        }
        Some(_) => {
            builder.incompatible("provider", HERMES_MODEL_KEY, ManagedFieldKind::Provider);
            builder.incompatible("model", HERMES_MODEL_KEY, ManagedFieldKind::Model);
        }
        None => {}
    }
    if let Some(JsonValue::Object(mut providers)) = root.remove(HERMES_PROVIDERS_KEY) {
        // Provisioning rewrites the managed entry from canonical config, so it is consumed.
        providers.remove(HERMES_MANAGED_ENTRY_KEY);
        let keyed: Vec<String> = providers
            .iter()
            .filter(|(_, entry)| {
                entry.as_object().is_some_and(|entry| {
                    HERMES_PROVIDER_ENTRY_CREDENTIAL_KEYS
                        .iter()
                        .any(|key| entry.contains_key(*key))
                })
            })
            .map(|(name, _)| name.clone())
            .collect();
        for name in keyed {
            providers.remove(&name);
            builder.block(
                format!("{HERMES_PROVIDERS_KEY}.{name}"),
                BlockedReason::Credentials,
            );
        }
        if !providers.is_empty() {
            root.insert(
                HERMES_PROVIDERS_KEY.to_owned(),
                JsonValue::Object(providers),
            );
        }
    }
    for key in HERMES_CREDENTIAL_ROOTS {
        if root.remove(*key).is_some() {
            builder.block(*key, BlockedReason::Credentials);
        }
    }
    for key in HERMES_SANDBOX_ROOTS {
        if let Some(value) = root.remove(*key) {
            if value
                .as_object()
                .is_some_and(|terminal| terminal.contains_key("sudo_password"))
            {
                builder.block(format!("{key}.sudo_password"), BlockedReason::Credentials);
            }
            builder.block(*key, BlockedReason::Sandbox);
        }
    }
    for key in HERMES_IGNORED_ROOTS {
        if root.remove(*key).is_some() {
            builder.warn(&format!("hermes-{key}-ignored"));
        }
    }
    // The adapter advertises no ACP MCP passthrough, so Hermes reads `mcp_servers` from this
    // file gateway-wide: the table stays in the residual verbatim, held out of the sanitizer
    // so its literal env and header values survive the rewrite.
    let mcp_servers = root.remove("mcp_servers");

    sanitize_sensitive_json_object(&mut root, "", &mut builder);
    if let Some(mcp_servers) = mcp_servers {
        root.insert("mcp_servers".to_owned(), mcp_servers);
    }
    let residual = yaml_bytes(root)?;
    builder.finish_yaml(residual)
}

/// Consumes the provider and model selection out of Hermes' `model:` mapping and blocks the
/// credential and auth keys that live beside them.
fn inspect_hermes_model(builder: &mut InspectionBuilder, model: &mut JsonMap<String, JsonValue>) {
    let mut selected: Option<(&'static str, String)> = None;
    let mut nested_provider: Option<String> = None;
    // A malformed higher-precedence key falls through to the next alias; the incompatible
    // field is recorded only when no fallback yields a candidate, since duplicate field ids
    // resolve first-wins at selection time.
    let mut malformed_model_key: Option<&'static str> = None;
    for key in HERMES_MODEL_NAME_KEYS {
        let Some(value) = model.remove(*key) else {
            continue;
        };
        if selected.is_some() {
            continue;
        }
        match value {
            JsonValue::String(value) => selected = Some((key, value)),
            // Upstream flattens a `{provider, model}` mapping and lets its provider win over an
            // outer `auto`.
            JsonValue::Object(mapping) => {
                if let Some(value) = mapping.get("model").and_then(JsonValue::as_str) {
                    selected = Some((key, value.to_owned()));
                    nested_provider = mapping
                        .get(HERMES_MODEL_PROVIDER_KEY)
                        .and_then(JsonValue::as_str)
                        .map(str::to_owned);
                } else if malformed_model_key.is_none() {
                    malformed_model_key = Some(key);
                }
            }
            _ => {
                if malformed_model_key.is_none() {
                    malformed_model_key = Some(key);
                }
            }
        }
    }
    let mut outer_provider_malformed = false;
    let outer_provider = match model.remove(HERMES_MODEL_PROVIDER_KEY) {
        Some(JsonValue::String(provider)) => Some(provider),
        Some(_) => {
            outer_provider_malformed = true;
            None
        }
        None => None,
    };
    let outer_provider_path = format!("{HERMES_MODEL_KEY}.{HERMES_MODEL_PROVIDER_KEY}");
    let nested_provider_path = selected
        .as_ref()
        .map(|(key, _)| format!("{HERMES_MODEL_KEY}.{key}.{HERMES_MODEL_PROVIDER_KEY}"));
    let (provider, provider_path) = match (outer_provider, nested_provider) {
        (Some(outer), Some(nested)) if outer == "auto" => {
            (Some(nested), nested_provider_path.clone())
        }
        (Some(outer), _) => (Some(outer), None),
        (None, nested) => (nested, nested_provider_path),
    };
    let provider_path = provider_path
        .as_deref()
        .unwrap_or(outer_provider_path.as_str());
    let mut provider_selected = false;
    if let Some(provider) = provider.as_deref() {
        provider_selected = true;
        if provider == HERMES_MANAGED_PROVIDER_REF {
            // acps's own provisioning ref, not a user selection.
            builder.warn("hermes-managed-provider-dropped");
            provider_selected = false;
        } else if provider == "auto"
            || provider == HERMES_CUSTOM_PROVIDER_ID
            || provider.starts_with(&format!("{HERMES_CUSTOM_PROVIDER_ID}:"))
            || HERMES_LOCAL_SERVER_PROVIDER_IDS.contains(&provider)
        {
            builder.incompatible("provider", provider_path, ManagedFieldKind::Provider);
        } else {
            match canonical_provider_id_for_agent_native_id(HERMES_AGENT_ID, provider) {
                Some(canonical) => builder.add_candidate(
                    "provider",
                    provider_path,
                    ManagedFieldKind::Provider,
                    true,
                    CandidateValue::Provider(canonical.to_owned()),
                ),
                None => builder.incompatible("provider", provider_path, ManagedFieldKind::Provider),
            }
        }
    } else if outer_provider_malformed {
        builder.incompatible("provider", outer_provider_path, ManagedFieldKind::Provider);
    }
    model.remove(HERMES_MODEL_BASE_URL_KEY);
    if let Some((key, value)) = selected {
        let provider_hint = native_provider_hint(builder, HERMES_AGENT_ID);
        builder.add_candidate(
            "model",
            format!("{HERMES_MODEL_KEY}.{key}"),
            ManagedFieldKind::Model,
            !value.trim().is_empty() && (!provider_selected || builder.has_candidate("provider")),
            CandidateValue::Model {
                value,
                provider_hint,
            },
        );
    } else if let Some(key) = malformed_model_key {
        builder.incompatible(
            "model",
            format!("{HERMES_MODEL_KEY}.{key}"),
            ManagedFieldKind::Model,
        );
    }
    for key in HERMES_MODEL_CREDENTIAL_KEYS {
        if model.remove(*key).is_some() {
            builder.block(
                format!("{HERMES_MODEL_KEY}.{key}"),
                BlockedReason::Credentials,
            );
        }
    }
    for key in HERMES_MODEL_AUTH_KEYS {
        if model.remove(*key).is_some() {
            builder.block(
                format!("{HERMES_MODEL_KEY}.{key}"),
                BlockedReason::AuthenticationState,
            );
        }
    }
}

/// Provider hint for a model candidate, as the agent-native provider id so apply can compare
/// it against the effective provider's native id.
fn native_provider_hint(builder: &InspectionBuilder, agent_id: &str) -> Option<String> {
    match builder.candidate("provider")? {
        CandidateValue::Provider(provider) => agent_provider_id_for_provider_id(agent_id, provider)
            .map(str::to_owned)
            .or_else(|| Some(provider.clone())),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
