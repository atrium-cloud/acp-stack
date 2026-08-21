//! Serialization, parsing, and sanitization utilities for native configs.

use super::*;

pub(super) fn parse_json_object(content: &str) -> Result<JsonMap<String, JsonValue>> {
    match serde_json::from_str::<JsonValue>(content) {
        Ok(JsonValue::Object(root)) => Ok(root),
        _ => Err(native_error("agent.native_config_invalid")),
    }
}

pub(super) fn parse_jsonc_object(content: &str) -> Result<JsonMap<String, JsonValue>> {
    let stripped = strip_jsonc_comments(content)?;
    let normalized = strip_jsonc_trailing_commas(&stripped)?;
    parse_json_object(&normalized)
}

pub(super) fn parse_toml_table(content: &str) -> Result<TomlMap<String, TomlValue>> {
    match toml::from_str::<TomlValue>(content) {
        Ok(TomlValue::Table(root)) => Ok(root),
        _ => Err(native_error("agent.native_config_invalid")),
    }
}

/// Parse a Goose `config.yaml` root into a JSON object. Goose config is YAML,
/// but the whole classification/sanitize/paths pipeline is JSON-shaped, so the
/// document is converted to JSON up front. YAML permits non-string mapping keys
/// (numbers, booleans, sequences); those have no JSON representation and no
/// legitimate place in a Goose config, so any mapping carrying one is rejected
/// as invalid rather than lossily coerced.
pub(super) fn parse_goose_root(content: &str) -> Result<JsonMap<String, JsonValue>> {
    let value: YamlValue =
        serde_norway::from_str(content).map_err(|_| native_error("agent.native_config_invalid"))?;
    match yaml_value_to_json(value)? {
        JsonValue::Object(root) => Ok(root),
        _ => Err(native_error("agent.native_config_invalid")),
    }
}

fn yaml_value_to_json(value: YamlValue) -> Result<JsonValue> {
    match value {
        YamlValue::Null => Ok(JsonValue::Null),
        YamlValue::Bool(value) => Ok(JsonValue::Bool(value)),
        YamlValue::Number(number) => yaml_number_to_json(number),
        YamlValue::String(value) => Ok(JsonValue::String(value)),
        YamlValue::Sequence(values) => Ok(JsonValue::Array(
            values
                .into_iter()
                .map(yaml_value_to_json)
                .collect::<Result<Vec<_>>>()?,
        )),
        YamlValue::Mapping(mapping) => {
            let mut object = JsonMap::with_capacity(mapping.len());
            for (key, value) in mapping {
                // Reject non-string keys instead of stringifying them: a Goose
                // config never uses them, and a silent coercion could collide
                // two distinct keys or smuggle content past the sanitize pass.
                let YamlValue::String(key) = key else {
                    return Err(native_error("agent.native_config_invalid"));
                };
                object.insert(key, yaml_value_to_json(value)?);
            }
            Ok(JsonValue::Object(object))
        }
        YamlValue::Tagged(_) => Err(native_error("agent.native_config_invalid")),
    }
}

fn yaml_number_to_json(number: serde_norway::Number) -> Result<JsonValue> {
    if let Some(value) = number.as_i64() {
        return Ok(JsonValue::Number(value.into()));
    }
    if let Some(value) = number.as_u64() {
        return Ok(JsonValue::Number(value.into()));
    }
    number
        .as_f64()
        .and_then(serde_json::Number::from_f64)
        .map(JsonValue::Number)
        .ok_or_else(|| native_error("agent.native_config_invalid"))
}

fn json_value_to_yaml(value: JsonValue) -> YamlValue {
    match value {
        JsonValue::Null => YamlValue::Null,
        JsonValue::Bool(value) => YamlValue::Bool(value),
        JsonValue::Number(number) => json_number_to_yaml(number),
        JsonValue::String(value) => YamlValue::String(value),
        JsonValue::Array(values) => {
            YamlValue::Sequence(values.into_iter().map(json_value_to_yaml).collect())
        }
        JsonValue::Object(object) => {
            let mut mapping = YamlMapping::with_capacity(object.len());
            for (key, value) in object {
                mapping.insert(YamlValue::String(key), json_value_to_yaml(value));
            }
            YamlValue::Mapping(mapping)
        }
    }
}

fn json_number_to_yaml(number: serde_json::Number) -> YamlValue {
    if let Some(value) = number.as_i64() {
        return YamlValue::Number(value.into());
    }
    if let Some(value) = number.as_u64() {
        return YamlValue::Number(value.into());
    }
    match number.as_f64() {
        Some(value) => YamlValue::Number(value.into()),
        None => YamlValue::Null,
    }
}

pub(super) fn goose_yaml_bytes(root: JsonMap<String, JsonValue>) -> Result<Vec<u8>> {
    let value = json_value_to_yaml(JsonValue::Object(root));
    let text =
        serde_norway::to_string(&value).map_err(|_| native_error("agent.native_config_invalid"))?;
    Ok(text.into_bytes())
}

pub(super) fn json_bytes(root: JsonMap<String, JsonValue>) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(&JsonValue::Object(root))
        .map_err(|_| native_error("agent.native_config_invalid"))?;
    bytes.push(b'\n');
    Ok(bytes)
}

pub(super) fn toml_bytes(root: TomlMap<String, TomlValue>) -> Result<Vec<u8>> {
    let text = toml::to_string_pretty(&TomlValue::Table(root))
        .map_err(|_| native_error("agent.native_config_invalid"))?;
    Ok(text.into_bytes())
}

pub(super) fn json_string_array(value: &JsonValue) -> Option<Vec<String>> {
    value
        .as_array()?
        .iter()
        .map(|value| value.as_str().map(str::to_owned))
        .collect()
}

pub(super) fn toml_string_array(value: &TomlValue) -> Option<Vec<String>> {
    value
        .as_array()?
        .iter()
        .map(|value| value.as_str().map(str::to_owned))
        .collect()
}

pub(super) fn collect_json_paths(value: &JsonValue, prefix: &str, out: &mut Vec<String>) {
    if out.len() > MAX_MANIFEST_PATHS {
        return;
    }
    match value {
        JsonValue::Object(object) if !object.is_empty() => {
            for (key, value) in object {
                let path = join_path(prefix, key);
                collect_json_paths(value, &path, out);
            }
        }
        JsonValue::Array(_) => {
            if !prefix.is_empty() {
                out.push(prefix.to_owned());
            }
        }
        _ => {
            if !prefix.is_empty() {
                out.push(prefix.to_owned());
            }
        }
    }
}

pub(super) fn collect_toml_paths(value: &TomlValue, prefix: &str, out: &mut Vec<String>) {
    if out.len() > MAX_MANIFEST_PATHS {
        return;
    }
    match value {
        TomlValue::Table(table) if !table.is_empty() => {
            for (key, value) in table {
                let path = join_path(prefix, key);
                collect_toml_paths(value, &path, out);
            }
        }
        TomlValue::Array(_) => {
            if !prefix.is_empty() {
                out.push(prefix.to_owned());
            }
        }
        _ => {
            if !prefix.is_empty() {
                out.push(prefix.to_owned());
            }
        }
    }
}

fn join_path(prefix: &str, key: &str) -> String {
    if prefix.is_empty() {
        key.to_owned()
    } else {
        format!("{prefix}.{key}")
    }
}

pub(super) fn sanitize_sensitive_json_object(
    object: &mut JsonMap<String, JsonValue>,
    prefix: &str,
    builder: &mut InspectionBuilder,
) {
    let keys = object.keys().cloned().collect::<Vec<_>>();
    for key in keys {
        let path = join_path(prefix, &key);
        if let Some(reason) = sensitive_field_reason(&key) {
            object.remove(&key);
            builder.block(path, reason);
            continue;
        }
        if object
            .get(&key)
            .is_some_and(json_value_contains_high_confidence_credential)
        {
            object.remove(&key);
            builder.block(path, BlockedReason::Credentials);
            continue;
        }
        if let Some(value) = object.get_mut(&key) {
            sanitize_sensitive_json_value(value, &path, builder);
        }
    }
}

fn sanitize_sensitive_json_value(
    value: &mut JsonValue,
    prefix: &str,
    builder: &mut InspectionBuilder,
) {
    match value {
        JsonValue::Object(object) => sanitize_sensitive_json_object(object, prefix, builder),
        JsonValue::Array(values) => {
            for (index, value) in values.iter_mut().enumerate() {
                sanitize_sensitive_json_value(value, &format!("{prefix}[{index}]"), builder);
            }
        }
        _ => {}
    }
}

pub(super) fn sanitize_sensitive_toml_table(
    table: &mut TomlMap<String, TomlValue>,
    prefix: &str,
    builder: &mut InspectionBuilder,
) {
    let keys = table.keys().cloned().collect::<Vec<_>>();
    for key in keys {
        let path = join_path(prefix, &key);
        if let Some(reason) = sensitive_field_reason(&key) {
            table.remove(&key);
            builder.block(path, reason);
            continue;
        }
        if table
            .get(&key)
            .is_some_and(toml_value_contains_high_confidence_credential)
        {
            table.remove(&key);
            builder.block(path, BlockedReason::Credentials);
            continue;
        }
        if let Some(value) = table.get_mut(&key) {
            sanitize_sensitive_toml_value(value, &path, builder);
        }
    }
}

fn sanitize_sensitive_toml_value(
    value: &mut TomlValue,
    prefix: &str,
    builder: &mut InspectionBuilder,
) {
    match value {
        TomlValue::Table(table) => sanitize_sensitive_toml_table(table, prefix, builder),
        TomlValue::Array(values) => {
            for (index, value) in values.iter_mut().enumerate() {
                sanitize_sensitive_toml_value(value, &format!("{prefix}[{index}]"), builder);
            }
        }
        _ => {}
    }
}

pub(super) fn sensitive_field_reason(key: &str) -> Option<BlockedReason> {
    let normalized = key.to_ascii_lowercase().replace('-', "_");
    let flattened = normalized.replace('_', "");
    if matches!(
        normalized.as_str(),
        "auth" | "authentication" | "login" | "login_state" | "oauth" | "oauth_state"
    ) || flattened.contains("credential")
        || flattened.contains("login")
        // `coauth…` guards Claude Code's `includeCoAuthoredBy` (and similar
        // co-author fields) from matching the `oauth` substring.
        || (flattened.contains("oauth") && !flattened.contains("coauth"))
    {
        return Some(BlockedReason::AuthenticationState);
    }
    if normalized == "key"
        || normalized.ends_with("_key")
        || flattened.contains("apikey")
        || flattened.contains("secret")
        || flattened.contains("password")
        || flattened.contains("passwd")
        || flattened.contains("bearer")
        || flattened.contains("authorization")
        // A credential-style `token` either ends the key (`authToken`,
        // `github_token`) or is followed by a value word (`tokenValue`,
        // `tokenRef`). Quantity fields — `max_tokens`,
        // `model_auto_compact_token_limit`, `…_token_weight` — follow
        // `token(s)` with a count word instead and are routine model tuning,
        // not credentials.
        || flattened.ends_with("token")
        || flattened.contains("tokenvalue")
        || flattened.contains("tokenref")
        || flattened.contains("tokenid")
    {
        return Some(BlockedReason::Credentials);
    }
    None
}

fn json_value_contains_high_confidence_credential(value: &JsonValue) -> bool {
    match value {
        JsonValue::String(value) => string_contains_high_confidence_credential(value),
        JsonValue::Array(values) => values
            .iter()
            .any(json_value_contains_high_confidence_credential),
        JsonValue::Object(object) => object
            .values()
            .any(json_value_contains_high_confidence_credential),
        _ => false,
    }
}

fn toml_value_contains_high_confidence_credential(value: &TomlValue) -> bool {
    match value {
        TomlValue::String(value) => string_contains_high_confidence_credential(value),
        TomlValue::Array(values) => values
            .iter()
            .any(toml_value_contains_high_confidence_credential),
        TomlValue::Table(table) => table
            .values()
            .any(toml_value_contains_high_confidence_credential),
        _ => false,
    }
}

pub(super) fn string_contains_high_confidence_credential(value: &str) -> bool {
    let trimmed = value.trim();
    if path_segment_looks_like_credential(trimmed) || argument_carries_header_credential(trimmed) {
        return true;
    }
    reqwest::Url::parse(trimmed).is_ok_and(|url| matches!(url.scheme(), "http" | "https"))
        && !mcp_http_url_is_credential_free(trimmed)
}

pub(super) fn executable_environment_key(key: &str) -> bool {
    let key = key.to_ascii_uppercase();
    matches!(
        key.as_str(),
        "BASH_ENV"
            | "CLAUDE_CODE_GIT_BASH_PATH"
            | "COMSPEC"
            | "EDITOR"
            | "ENV"
            | "GIT_ASKPASS"
            | "GIT_PAGER"
            | "GIT_SSH"
            | "GIT_SSH_COMMAND"
            | "LD_PRELOAD"
            | "NODE_OPTIONS"
            | "PAGER"
            | "PERL5OPT"
            | "PATH"
            | "PYTHONPATH"
            | "PYTHONSTARTUP"
            | "RUSTC_WRAPPER"
            | "RUBYOPT"
            | "SHELL"
            | "SSH_ASKPASS"
            | "VISUAL"
    ) || key.starts_with("DYLD_")
        || key.ends_with("_COMMAND")
        || key.ends_with("_EXECUTABLE")
        || key.ends_with("_HELPER")
}

fn strip_jsonc_comments(input: &str) -> Result<String> {
    let bytes = input.as_bytes();
    let mut output = Vec::with_capacity(input.len());
    let mut index = 0;
    let mut in_string = false;
    let mut escaped = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if in_string {
            output.push(byte);
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        if byte == b'"' {
            in_string = true;
            output.push(b'"');
            index += 1;
            continue;
        }
        if byte == b'/' && index + 1 < bytes.len() && bytes[index + 1] == b'/' {
            index += 2;
            while index < bytes.len() && bytes[index] != b'\n' {
                output.push(b' ');
                index += 1;
            }
            continue;
        }
        if byte == b'/' && index + 1 < bytes.len() && bytes[index + 1] == b'*' {
            index += 2;
            let mut closed = false;
            while index + 1 < bytes.len() {
                if bytes[index] == b'*' && bytes[index + 1] == b'/' {
                    index += 2;
                    closed = true;
                    break;
                }
                output.push(if bytes[index] == b'\n' { b'\n' } else { b' ' });
                index += 1;
            }
            if !closed {
                return Err(native_error("agent.native_config_invalid"));
            }
            continue;
        }
        output.push(byte);
        index += 1;
    }
    if in_string {
        return Err(native_error("agent.native_config_invalid"));
    }
    String::from_utf8(output).map_err(|_| native_error("agent.native_config_invalid"))
}

fn strip_jsonc_trailing_commas(input: &str) -> Result<String> {
    let bytes = input.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    let mut in_string = false;
    let mut escaped = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if in_string {
            output.push(byte);
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        if byte == b'"' {
            in_string = true;
            output.push(byte);
            index += 1;
            continue;
        }
        if byte == b',' {
            let mut lookahead = index + 1;
            while lookahead < bytes.len() && bytes[lookahead].is_ascii_whitespace() {
                lookahead += 1;
            }
            if lookahead < bytes.len() && matches!(bytes[lookahead], b'}' | b']') {
                index += 1;
                continue;
            }
        }
        output.push(byte);
        index += 1;
    }
    String::from_utf8(output).map_err(|_| native_error("agent.native_config_invalid"))
}
