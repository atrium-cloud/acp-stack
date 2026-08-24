//! MCP server classification, credential detection, and inspection builder.

use super::*;

pub(super) fn classify_goose_extensions(builder: &mut InspectionBuilder, value: JsonValue) {
    let JsonValue::Object(servers) = value else {
        builder.block("extensions", BlockedReason::McpUnmappable);
        return;
    };
    for (name, value) in &servers {
        let path = format!("extensions.{name}");
        match goose_extension_server(name, value) {
            Ok(server) => {
                let candidate_id = format!("mcp:{name}");
                if matches!(server, McpServerConfig::Stdio(_)) {
                    builder.executable_candidate(
                        candidate_id.clone(),
                        ExecutableCategory::CommandHelpers,
                    );
                }
                builder.add_candidate(
                    candidate_id,
                    path,
                    ManagedFieldKind::Mcp,
                    true,
                    CandidateValue::Mcp(server),
                );
            }
            Err(reason) => builder.block(path, reason),
        }
    }
}

fn goose_extension_server(
    name: &str,
    value: &JsonValue,
) -> std::result::Result<McpServerConfig, BlockedReason> {
    let object = value.as_object().ok_or(BlockedReason::McpUnmappable)?;
    if object.get("enabled").and_then(JsonValue::as_bool) == Some(false) {
        return Err(BlockedReason::McpUnmappable);
    }
    // Only `stdio` and the remote transports have an external server to import;
    // the rest run inside the Goose process.
    let extension_type = object
        .get("type")
        .and_then(JsonValue::as_str)
        .ok_or(BlockedReason::McpUnmappable)?;
    match extension_type {
        "stdio" => goose_stdio_extension(name, object),
        "streamable_http" | "sse" => goose_remote_extension(name, object),
        _ => Err(BlockedReason::McpUnmappable),
    }
}

fn goose_stdio_extension(
    name: &str,
    object: &JsonMap<String, JsonValue>,
) -> std::result::Result<McpServerConfig, BlockedReason> {
    let command = object
        .get("cmd")
        .and_then(JsonValue::as_str)
        .ok_or(BlockedReason::McpUnmappable)?
        .to_owned();
    let args = match object.get("args") {
        Some(value) => json_string_array(value).ok_or(BlockedReason::McpUnmappable)?,
        None => Vec::new(),
    };
    // acps stdio `env` entries are secret-store reference NAMES, so a literal
    // env table cannot be represented; classify by key name so a
    // credential-bearing table surfaces as credentials, not a mapping failure.
    if let Some(envs) = object.get("envs") {
        let envs = envs.as_object().ok_or(BlockedReason::McpUnmappable)?;
        if !envs.is_empty() {
            if envs.keys().any(|key| sensitive_field_reason(key).is_some()) {
                return Err(BlockedReason::Credentials);
            }
            return Err(BlockedReason::McpUnmappable);
        }
    }
    // `env_keys` forwards variable NAMES, not values; acps satisfies them from
    // its own secret store at session attach.
    let env = match object.get("env_keys") {
        Some(value) => json_string_array(value).ok_or(BlockedReason::McpUnmappable)?,
        None => Vec::new(),
    };
    // `cwd` and `available_tools` stay OUT of this allowlist deliberately:
    // dropping `cwd` corrupts command resolution, and dropping the tool filter
    // would silently re-enable tools the user turned off.
    let allowed: BTreeSet<&str> = [
        "type",
        "name",
        "display_name",
        "description",
        "cmd",
        "args",
        "envs",
        "env_keys",
        "timeout",
        "bundled",
        "enabled",
    ]
    .into_iter()
    .collect();
    if object.keys().any(|key| !allowed.contains(key.as_str())) {
        return Err(BlockedReason::McpUnmappable);
    }
    if command_args_contain_literal_credentials(&args) {
        return Err(BlockedReason::Credentials);
    }
    Ok(McpServerConfig::Stdio(McpStdioServer {
        name: name.to_owned(),
        command,
        args,
        env,
    }))
}

fn goose_remote_extension(
    name: &str,
    object: &JsonMap<String, JsonValue>,
) -> std::result::Result<McpServerConfig, BlockedReason> {
    // A literal `headers` table carries auth material acps expresses only as
    // secret-store references, so any non-empty headers table blocks.
    if let Some(headers) = object.get("headers") {
        let headers = headers.as_object().ok_or(BlockedReason::McpUnmappable)?;
        if !headers.is_empty() {
            if headers
                .keys()
                .any(|key| sensitive_field_reason(key).is_some())
            {
                return Err(BlockedReason::Credentials);
            }
            return Err(BlockedReason::McpUnmappable);
        }
    }
    // Same literal-`envs` reasoning as the stdio path.
    if let Some(envs) = object.get("envs") {
        let envs = envs.as_object().ok_or(BlockedReason::McpUnmappable)?;
        if !envs.is_empty() {
            if envs.keys().any(|key| sensitive_field_reason(key).is_some()) {
                return Err(BlockedReason::Credentials);
            }
            return Err(BlockedReason::McpUnmappable);
        }
    }
    let uri = object
        .get("uri")
        .and_then(JsonValue::as_str)
        .ok_or(BlockedReason::McpUnmappable)?;
    // `socket` and `env_keys` stay OUT of this allowlist: neither is
    // expressible on an acps http server, so such servers must keep blocking.
    let allowed: BTreeSet<&str> = [
        "type",
        "name",
        "description",
        "uri",
        "headers",
        "envs",
        "timeout",
        "bundled",
        "enabled",
    ]
    .into_iter()
    .collect();
    if object.keys().any(|key| !allowed.contains(key.as_str())) {
        return Err(BlockedReason::McpUnmappable);
    }
    if !mcp_http_url_is_credential_free(uri) {
        return Err(BlockedReason::Credentials);
    }
    Ok(McpServerConfig::Http(McpHttpServer {
        name: name.to_owned(),
        url: uri.to_owned(),
        headers: Vec::new(),
    }))
}

#[derive(Clone, Copy)]
pub(super) enum JsonMcpDialect {
    Claude,
    OpenCode,
    Amp,
}

pub(super) fn classify_json_mcp(
    builder: &mut InspectionBuilder,
    root_path: &str,
    value: JsonValue,
    dialect: JsonMcpDialect,
) {
    let Some(servers) = value.as_object() else {
        builder.block(root_path, BlockedReason::McpUnmappable);
        return;
    };
    for (name, value) in servers {
        let path = format!("{root_path}.{name}");
        match json_mcp_server(name, value, dialect) {
            Ok(server) => {
                let candidate_id = format!("mcp:{name}");
                if matches!(server, McpServerConfig::Stdio(_)) {
                    builder.executable_candidate(
                        candidate_id.clone(),
                        ExecutableCategory::CommandHelpers,
                    );
                }
                builder.add_candidate(
                    candidate_id,
                    path,
                    ManagedFieldKind::Mcp,
                    true,
                    CandidateValue::Mcp(server),
                );
            }
            Err(reason) => builder.block(path, reason),
        }
    }
}

fn json_mcp_server(
    name: &str,
    value: &JsonValue,
    dialect: JsonMcpDialect,
) -> std::result::Result<McpServerConfig, BlockedReason> {
    let object = value.as_object().ok_or(BlockedReason::McpUnmappable)?;
    if object.get("enabled").and_then(JsonValue::as_bool) == Some(false) {
        return Err(BlockedReason::McpUnmappable);
    }
    // acps http header values are secret-store indirections, so Amp's literal
    // header table cannot be represented; classify by key name so a
    // credential-bearing table surfaces as credentials, not a mapping failure.
    if matches!(dialect, JsonMcpDialect::Amp)
        && let Some(headers) = object.get("headers")
    {
        let headers = headers.as_object().ok_or(BlockedReason::McpUnmappable)?;
        if headers
            .keys()
            .any(|key| sensitive_field_reason(key).is_some())
        {
            return Err(BlockedReason::Credentials);
        }
        return Err(BlockedReason::McpUnmappable);
    }
    if let Some(url) = object.get("url").and_then(JsonValue::as_str) {
        let allowed: BTreeSet<&str> = match dialect {
            JsonMcpDialect::Claude => ["url", "type"].into_iter().collect(),
            JsonMcpDialect::OpenCode => ["url", "type", "enabled"].into_iter().collect(),
            JsonMcpDialect::Amp => ["url", "type", "includeTools"].into_iter().collect(),
        };
        if object.keys().any(|key| !allowed.contains(key.as_str())) {
            return Err(BlockedReason::McpUnmappable);
        }
        if !mcp_http_url_is_credential_free(url) {
            return Err(BlockedReason::Credentials);
        }
        return Ok(McpServerConfig::Http(McpHttpServer {
            name: name.to_owned(),
            url: url.to_owned(),
            headers: Vec::new(),
        }));
    }

    let (command, args) = match object.get("command").ok_or(BlockedReason::McpUnmappable)? {
        JsonValue::String(command) => {
            let args = match object.get("args") {
                Some(value) => json_string_array(value).ok_or(BlockedReason::McpUnmappable)?,
                None => Vec::new(),
            };
            (command.clone(), args)
        }
        JsonValue::Array(command) if !command.is_empty() => {
            let mut values = command.iter().map(JsonValue::as_str);
            let command = values
                .next()
                .flatten()
                .ok_or(BlockedReason::McpUnmappable)?
                .to_owned();
            let args = values
                .map(|value| value.map(str::to_owned))
                .collect::<Option<Vec<_>>>()
                .ok_or(BlockedReason::McpUnmappable)?;
            (command, args)
        }
        _ => return Err(BlockedReason::McpUnmappable),
    };
    // Same literal-`env` reasoning as the Goose stdio path.
    if matches!(dialect, JsonMcpDialect::Amp)
        && let Some(env) = object.get("env")
    {
        let env = env.as_object().ok_or(BlockedReason::McpUnmappable)?;
        if env.keys().any(|key| sensitive_field_reason(key).is_some()) {
            return Err(BlockedReason::Credentials);
        }
        return Err(BlockedReason::McpUnmappable);
    }
    let allowed: BTreeSet<&str> = match dialect {
        JsonMcpDialect::Claude => ["command", "args", "type"].into_iter().collect(),
        JsonMcpDialect::OpenCode => ["command", "type", "enabled"].into_iter().collect(),
        // `includeTools` stays out: dropping a tool filter would silently
        // re-enable tools the user turned off.
        JsonMcpDialect::Amp => ["command", "args", "type"].into_iter().collect(),
    };
    if object.keys().any(|key| !allowed.contains(key.as_str())) {
        return Err(BlockedReason::McpUnmappable);
    }
    if command_args_contain_literal_credentials(&args) {
        return Err(BlockedReason::Credentials);
    }
    Ok(McpServerConfig::Stdio(McpStdioServer {
        name: name.to_owned(),
        command,
        args,
        env: Vec::new(),
    }))
}

pub(super) fn classify_toml_mcp(builder: &mut InspectionBuilder, value: TomlValue) {
    let Some(servers) = value.as_table() else {
        builder.block("mcp_servers", BlockedReason::McpUnmappable);
        return;
    };
    for (name, value) in servers {
        let path = format!("mcp_servers.{name}");
        match toml_mcp_server(name, value) {
            Ok(server) => {
                let candidate_id = format!("mcp:{name}");
                if matches!(server, McpServerConfig::Stdio(_)) {
                    builder.executable_candidate(
                        candidate_id.clone(),
                        ExecutableCategory::CommandHelpers,
                    );
                }
                builder.add_candidate(
                    candidate_id,
                    path,
                    ManagedFieldKind::Mcp,
                    true,
                    CandidateValue::Mcp(server),
                );
            }
            Err(reason) => builder.block(path, reason),
        }
    }
}

pub(super) fn toml_mcp_server(
    name: &str,
    value: &TomlValue,
) -> std::result::Result<McpServerConfig, BlockedReason> {
    let table = value.as_table().ok_or(BlockedReason::McpUnmappable)?;
    if table.get("enabled").and_then(TomlValue::as_bool) == Some(false) {
        return Err(BlockedReason::McpUnmappable);
    }
    if let Some(url) = table.get("url").and_then(TomlValue::as_str) {
        // Auth material (`bearer_token_env_var`, `http_headers`, …) stays out
        // of this allowlist so those servers keep blocking.
        let allowed: BTreeSet<&str> = [
            "url",
            "enabled",
            "required",
            "startup_timeout_sec",
            "startup_timeout_ms",
            "tool_timeout_sec",
            "tool_timeout_ms",
        ]
        .into_iter()
        .collect();
        if table.keys().any(|key| !allowed.contains(key.as_str())) {
            return Err(BlockedReason::McpUnmappable);
        }
        if !mcp_http_url_is_credential_free(url) {
            return Err(BlockedReason::Credentials);
        }
        return Ok(McpServerConfig::Http(McpHttpServer {
            name: name.to_owned(),
            url: url.to_owned(),
            headers: Vec::new(),
        }));
    }
    let command = table
        .get("command")
        .and_then(TomlValue::as_str)
        .ok_or(BlockedReason::McpUnmappable)?
        .to_owned();
    let args = match table.get("args") {
        Some(value) => toml_string_array(value).ok_or(BlockedReason::McpUnmappable)?,
        None => Vec::new(),
    };
    // Same literal-`env` reasoning as the other dialects.
    if let Some(env) = table.get("env") {
        let env = env.as_table().ok_or(BlockedReason::McpUnmappable)?;
        if env.keys().any(|key| sensitive_field_reason(key).is_some()) {
            return Err(BlockedReason::Credentials);
        }
        return Err(BlockedReason::McpUnmappable);
    }
    // Dropping `cwd` would corrupt command resolution rather than degrade it.
    if table.get("cwd").is_some() {
        return Err(BlockedReason::McpUnmappable);
    }
    // `env_vars` forwards variable NAMES, not values, as strings or
    // `{ name, source? }` objects.
    let env = match table.get("env_vars") {
        Some(value) => toml_env_var_names(value).ok_or(BlockedReason::McpUnmappable)?,
        None => Vec::new(),
    };
    // `enabled_tools`/`disabled_tools` stay OUT of this allowlist: dropping a
    // tool filter would silently re-enable tools the user turned off.
    let allowed: BTreeSet<&str> = [
        "command",
        "args",
        "env",
        "env_vars",
        "cwd",
        "enabled",
        "required",
        "startup_timeout_sec",
        "startup_timeout_ms",
        "tool_timeout_sec",
        "tool_timeout_ms",
    ]
    .into_iter()
    .collect();
    if table.keys().any(|key| !allowed.contains(key.as_str())) {
        return Err(BlockedReason::McpUnmappable);
    }
    if command_args_contain_literal_credentials(&args) {
        return Err(BlockedReason::Credentials);
    }
    Ok(McpServerConfig::Stdio(McpStdioServer {
        name: name.to_owned(),
        command,
        args,
        env,
    }))
}

fn toml_env_var_names(value: &TomlValue) -> Option<Vec<String>> {
    let array = value.as_array()?;
    let mut names = Vec::with_capacity(array.len());
    for entry in array {
        if let Some(name) = entry.as_str() {
            names.push(name.to_owned());
            continue;
        }
        let table = entry.as_table()?;
        if table
            .keys()
            .any(|key| !matches!(key.as_str(), "name" | "source"))
        {
            return None;
        }
        names.push(table.get("name")?.as_str()?.to_owned());
    }
    Some(names)
}

pub(super) fn mcp_http_url_is_credential_free(value: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(value) else {
        return false;
    };
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return false;
    }
    if url
        .path_segments()
        .is_some_and(|mut segments| segments.any(path_segment_looks_like_credential))
    {
        return false;
    }
    !url.query_pairs().any(|(name, _)| {
        let normalized = name.to_ascii_lowercase().replace('-', "_");
        sensitive_field_reason(&name).is_some()
            || matches!(
                normalized.as_str(),
                "sig" | "signature" | "access_key" | "access_key_id"
            )
            || normalized.contains("signature")
    })
}

/// Tokens embedded as URL path segments carry no field name to classify, so
/// match key prefixes; the length floor excludes words like `sk-learn`.
pub(super) fn path_segment_looks_like_credential(segment: &str) -> bool {
    let lowered = segment.to_ascii_lowercase();
    CREDENTIAL_PATH_SEGMENT_PREFIXES
        .iter()
        .any(|prefix| lowered.starts_with(prefix) && lowered.len() > prefix.len() + 8)
}

pub(super) fn url_contains_userinfo(value: &str) -> bool {
    reqwest::Url::parse(value)
        .is_ok_and(|url| !url.username().is_empty() || url.password().is_some())
}

pub(super) fn command_args_contain_literal_credentials(args: &[String]) -> bool {
    args.iter().enumerate().any(|(index, argument)| {
        let trimmed = argument.trim_start_matches('-');
        let name = trimmed.split_once('=').map_or(trimmed, |(name, _)| name);
        let sensitive = sensitive_field_reason(name).is_some()
            || name.to_ascii_lowercase().contains("signature");
        sensitive && (trimmed.contains('=') || args.get(index + 1).is_some())
    }) || args.iter().any(|argument| {
        argument_carries_header_credential(argument)
            || string_contains_high_confidence_credential(argument)
    })
}

/// Header-style credentials arrive as values rather than flag names, so the
/// name-based flag scan alone misses them.
pub(super) fn argument_carries_header_credential(argument: &str) -> bool {
    if argument
        .trim_start()
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("bearer "))
    {
        return true;
    }
    let Some((name, value)) = argument.split_once(':') else {
        return false;
    };
    !value.trim().is_empty() && sensitive_field_reason(name.trim()).is_some()
}

pub(super) fn split_opencode_model(value: &str) -> (Option<&str>, &str) {
    match value.split_once('/') {
        Some((provider, model)) if !provider.is_empty() && !model.is_empty() => {
            (Some(provider), model)
        }
        _ => (None, value),
    }
}

pub(super) struct InspectionBuilder {
    inspection: NativeConfigInspection,
    candidates: BTreeMap<String, CandidateValue>,
    blocked_paths: BTreeSet<String>,
    executable: BTreeSet<ExecutableCategory>,
    executable_candidate_ids: BTreeSet<String>,
    residual_has_executable: bool,
}

impl InspectionBuilder {
    pub(super) fn new(
        harness: &str,
        format: NativeConfigFormat,
        revision: String,
        size_bytes: usize,
    ) -> Self {
        Self {
            inspection: NativeConfigInspection {
                revision,
                harness: harness.to_owned(),
                format,
                size_bytes,
                managed_fields: Vec::new(),
                blocked_fields: Vec::new(),
                unmanaged_field_paths: Vec::new(),
                executable_categories: Vec::new(),
                warnings: Vec::new(),
            },
            candidates: BTreeMap::new(),
            blocked_paths: BTreeSet::new(),
            executable: BTreeSet::new(),
            executable_candidate_ids: BTreeSet::new(),
            residual_has_executable: false,
        }
    }

    pub(super) fn has_candidate(&self, id: &str) -> bool {
        self.candidates.contains_key(id)
    }

    pub(super) fn candidate(&self, id: &str) -> Option<&CandidateValue> {
        self.candidates.get(id)
    }

    pub(super) fn add_candidate(
        &mut self,
        id: impl Into<String>,
        path: impl Into<String>,
        kind: ManagedFieldKind,
        compatible: bool,
        value: CandidateValue,
    ) {
        let id = id.into();
        if self.inspection.managed_fields.len() >= MAX_MANIFEST_PATHS {
            self.warn_once("manifest-truncated");
            return;
        }
        if self.candidates.contains_key(&id) {
            self.block(path, BlockedReason::ManagedUnsupported);
            return;
        }
        self.inspection.managed_fields.push(ManagedField {
            id: id.clone(),
            path: path.into(),
            kind,
            compatible,
        });
        self.candidates.insert(id, value);
    }

    pub(super) fn incompatible(
        &mut self,
        id: impl Into<String>,
        path: impl Into<String>,
        kind: ManagedFieldKind,
    ) {
        if self.inspection.managed_fields.len() >= MAX_MANIFEST_PATHS {
            self.warn_once("manifest-truncated");
            return;
        }
        self.inspection.managed_fields.push(ManagedField {
            id: id.into(),
            path: path.into(),
            kind,
            compatible: false,
        });
    }

    pub(super) fn add_string_candidate<F>(
        &mut self,
        id: &str,
        path: &str,
        kind: ManagedFieldKind,
        value: JsonValue,
        convert: F,
    ) where
        F: FnOnce(&str) -> Option<CandidateValue>,
    {
        match value.as_str().and_then(convert) {
            Some(value) => self.add_candidate(id, path, kind, true, value),
            None => self.incompatible(id, path, kind),
        }
    }

    pub(super) fn add_toml_string_candidate<F>(
        &mut self,
        id: &str,
        path: &str,
        kind: ManagedFieldKind,
        value: TomlValue,
        convert: F,
    ) where
        F: FnOnce(&str) -> Option<CandidateValue>,
    {
        match value.as_str().and_then(convert) {
            Some(value) => self.add_candidate(id, path, kind, true, value),
            None => self.incompatible(id, path, kind),
        }
    }

    pub(super) fn block(&mut self, path: impl Into<String>, reason: BlockedReason) {
        let path = path.into();
        if self.blocked_paths.contains(&path) {
            return;
        }
        if self.inspection.blocked_fields.len() >= MAX_MANIFEST_PATHS {
            self.warn_once("manifest-truncated");
            return;
        }
        self.blocked_paths.insert(path.clone());
        self.inspection
            .blocked_fields
            .push(BlockedField { path, reason });
    }

    pub(super) fn executable(&mut self, category: ExecutableCategory) {
        self.residual_has_executable = true;
        self.executable.insert(category);
    }

    pub(super) fn executable_candidate(&mut self, id: String, category: ExecutableCategory) {
        self.executable_candidate_ids.insert(id);
        self.executable.insert(category);
    }

    pub(super) fn warn(&mut self, code: &str) {
        self.inspection.warnings.push(code.to_owned());
    }

    pub(super) fn warn_once(&mut self, code: &str) {
        if !self
            .inspection
            .warnings
            .iter()
            .any(|warning| warning == code)
        {
            self.warn(code);
        }
    }

    pub(super) fn finish_json(mut self, residual: Vec<u8>) -> Result<InspectedNativeConfig> {
        let value: JsonValue = serde_json::from_slice(&residual)
            .map_err(|_| native_error("agent.native_config_invalid"))?;
        collect_json_paths(&value, "", &mut self.inspection.unmanaged_field_paths);
        self.finish(residual)
    }

    pub(super) fn finish_toml(mut self, residual: Vec<u8>) -> Result<InspectedNativeConfig> {
        let text = std::str::from_utf8(&residual)
            .map_err(|_| native_error("agent.native_config_invalid"))?;
        let value: TomlValue =
            toml::from_str(text).map_err(|_| native_error("agent.native_config_invalid"))?;
        collect_toml_paths(&value, "", &mut self.inspection.unmanaged_field_paths);
        self.finish(residual)
    }

    pub(super) fn finish_yaml(mut self, residual: Vec<u8>) -> Result<InspectedNativeConfig> {
        // Projected through JSON so the dotted-path shape matches the JSON
        // harnesses; re-parsed via the same non-string-key guard used on input.
        let text = std::str::from_utf8(&residual)
            .map_err(|_| native_error("agent.native_config_invalid"))?;
        let root = parse_goose_root(text)?;
        let value = JsonValue::Object(root);
        collect_json_paths(&value, "", &mut self.inspection.unmanaged_field_paths);
        self.finish(residual)
    }

    pub(super) fn finish(mut self, residual: Vec<u8>) -> Result<InspectedNativeConfig> {
        if residual.len() > IMPORT_SIZE_LIMIT {
            return Err(native_error("agent.native_config_normalized_too_large"));
        }
        self.inspection
            .managed_fields
            .sort_by(|a, b| a.id.cmp(&b.id));
        self.inspection
            .blocked_fields
            .sort_by(|a, b| a.path.cmp(&b.path));
        self.inspection.unmanaged_field_paths.sort();
        self.inspection.unmanaged_field_paths.dedup();
        if self.inspection.unmanaged_field_paths.len() > MAX_MANIFEST_PATHS {
            self.warn_once("manifest-truncated");
        }
        self.inspection
            .unmanaged_field_paths
            .truncate(MAX_MANIFEST_PATHS);
        self.inspection.executable_categories = self.executable.into_iter().collect();
        Ok(InspectedNativeConfig {
            inspection: self.inspection,
            residual,
            candidates: self.candidates,
            executable_candidate_ids: self.executable_candidate_ids,
            residual_has_executable: self.residual_has_executable,
        })
    }
}
