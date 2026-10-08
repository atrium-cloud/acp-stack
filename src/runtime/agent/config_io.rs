//! Workload-home file access plus the generic JSON / YAML / TOML read/write helpers for the
//! per-agent headless config provisioners; unrelated fields survive every write.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde_json::{Map, Value as JsonValue, json};
use serde_norway::{Mapping as YamlMapping, Value as YamlValue};
use toml::{Value as TomlValue, map::Map as TomlMap};

use crate::config::Config;
use crate::error::{Result, StackError};
use crate::runtime::sandbox::SandboxProfile;
use crate::workload_fs::{
    Anchor, DEFAULT_JOB_TIMEOUT, EntryInfo, EntryKind, Executor, LinkPolicy, OWNER_ONLY_DIR_MODE,
    OWNER_ONLY_FILE_MODE, UMASK_DIR_MODE, UMASK_FILE_MODE, WriteOptions,
};

// === CONSTANTS ===

/// Bound for one file read below the workload home.
pub const WORKLOAD_FILE_READ_LIMIT: u64 = 16 * 1024 * 1024;

/// The workload home and how files below it are accessed. Native agent config and skills live
/// there, so every read, write, and removal goes through the walker run by the sandbox profile's
/// executor. The runtime home stays the source of runtime-owned inputs such as the secret store
/// and the provider model catalog.
#[derive(Clone, Debug)]
pub struct WorkloadHome {
    runtime_home: PathBuf,
    home: PathBuf,
    executor: Executor,
    links: LinkPolicy,
    file_mode: u32,
    dir_mode: u32,
    expected_owner: u32,
    anchor: OnceLock<Arc<Anchor>>,
}

impl WorkloadHome {
    /// The workload home `config`'s `[workspace.sandbox]` resolves to.
    pub fn resolve(config: &Config, runtime_home: &Path) -> Result<Self> {
        let profile = SandboxProfile::resolve(&config.workspace.sandbox)?;
        Ok(Self::for_profile(&profile, runtime_home))
    }

    /// Workload-owned results follow the umask; without an identity they stay owner-only and
    /// owned by the runtime's effective uid.
    pub fn for_profile(profile: &SandboxProfile, runtime_home: &Path) -> Self {
        let (file_mode, dir_mode, expected_owner) = match &profile.identity {
            Some(identity) => (UMASK_FILE_MODE, UMASK_DIR_MODE, identity.uid),
            None => (
                OWNER_ONLY_FILE_MODE,
                OWNER_ONLY_DIR_MODE,
                crate::ownership::process_euid(),
            ),
        };
        Self {
            runtime_home: runtime_home.to_path_buf(),
            home: profile.workload_home(runtime_home).to_path_buf(),
            executor: profile.executor(),
            links: profile.link_policy(false),
            file_mode,
            dir_mode,
            expected_owner,
            anchor: OnceLock::new(),
        }
    }

    /// Separate runtime and workload homes accessed with the process's own credentials.
    #[cfg(test)]
    pub(crate) fn with_process_credentials(runtime_home: &Path, workload_home: &Path) -> Self {
        let mut workload = Self::for_profile(&SandboxProfile::default(), runtime_home);
        workload.home = workload_home.to_path_buf();
        workload
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    pub fn runtime_home(&self) -> &Path {
        &self.runtime_home
    }

    pub fn executor(&self) -> &Executor {
        &self.executor
    }

    pub fn file_mode(&self) -> u32 {
        self.file_mode
    }

    pub fn dir_mode(&self) -> u32 {
        self.dir_mode
    }

    /// Whether copies into the home may read hard-linked sources; see [`SandboxProfile::accepts_hard_links`].
    pub fn accepts_hard_links(&self) -> bool {
        matches!(self.links, LinkPolicy::Follow { .. })
    }

    /// Writes create missing parents and refuse to replace a file another uid owns.
    pub fn write_options(&self) -> WriteOptions {
        WriteOptions {
            create_parents: true,
            file_mode: self.file_mode,
            dir_mode: self.dir_mode,
            require_existing_owner: Some(self.expected_owner),
        }
    }

    /// `path` relative to the workload home, spelled with either the configured or the canonical
    /// home; a path outside it is refused.
    pub fn relative(&self, path: &Path) -> Result<PathBuf> {
        if let Ok(relative) = path.strip_prefix(&self.home) {
            return Ok(relative.to_path_buf());
        }
        path.strip_prefix(self.canonical_home()?)
            .map(Path::to_path_buf)
            .map_err(|_| StackError::WorkloadFsInvalidPath {
                path: path.to_path_buf(),
                reason: "is outside the workload home",
            })
    }

    /// Canonical path of the workload home, as the walker's anchor resolved it.
    pub fn canonical_home(&self) -> Result<PathBuf> {
        Ok(self.anchor()?.path().to_path_buf())
    }

    /// Run `job` on the executor against the workload home's anchor.
    pub fn run<T: Send + 'static>(
        &self,
        job: impl FnOnce(&Anchor, &WriteOptions) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        self.run_with_timeout(DEFAULT_JOB_TIMEOUT, job)
    }

    /// [`WorkloadHome::run`] bounded by `timeout`, for jobs larger than a config edit.
    pub fn run_with_timeout<T: Send + 'static>(
        &self,
        timeout: Duration,
        job: impl FnOnce(&Anchor, &WriteOptions) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let anchor = self.anchor()?;
        let options = self.write_options();
        self.executor.run(timeout, move || job(&anchor, &options))
    }

    /// Opened under the executor because the runtime may lack search permission on a workload
    /// identity's home.
    fn anchor(&self) -> Result<Arc<Anchor>> {
        if let Some(anchor) = self.anchor.get() {
            return Ok(Arc::clone(anchor));
        }
        let home = self.home.clone();
        let links = self.links;
        let opened = Arc::new(
            self.executor
                .run(DEFAULT_JOB_TIMEOUT, move || Anchor::open_with(&home, links))?,
        );
        Ok(Arc::clone(self.anchor.get_or_init(|| opened)))
    }

    /// Whether anything, a symlink included, sits at `path`.
    pub fn exists(&self, path: &Path) -> Result<bool> {
        Ok(self.stat(path)?.is_some())
    }

    /// `lstat` of `path`, or `None` when it or a parent is missing.
    pub fn stat(&self, path: &Path) -> Result<Option<EntryInfo>> {
        let relative = self.relative(path)?;
        self.run(move |anchor, _| crate::workload_fs::stat(anchor, &relative))
    }

    pub fn list_dir(&self, path: &Path) -> Result<Vec<(OsString, EntryInfo)>> {
        let relative = self.relative(path)?;
        self.run(move |anchor, _| crate::workload_fs::list_dir(anchor, &relative))
    }

    /// [`WorkloadHome::list_dir`] with a missing directory listed as empty.
    pub fn list_dir_or_empty(&self, path: &Path) -> Result<Vec<(OsString, EntryInfo)>> {
        match self.list_dir(path) {
            Err(StackError::WorkloadFsNotFound { .. }) => Ok(Vec::new()),
            listed => listed,
        }
    }

    /// Content of the regular file at `path`, or `None` when it or a parent is missing.
    pub fn read(&self, path: &Path) -> Result<Option<Vec<u8>>> {
        let relative = self.relative(path)?;
        self.run(move |anchor, _| {
            match crate::workload_fs::read_file(anchor, &relative, WORKLOAD_FILE_READ_LIMIT) {
                Ok(content) => Ok(Some(content)),
                Err(StackError::WorkloadFsNotFound { .. }) => Ok(None),
                Err(error) => Err(error),
            }
        })
    }

    pub fn write_atomic(&self, path: &Path, content: Vec<u8>) -> Result<()> {
        let relative = self.relative(path)?;
        self.run(move |anchor, options| {
            crate::workload_fs::write_file_atomic(anchor, &relative, &content, options)
        })
    }

    pub fn remove_file(&self, path: &Path) -> Result<()> {
        let relative = self.relative(path)?;
        self.run(move |anchor, _| crate::workload_fs::remove_file(anchor, &relative))
    }

    /// Whether a file was there to remove.
    pub fn remove_file_if_present(&self, path: &Path) -> Result<bool> {
        match self.remove_file(path) {
            Ok(()) => Ok(true),
            Err(StackError::WorkloadFsNotFound { .. }) => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Remove `path` and, for a directory, everything below it; symlinks are unlinked as links.
    pub fn remove_tree(&self, path: &Path) -> Result<()> {
        let relative = self.relative(path)?;
        self.run(move |anchor, _| crate::workload_fs::remove_tree(anchor, &relative))
    }

    pub fn remove_empty_dir(&self, path: &Path) -> Result<()> {
        let relative = self.relative(path)?;
        self.run(move |anchor, _| crate::workload_fs::remove_empty_dir(anchor, &relative))
    }

    pub fn create_dir_all(&self, path: &Path) -> Result<()> {
        let relative = self.relative(path)?;
        self.run(move |anchor, options| {
            crate::workload_fs::create_dir_all(anchor, &relative, options.dir_mode)
        })
    }

    pub fn symlink(&self, path: &Path, target: &Path) -> Result<()> {
        let relative = self.relative(path)?;
        let target = target.to_path_buf();
        self.run(move |anchor, _| crate::workload_fs::symlink(anchor, &relative, &target))
    }

    pub fn read_link(&self, path: &Path) -> Result<PathBuf> {
        let relative = self.relative(path)?;
        self.run(move |anchor, _| crate::workload_fs::read_link(anchor, &relative))
    }

    /// Validate a file target below the workload home: every existing parent must be a real
    /// directory owned by the expected uid, missing parents are created, and an existing target
    /// must be a single-link regular file owned by the expected uid.
    pub fn prepare_owned_file(&self, path: &Path) -> Result<()> {
        let relative = self.relative(path)?;
        let home = self.home.clone();
        let expected_owner = self.expected_owner;
        self.run(move |anchor, options| {
            let mut parent = PathBuf::new();
            for component in relative.parent().into_iter().flat_map(Path::components) {
                let Component::Normal(name) = component else {
                    return Err(StackError::WorkloadFsInvalidPath {
                        path: home.join(&relative),
                        reason: "contains a non-normal path component",
                    });
                };
                parent.push(name);
                let display = home.join(&parent);
                match crate::workload_fs::stat(anchor, &parent)? {
                    None => crate::workload_fs::create_dir_all(anchor, &parent, options.dir_mode)?,
                    Some(info) if info.kind == EntryKind::Dir => {
                        require_owner(&info, expected_owner, display)?;
                    }
                    Some(info) if info.kind == EntryKind::Symlink => {
                        return Err(StackError::WorkloadFsSymlinkRefused { path: display });
                    }
                    Some(_) => {
                        return Err(StackError::WorkloadFsInvalidPath {
                            path: display,
                            reason: "a path component is not a directory",
                        });
                    }
                }
            }
            let display = home.join(&relative);
            match crate::workload_fs::stat(anchor, &relative)? {
                None => Ok(()),
                Some(info) => match info.kind {
                    EntryKind::File if info.nlink > 1 => {
                        Err(StackError::WorkloadFsHardLinkRefused { path: display })
                    }
                    EntryKind::File => require_owner(&info, expected_owner, display),
                    EntryKind::Symlink => {
                        Err(StackError::WorkloadFsSymlinkRefused { path: display })
                    }
                    EntryKind::Dir | EntryKind::Other => {
                        Err(StackError::WorkloadFsNotRegular { path: display })
                    }
                },
            }
        })
    }
}

fn require_owner(info: &EntryInfo, expected_uid: u32, path: PathBuf) -> Result<()> {
    if info.uid == expected_uid {
        return Ok(());
    }
    Err(StackError::WorkloadFsOwnerMismatch {
        path,
        expected_uid,
        actual_uid: info.uid,
    })
}

/// UTF-8 content of the file at `path`, or `None` when it is missing.
fn read_text(workload: &WorkloadHome, path: &Path) -> Result<Option<String>> {
    let Some(content) = workload.read(path)? else {
        return Ok(None);
    };
    String::from_utf8(content)
        .map(Some)
        .map_err(|source| StackError::ConfigRead {
            path: path.to_path_buf(),
            source: std::io::Error::new(std::io::ErrorKind::InvalidData, source),
        })
}

pub(super) fn read_json_object(
    workload: &WorkloadHome,
    path: &Path,
) -> Result<Map<String, JsonValue>> {
    let Some(content) = read_text(workload, path)? else {
        return Ok(Map::new());
    };
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

pub(super) fn write_json_object(
    workload: &WorkloadHome,
    path: &Path,
    object: Map<String, JsonValue>,
) -> Result<()> {
    let content = serde_json::to_vec_pretty(&JsonValue::Object(object)).map_err(|source| {
        StackError::AgentConfigProvision {
            path: path.to_path_buf(),
            reason: format!("failed to serialize JSON: {source}"),
        }
    })?;
    let mut with_newline = content;
    with_newline.push(b'\n');
    workload.write_atomic(path, with_newline)
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

pub(super) fn read_yaml_mapping(workload: &WorkloadHome, path: &Path) -> Result<YamlMapping> {
    let Some(content) = read_text(workload, path)? else {
        return Ok(YamlMapping::new());
    };
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

pub(super) fn write_yaml_mapping(
    workload: &WorkloadHome,
    path: &Path,
    mapping: YamlMapping,
) -> Result<()> {
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
    workload.write_atomic(path, bytes)
}

pub(super) fn read_toml_table(
    workload: &WorkloadHome,
    path: &Path,
) -> Result<TomlMap<String, TomlValue>> {
    let Some(content) = read_text(workload, path)? else {
        return Ok(TomlMap::new());
    };
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

pub(super) fn write_toml_table(
    workload: &WorkloadHome,
    path: &Path,
    table: TomlMap<String, TomlValue>,
) -> Result<()> {
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
    workload.write_atomic(path, bytes)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    fn relative_accepts_either_home_spelling_and_refuses_outside_paths() {
        let home = tempfile::tempdir().expect("home");
        let workload = WorkloadHome::with_process_credentials(home.path(), home.path());
        let canonical = home.path().canonicalize().expect("canonical home");

        assert_eq!(
            workload
                .relative(&home.path().join(".codex/config.toml"))
                .expect("configured spelling"),
            Path::new(".codex/config.toml")
        );
        assert_eq!(
            workload
                .relative(&canonical.join(".codex/config.toml"))
                .expect("canonical spelling"),
            Path::new(".codex/config.toml")
        );
        let error = workload
            .relative(Path::new("/etc/passwd"))
            .expect_err("outside path");
        assert!(
            matches!(error, StackError::WorkloadFsInvalidPath { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn writes_create_owner_only_parents_and_files_without_an_identity() {
        let home = tempfile::tempdir().expect("home");
        let workload = WorkloadHome::with_process_credentials(home.path(), home.path());
        let path = home.path().join(".config/agent/settings.json");

        workload
            .write_atomic(&path, b"{}\n".to_vec())
            .expect("write");

        let mode = |path: &Path| {
            std::fs::symlink_metadata(path)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode(&path), OWNER_ONLY_FILE_MODE);
        assert_eq!(
            mode(&home.path().join(".config/agent")),
            OWNER_ONLY_DIR_MODE
        );
        assert_eq!(workload.read(&path).expect("read"), Some(b"{}\n".to_vec()));
        assert_eq!(
            workload
                .read(&home.path().join("missing/file"))
                .expect("read"),
            None
        );
    }

    #[test]
    fn prepare_owned_file_creates_missing_parents_and_refuses_links() {
        let home = tempfile::tempdir().expect("home");
        let outside = tempfile::tempdir().expect("outside");
        let workload = WorkloadHome::with_process_credentials(home.path(), home.path());

        workload
            .prepare_owned_file(&home.path().join(".config/opencode/opencode.json"))
            .expect("missing parents created");
        assert!(home.path().join(".config/opencode").is_dir());

        std::os::unix::fs::symlink(outside.path(), home.path().join(".codex")).expect("link");
        let error = workload
            .prepare_owned_file(&home.path().join(".codex/config.toml"))
            .expect_err("symlinked parent");
        assert!(
            matches!(error, StackError::WorkloadFsSymlinkRefused { .. }),
            "{error:?}"
        );

        let outside_file = outside.path().join("settings.json");
        std::fs::write(&outside_file, b"{}").expect("outside file");
        std::fs::create_dir(home.path().join(".claude")).expect("claude dir");
        let linked = home.path().join(".claude/settings.json");
        std::fs::hard_link(&outside_file, &linked).expect("hard link");
        let error = workload
            .prepare_owned_file(&linked)
            .expect_err("hard-linked target");
        assert!(
            matches!(error, StackError::WorkloadFsHardLinkRefused { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn ownership_checks_compare_against_the_expected_owner() {
        let home = tempfile::tempdir().expect("home");
        let path = home.path().join(".claude.json");
        std::fs::write(&path, b"{}").expect("file");
        let mut workload = WorkloadHome::with_process_credentials(home.path(), home.path());
        workload.expected_owner = crate::ownership::process_euid().wrapping_add(1);

        let prepared = workload.prepare_owned_file(&path);
        let written = workload.write_atomic(&path, b"{\"changed\":true}".to_vec());

        assert!(
            matches!(prepared, Err(StackError::WorkloadFsOwnerMismatch { .. })),
            "{prepared:?}"
        );
        assert!(
            matches!(written, Err(StackError::WorkloadFsOwnerMismatch { .. })),
            "{written:?}"
        );
        assert_eq!(std::fs::read(&path).expect("unchanged"), b"{}");
    }
}
