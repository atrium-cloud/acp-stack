//! Transaction and snapshot mechanics for native config imports.

use super::*;

/// The files a native config transaction spans: the acps config file stays on the runtime side,
/// and every native target resolves below the workload home.
#[derive(Clone, Copy)]
pub struct NativeConfigFiles<'a> {
    pub config_path: &'a Path,
    pub workload: &'a WorkloadHome,
}

impl<'a> NativeConfigFiles<'a> {
    pub fn new(config_path: &'a Path, workload: &'a WorkloadHome) -> Self {
        Self {
            config_path,
            workload,
        }
    }

    fn is_runtime_side(&self, path: &Path) -> bool {
        path == self.config_path
    }

    fn claude_state_path(&self) -> PathBuf {
        self.workload.home().join(".claude.json")
    }

    fn prepare(&self, path: &Path) -> Result<()> {
        if self.is_runtime_side(path) {
            return prepare_owner_managed_file_path(self.workload.runtime_home(), path);
        }
        self.workload.prepare_owned_file(path)
    }

    fn read(&self, path: &Path) -> Result<Option<Vec<u8>>> {
        if !self.is_runtime_side(path) {
            return self.workload.read(path);
        }
        match std::fs::read(path) {
            Ok(content) => Ok(Some(content)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(StackError::ConfigRead {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    fn write(&self, path: &Path, content: &[u8]) -> Result<()> {
        if self.is_runtime_side(path) {
            return atomic_write_owner_only(path, content);
        }
        self.workload.write_atomic(path, content.to_vec())
    }

    fn remove_if_present(&self, path: &Path) -> Result<()> {
        if !self.is_runtime_side(path) {
            return self.workload.remove_file_if_present(path).map(drop);
        }
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(StackError::FileRemove {
                path: path.to_path_buf(),
                source,
            }),
        }
    }
}

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
        "claude" => Ok(home.join(".claude").join("settings.json")),
        "codex" => Ok(home.join(".codex").join("config.toml")),
        "opencode" => Ok(home.join(".config").join("opencode").join("opencode.json")),
        "amp" => Ok(home.join(".config").join("amp").join("settings.json")),
        "pi" => Ok(home.join(".pi").join("agent").join("settings.json")),
        "goose" => Ok(home.join(".config").join("goose").join("config.yaml")),
        KIMI_CODE_AGENT_ID => Ok(home.join(".kimi-code").join("config.toml")),
        HERMES_AGENT_ID => Ok(home.join(".hermes").join("config.yaml")),
        _ => Err(native_error("agent.native_config_harness_unsupported")),
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

/// MCP-only slice of [`validate_native_config_secret_refs`]: a hosted init
/// deferring a provider credential would hard-fail the full environment check,
/// but its MCP refs are unrelated to the deferral and must still hold.
pub fn validate_native_config_mcp_secret_refs(
    prepared: &PreparedNativeConfigImport,
    home: &Path,
) -> Result<()> {
    let secrets = SecretStore::open(home)?;
    validate_mcp_secret_refs(&prepared.canonical_config.mcp, &secrets)
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
    if harness == "claude" {
        paths.push(home.join(".claude.json"));
    }
    paths.sort();
    paths.dedup();
    paths
}

pub fn prepare_native_config_file_paths(
    prepared: &PreparedNativeConfigImport,
    files: NativeConfigFiles<'_>,
) -> Result<Vec<PathBuf>> {
    let paths = native_config_transaction_paths(
        files.config_path,
        &prepared.native_path,
        &prepared.harness,
        files.workload.home(),
    );
    for path in &paths {
        files.prepare(path)?;
    }
    Ok(paths)
}

pub fn capture_native_config_snapshots(
    paths: &[PathBuf],
    files: NativeConfigFiles<'_>,
) -> Result<Vec<NativeConfigPathSnapshot>> {
    let claude_state_path = files.claude_state_path();
    let mut snapshots = Vec::with_capacity(paths.len());
    for path in paths {
        files.prepare(path)?;
        let content = if path == &claude_state_path {
            match files.read(path)? {
                Some(content) => {
                    let root = match serde_json::from_slice::<JsonValue>(&content) {
                        Ok(JsonValue::Object(root)) => root,
                        Ok(_) => {
                            return Err(native_error("agent.native_config_claude_state_invalid"));
                        }
                        Err(error) => {
                            return Err(json_parse_error(
                                "agent.native_config_claude_state_invalid",
                                &error,
                            ));
                        }
                    };
                    let value = match root.get("hasCompletedOnboarding") {
                        Some(JsonValue::Bool(value)) => Some(*value),
                        None => None,
                        Some(_) => {
                            return Err(native_error("agent.native_config_claude_state_invalid"));
                        }
                    };
                    NativeConfigSnapshotContent::ClaudeOnboarding {
                        file_existed: true,
                        value,
                    }
                }
                None => NativeConfigSnapshotContent::ClaudeOnboarding {
                    file_existed: false,
                    value: None,
                },
            }
        } else {
            NativeConfigSnapshotContent::File(files.read(path)?)
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
    files: NativeConfigFiles<'_>,
) -> Result<()> {
    for snapshot in snapshots {
        files.prepare(&snapshot.path)?;
        match &snapshot.content {
            NativeConfigSnapshotContent::File(Some(content)) => {
                files.write(&snapshot.path, content)?;
            }
            NativeConfigSnapshotContent::File(None)
            | NativeConfigSnapshotContent::ClaudeOnboarding {
                file_existed: false,
                ..
            } => files.remove_if_present(&snapshot.path)?,
            NativeConfigSnapshotContent::ClaudeOnboarding {
                file_existed: true,
                value,
            } => {
                let content =
                    files
                        .read(&snapshot.path)?
                        .ok_or_else(|| StackError::ConfigRead {
                            path: snapshot.path.clone(),
                            source: std::io::ErrorKind::NotFound.into(),
                        })?;
                let mut root = match serde_json::from_slice::<JsonValue>(&content) {
                    Ok(JsonValue::Object(root)) => root,
                    Ok(_) => return Err(native_error("agent.native_config_claude_state_invalid")),
                    Err(error) => {
                        return Err(json_parse_error(
                            "agent.native_config_claude_state_invalid",
                            &error,
                        ));
                    }
                };
                match value {
                    Some(value) => {
                        root.insert("hasCompletedOnboarding".to_owned(), JsonValue::Bool(*value));
                    }
                    None => {
                        root.remove("hasCompletedOnboarding");
                    }
                }
                files.write(&snapshot.path, &json_bytes(root)?)?;
            }
        }
    }
    Ok(())
}

pub fn write_native_config_files(
    prepared: &PreparedNativeConfigImport,
    files: NativeConfigFiles<'_>,
) -> Result<()> {
    files.write(files.config_path, prepared.canonical_toml.as_bytes())?;
    files.write(&prepared.native_path, &prepared.native_content)?;
    provision_agent_headless_config_in(&prepared.canonical_config, files.workload)?;
    Ok(())
}

pub fn capture_native_config_file_digests(
    paths: &[PathBuf],
    files: NativeConfigFiles<'_>,
) -> Result<Vec<NativeConfigFileDigest>> {
    paths
        .iter()
        .map(|path| {
            files.prepare(path)?;
            let sha256 = native_config_file_digest(path, files)?;
            Ok(NativeConfigFileDigest {
                path: path.clone(),
                sha256,
            })
        })
        .collect()
}

pub fn validate_native_config_file_digests(
    digests: &[NativeConfigFileDigest],
    files: NativeConfigFiles<'_>,
) -> Result<()> {
    if digests.is_empty() {
        return Err(native_error("agent.native_config_rollback_conflict"));
    }
    for expected in digests {
        files.prepare(&expected.path)?;
        let actual = native_config_file_digest(&expected.path, files)?;
        if actual != expected.sha256 {
            return Err(native_error("agent.native_config_rollback_conflict"));
        }
    }
    Ok(())
}

fn native_config_file_digest(path: &Path, files: NativeConfigFiles<'_>) -> Result<Option<String>> {
    let Some(content) = files.read(path)? else {
        return Ok(None);
    };
    if path != files.claude_state_path() {
        return Ok(Some(sha256_hex(&content)));
    }
    let root = match serde_json::from_slice::<JsonValue>(&content) {
        Ok(JsonValue::Object(root)) => root,
        Ok(_) => return Err(native_error("agent.native_config_claude_state_invalid")),
        Err(error) => {
            return Err(json_parse_error(
                "agent.native_config_claude_state_invalid",
                &error,
            ));
        }
    };
    let owned_value = match root.get("hasCompletedOnboarding") {
        Some(JsonValue::Bool(true)) => b"true".as_slice(),
        Some(JsonValue::Bool(false)) => b"false".as_slice(),
        None => b"missing".as_slice(),
        Some(_) => return Err(native_error("agent.native_config_claude_state_invalid")),
    };
    Ok(Some(sha256_hex(owned_value)))
}
