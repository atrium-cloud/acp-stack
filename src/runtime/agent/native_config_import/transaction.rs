//! Transaction and snapshot mechanics for native config imports.

use super::*;

pub fn native_config_projection(config: &Config) -> NativeConfigProjection {
    NativeConfigProjection {
        id: config.agent.id.clone(),
        provider: config
            .agent
            .provider
            .as_ref()
            .map(|provider| provider.id.clone()),
        model: config
            .agent
            .provider
            .as_ref()
            .and_then(|provider| provider.model.clone())
            .or_else(|| config.agent.model.clone()),
    }
}

pub fn native_config_path(harness: &str, home: &Path) -> Result<PathBuf> {
    match harness {
        "claude-code" => Ok(home.join(".claude").join("settings.json")),
        "codex" => Ok(home.join(".codex").join("config.toml")),
        "opencode" => Ok(home.join(".config").join("opencode").join("opencode.json")),
        "amp" => Ok(home.join(".config").join("amp").join("settings.json")),
        "pi" => Ok(home.join(".pi").join("agent").join("settings.json")),
        "goose" => Ok(home.join(".config").join("goose").join("config.yaml")),
        _ => Err(native_error("native_config_harness_unsupported")),
    }
}

pub fn validate_native_config_secret_refs_read_only(
    prepared: &PreparedNativeConfigImport,
    home: &Path,
) -> Result<()> {
    let secrets = SecretStore::open_read_only(home)?;
    validate_native_config_secret_refs_with_store(prepared, &secrets)
}

pub fn validate_native_config_secret_refs(
    prepared: &PreparedNativeConfigImport,
    home: &Path,
) -> Result<()> {
    let secrets = SecretStore::open(home)?;
    validate_native_config_secret_refs_with_store(prepared, &secrets)
}

fn validate_native_config_secret_refs_with_store(
    prepared: &PreparedNativeConfigImport,
    secrets: &SecretStore,
) -> Result<()> {
    crate::runtime::agent::provider_keys::resolve_agent_environment(
        &prepared.canonical_config,
        secrets,
    )?;
    validate_mcp_secret_refs(&prepared.canonical_config.mcp, secrets)?;
    Ok(())
}

pub fn native_config_transaction_paths(
    config_path: &Path,
    native_path: &Path,
    harness: &str,
    home: &Path,
) -> Vec<PathBuf> {
    let mut paths = vec![config_path.to_path_buf(), native_path.to_path_buf()];
    if harness == "claude-code" {
        paths.push(home.join(".claude.json"));
    }
    paths.sort();
    paths.dedup();
    paths
}

pub fn prepare_native_config_file_paths(
    prepared: &PreparedNativeConfigImport,
    config_path: &Path,
    home: &Path,
) -> Result<Vec<PathBuf>> {
    let paths = native_config_transaction_paths(
        config_path,
        &prepared.native_path,
        &prepared.harness,
        home,
    );
    for path in &paths {
        prepare_owner_managed_file_path(home, path)?;
    }
    Ok(paths)
}

pub fn capture_native_config_snapshots(
    paths: &[PathBuf],
    home: &Path,
) -> Result<Vec<NativeConfigPathSnapshot>> {
    let mut snapshots = Vec::with_capacity(paths.len());
    for path in paths {
        prepare_owner_managed_file_path(home, path)?;
        let content = if path == &home.join(".claude.json") {
            match std::fs::read(path) {
                Ok(content) => {
                    let root = match serde_json::from_slice::<JsonValue>(&content) {
                        Ok(JsonValue::Object(root)) => root,
                        _ => return Err(native_error("native_config_claude_state_invalid")),
                    };
                    let value = match root.get("hasCompletedOnboarding") {
                        Some(JsonValue::Bool(value)) => Some(*value),
                        None => None,
                        Some(_) => {
                            return Err(native_error("native_config_claude_state_invalid"));
                        }
                    };
                    NativeConfigSnapshotContent::ClaudeOnboarding {
                        file_existed: true,
                        value,
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    NativeConfigSnapshotContent::ClaudeOnboarding {
                        file_existed: false,
                        value: None,
                    }
                }
                Err(source) => {
                    return Err(StackError::ConfigRead {
                        path: path.clone(),
                        source,
                    });
                }
            }
        } else {
            let content = match std::fs::read(path) {
                Ok(content) => Some(content),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(source) => {
                    return Err(StackError::ConfigRead {
                        path: path.clone(),
                        source,
                    });
                }
            };
            NativeConfigSnapshotContent::File(content)
        };
        snapshots.push(NativeConfigPathSnapshot {
            path: path.clone(),
            content,
        });
    }
    Ok(snapshots)
}

pub fn restore_native_config_snapshots(
    snapshots: &[NativeConfigPathSnapshot],
    home: &Path,
) -> Result<()> {
    for snapshot in snapshots {
        prepare_owner_managed_file_path(home, &snapshot.path)?;
        match &snapshot.content {
            NativeConfigSnapshotContent::File(Some(content)) => {
                atomic_write_owner_only(&snapshot.path, content)?;
            }
            NativeConfigSnapshotContent::File(None)
            | NativeConfigSnapshotContent::ClaudeOnboarding {
                file_existed: false,
                ..
            } => match std::fs::remove_file(&snapshot.path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(StackError::FileRemove {
                        path: snapshot.path.clone(),
                        source,
                    });
                }
            },
            NativeConfigSnapshotContent::ClaudeOnboarding {
                file_existed: true,
                value,
            } => {
                let content =
                    std::fs::read(&snapshot.path).map_err(|source| StackError::ConfigRead {
                        path: snapshot.path.clone(),
                        source,
                    })?;
                let mut root = match serde_json::from_slice::<JsonValue>(&content) {
                    Ok(JsonValue::Object(root)) => root,
                    _ => return Err(native_error("native_config_claude_state_invalid")),
                };
                match value {
                    Some(value) => {
                        root.insert("hasCompletedOnboarding".to_owned(), JsonValue::Bool(*value));
                    }
                    None => {
                        root.remove("hasCompletedOnboarding");
                    }
                }
                atomic_write_owner_only(&snapshot.path, &json_bytes(root)?)?;
            }
        }
    }
    Ok(())
}

pub fn write_native_config_files(
    prepared: &PreparedNativeConfigImport,
    config_path: &Path,
    home: &Path,
) -> Result<()> {
    atomic_write_owner_only(config_path, prepared.canonical_toml.as_bytes())?;
    atomic_write_owner_only(&prepared.native_path, &prepared.native_content)?;
    provision_agent_headless_config(&prepared.canonical_config, home)?;
    Ok(())
}

pub fn capture_native_config_file_digests(
    paths: &[PathBuf],
    home: &Path,
) -> Result<Vec<NativeConfigFileDigest>> {
    paths
        .iter()
        .map(|path| {
            prepare_owner_managed_file_path(home, path)?;
            let sha256 = native_config_file_digest(path, home)?;
            Ok(NativeConfigFileDigest {
                path: path.clone(),
                sha256,
            })
        })
        .collect()
}

pub fn validate_native_config_file_digests(
    digests: &[NativeConfigFileDigest],
    home: &Path,
) -> Result<()> {
    if digests.is_empty() {
        return Err(native_error("native_config_rollback_conflict"));
    }
    for expected in digests {
        prepare_owner_managed_file_path(home, &expected.path)?;
        let actual = native_config_file_digest(&expected.path, home)?;
        if actual != expected.sha256 {
            return Err(native_error("native_config_rollback_conflict"));
        }
    }
    Ok(())
}

fn native_config_file_digest(path: &Path, home: &Path) -> Result<Option<String>> {
    let content = match std::fs::read(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(StackError::ConfigRead {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    if path != home.join(".claude.json") {
        return Ok(Some(sha256_hex(&content)));
    }
    let root = match serde_json::from_slice::<JsonValue>(&content) {
        Ok(JsonValue::Object(root)) => root,
        _ => return Err(native_error("native_config_claude_state_invalid")),
    };
    let owned_value = match root.get("hasCompletedOnboarding") {
        Some(JsonValue::Bool(true)) => b"true".as_slice(),
        Some(JsonValue::Bool(false)) => b"false".as_slice(),
        None => b"missing".as_slice(),
        Some(_) => return Err(native_error("native_config_claude_state_invalid")),
    };
    Ok(Some(sha256_hex(owned_value)))
}
