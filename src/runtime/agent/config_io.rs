//! Generic JSON / YAML / TOML read/write helpers used by per-agent headless
//! config provisioners.
//!
//! Extracted from `agent_headless_config.rs` so the provisioner file can
//! focus on agent-specific shapes. These helpers preserve unrelated fields
//! across writes and only mutate the keys the caller targets.

use std::path::Path;

use serde_json::{Map, Value as JsonValue, json};
use serde_norway::{Mapping as YamlMapping, Value as YamlValue};
use toml::{Value as TomlValue, map::Map as TomlMap};

use crate::error::{Result, StackError};
use crate::fs_util::{atomic_write_owner_only, create_dir_owner_only, parent_dir};

pub(super) fn read_json_object(path: &Path) -> Result<Map<String, JsonValue>> {
    if !path.exists() {
        return Ok(Map::new());
    }

    let content = std::fs::read_to_string(path).map_err(|source| StackError::ConfigRead {
        path: path.to_path_buf(),
        source,
    })?;
    let value: JsonValue =
        serde_json::from_str(&content).map_err(|source| StackError::AgentConfigProvision {
            path: path.to_path_buf(),
            reason: format!("existing JSON is invalid: {source}"),
        })?;
    match value {
        JsonValue::Object(object) => Ok(object),
        _ => Err(StackError::AgentConfigProvision {
            path: path.to_path_buf(),
            reason: "existing JSON root must be an object".to_owned(),
        }),
    }
}

pub(super) fn write_json_object(path: &Path, object: Map<String, JsonValue>) -> Result<()> {
    let parent = parent_dir(path)?;
    create_dir_owner_only(parent)?;
    let content = serde_json::to_vec_pretty(&JsonValue::Object(object)).map_err(|source| {
        StackError::AgentConfigProvision {
            path: path.to_path_buf(),
            reason: format!("failed to serialize JSON: {source}"),
        }
    })?;
    let mut with_newline = content;
    with_newline.push(b'\n');
    atomic_write_owner_only(path, &with_newline)
}

pub(super) fn ensure_object_field<'a>(
    object: &'a mut Map<String, JsonValue>,
    key: &str,
    path: &Path,
) -> Result<&'a mut Map<String, JsonValue>> {
    if !object.contains_key(key) {
        object.insert(key.to_owned(), json!({}));
    }
    object
        .get_mut(key)
        .and_then(JsonValue::as_object_mut)
        .ok_or_else(|| StackError::AgentConfigProvision {
            path: path.to_path_buf(),
            reason: format!("`{key}` must be an object when present"),
        })
}

pub(super) fn insert_if_missing(
    object: &mut Map<String, JsonValue>,
    key: &str,
    value: JsonValue,
    path: &Path,
) -> Result<()> {
    if let Some(existing) = object.get(key) {
        if existing.is_null() {
            return Err(StackError::AgentConfigProvision {
                path: path.to_path_buf(),
                reason: format!("`{key}` must not be null when present"),
            });
        }
        return Ok(());
    }
    object.insert(key.to_owned(), value);
    Ok(())
}

pub(super) fn read_yaml_mapping(path: &Path) -> Result<YamlMapping> {
    if !path.exists() {
        return Ok(YamlMapping::new());
    }

    let content = std::fs::read_to_string(path).map_err(|source| StackError::ConfigRead {
        path: path.to_path_buf(),
        source,
    })?;
    if content.trim().is_empty() {
        return Ok(YamlMapping::new());
    }
    let value: YamlValue =
        serde_norway::from_str(&content).map_err(|source| StackError::AgentConfigProvision {
            path: path.to_path_buf(),
            reason: format!("existing YAML is invalid: {source}"),
        })?;
    match value {
        YamlValue::Mapping(mapping) => Ok(mapping),
        _ => Err(StackError::AgentConfigProvision {
            path: path.to_path_buf(),
            reason: "existing YAML root must be a mapping".to_owned(),
        }),
    }
}

pub(super) fn write_yaml_mapping(path: &Path, mapping: YamlMapping) -> Result<()> {
    let parent = parent_dir(path)?;
    create_dir_owner_only(parent)?;
    let content = serde_norway::to_string(&YamlValue::Mapping(mapping)).map_err(|source| {
        StackError::AgentConfigProvision {
            path: path.to_path_buf(),
            reason: format!("failed to serialize YAML: {source}"),
        }
    })?;
    let mut bytes = content.into_bytes();
    if !bytes.ends_with(b"\n") {
        bytes.push(b'\n');
    }
    atomic_write_owner_only(path, &bytes)
}

pub(super) fn read_toml_table(path: &Path) -> Result<TomlMap<String, TomlValue>> {
    if !path.exists() {
        return Ok(TomlMap::new());
    }

    let content = std::fs::read_to_string(path).map_err(|source| StackError::ConfigRead {
        path: path.to_path_buf(),
        source,
    })?;
    if content.trim().is_empty() {
        return Ok(TomlMap::new());
    }
    let value: TomlValue =
        toml::from_str(&content).map_err(|source| StackError::AgentConfigProvision {
            path: path.to_path_buf(),
            reason: format!("existing TOML is invalid: {source}"),
        })?;
    match value {
        TomlValue::Table(table) => Ok(table),
        _ => Err(StackError::AgentConfigProvision {
            path: path.to_path_buf(),
            reason: "existing TOML root must be a table".to_owned(),
        }),
    }
}

pub(super) fn write_toml_table(path: &Path, table: TomlMap<String, TomlValue>) -> Result<()> {
    let parent = parent_dir(path)?;
    create_dir_owner_only(parent)?;
    let content = toml::to_string_pretty(&TomlValue::Table(table)).map_err(|source| {
        StackError::AgentConfigProvision {
            path: path.to_path_buf(),
            reason: format!("failed to serialize TOML: {source}"),
        }
    })?;
    let mut bytes = content.into_bytes();
    if !bytes.ends_with(b"\n") {
        bytes.push(b'\n');
    }
    atomic_write_owner_only(path, &bytes)
}

pub(super) fn ensure_toml_table_field<'a>(
    table: &'a mut TomlMap<String, TomlValue>,
    key: &str,
    path: &Path,
) -> Result<&'a mut TomlMap<String, TomlValue>> {
    if !table.contains_key(key) {
        table.insert(key.to_owned(), TomlValue::Table(TomlMap::new()));
    }
    table
        .get_mut(key)
        .and_then(TomlValue::as_table_mut)
        .ok_or_else(|| StackError::AgentConfigProvision {
            path: path.to_path_buf(),
            reason: format!("`{key}` must be a table when present"),
        })
}
