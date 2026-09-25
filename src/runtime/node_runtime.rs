//! Runtime-managed Node.js: one major line installed under `~/.local/lib/acp-stack/node`, outside
//! every sandbox-masked path, and resolved ahead of any host Node by agents, installers, and deps
//! shells. Any release on the major line satisfies it; patches are not chased.

use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use crate::dev_gates::{NODE_DIST_BASE_ENV, TEST_SKIP_NODE_RUNTIME_ENV};
use crate::error::{Result, StackError};
use crate::fs_util::{
    ExclusiveFileLock, acquire_exclusive_lock_file, replace_symlink_atomically,
    try_acquire_exclusive_lock_file,
};
use crate::runtime::process_runner::{CaptureOutcome, kill_process_group, run_captured};
use crate::runtime::workspace_sources::safe_download::{DownloadOpts, download_to_file};

// CONSTANTS

/// The Node.js major line the runtime manages; bump it when a newer line becomes current.
pub const NODE_MAJOR: u32 = 26;
const DEFAULT_DIST_BASE: &str = "https://nodejs.org/dist";
const SUPPORTED_OS: &str = "linux";
/// `std::env::consts::ARCH` to the arch token in Node.js release archive names.
const ARCH_MAP: &[(&str, &str)] = &[("x86_64", "x64"), ("aarch64", "arm64")];
const MANAGED_TOOLS: &[&str] = &["node", "npm", "npx"];
const LOCK_FILE_NAME: &str = ".lock";
const FAILED_INSTALL_MARKER_NAME: &str = ".install-failed";
/// Any failed install (unreachable dist, checksum mismatch, or a build that will not run) is not
/// retried until this long after it, so such a host is not re-downloaded by every installer, deps
/// apply, init retry, or serve restart.
const FAILED_INSTALL_COOLDOWN: Duration = Duration::from_secs(10 * 60);
const FAILED_INSTALL_REASON_UNREADABLE: &str = "the recorded failure reason could not be read";
const CURRENT_LINK_NAME: &str = "current";
const RELEASES_DIR_NAME: &str = "releases";
const STAGING_PREFIX: &str = ".staging-";
const UNPACK_DIR_NAME: &str = "unpacked";
const CHECKSUMS_FILE_NAME: &str = "SHASUMS256.txt";
const ARCHIVE_SUFFIX: &str = ".tar.gz";
/// npm's builtin config file, read before user and global config; `prefix` set here moves
/// `npm -g` out of the versioned release directory.
const NPM_BUILTIN_CONFIG_PATH: &[&str] = &["lib", "node_modules", "npm", "npmrc"];
const CHECKSUMS_MAX_BYTES: u64 = 1024 * 1024;
const ARCHIVE_MAX_BYTES: u64 = 256 * 1024 * 1024;
const SHA256_HEX_LEN: usize = 64;
const FETCH_ATTEMPTS: u32 = 3;
const FETCH_BACKOFF_BASE: Duration = Duration::from_secs(2);
const FETCH_BACKOFF_MAX: Duration = Duration::from_secs(10);
const FETCH_BACKOFF_MAX_EXPONENT: u32 = 3;
const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(15);
const VERSION_PROBE_STREAM_CAP: usize = 4 * 1024;
const FIXTURE_SKIP_REASON: &str = "skipped by the test fixture gate";

/// What an ensure or readiness check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeRuntimeStatus {
    /// A managed release on [`NODE_MAJOR`] is installed; `version` is its `vX.Y.Z`.
    Ready { version: String },
    /// This host gets no managed Node; whatever Node is on PATH applies.
    Unsupported { reason: String },
    /// No managed release is installed yet. Only [`wait_ready`] reports this.
    NotReady,
}

/// The platform the managed runtime targets, injectable so tests exercise other hosts' paths.
#[derive(Debug, Clone, Copy)]
pub struct NodePlatform<'a> {
    pub os: &'a str,
    pub arch: &'a str,
}

impl NodePlatform<'static> {
    pub fn host() -> Self {
        Self {
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
        }
    }
}

impl NodePlatform<'_> {
    fn node_arch(&self) -> Option<&'static str> {
        if self.os != SUPPORTED_OS {
            return None;
        }
        ARCH_MAP
            .iter()
            .find(|(arch, _)| *arch == self.arch)
            .map(|(_, node_arch)| *node_arch)
    }

    fn unsupported(&self) -> NodeRuntimeStatus {
        NodeRuntimeStatus::Unsupported {
            reason: format!(
                "no managed Node.js {NODE_MAJOR} build for {}/{}",
                self.os, self.arch
            ),
        }
    }
}

/// Root of the managed runtime. It must stay outside `sandbox::sensitive_mask_paths`, or sandboxed
/// spawns would see a masked, empty directory.
pub fn managed_root(home: &Path) -> PathBuf {
    home.join(".local")
        .join("lib")
        .join("acp-stack")
        .join("node")
}

/// The managed `bin` directory, stable across release swaps because it goes through `current`.
pub fn managed_bin_dir(home: &Path) -> PathBuf {
    managed_root(home).join(CURRENT_LINK_NAME).join("bin")
}

fn npm_global_prefix(home: &Path) -> PathBuf {
    home.join(".local")
}

fn dist_base() -> String {
    crate::dev_gates::fixture_string(NODE_DIST_BASE_ENV)
        .map(|base| base.trim_end_matches('/').to_owned())
        .unwrap_or_else(|| DEFAULT_DIST_BASE.to_owned())
}

fn fixture_skip() -> bool {
    crate::dev_gates::fixture_enabled(TEST_SKIP_NODE_RUNTIME_ENV)
}

/// Install the managed Node when it is missing or not on [`NODE_MAJOR`], then link its tools into
/// `~/.local/bin`. Serialized across processes by the root's lock, so a caller arriving during
/// another process's install waits for it and then takes the offline fast path.
pub fn ensure(home: &Path) -> Result<NodeRuntimeStatus> {
    ensure_holding(home, None)
}

/// [`ensure`] for installer and deps entry points, which proceed without managed Node: a recipe
/// that needs it then fails its own `required_tools` check with a typed prerequisites error.
pub fn ensure_before_install(home: &Path) {
    if let Err(error) = ensure(home) {
        tracing::warn!(%error, "managed Node.js is unavailable; continuing with the host Node.js");
    }
}

/// [`ensure`] under a lock the caller already took with [`try_lock_for_startup`].
pub fn ensure_holding(home: &Path, held: Option<ExclusiveFileLock>) -> Result<NodeRuntimeStatus> {
    if fixture_skip() {
        return Ok(NodeRuntimeStatus::Unsupported {
            reason: FIXTURE_SKIP_REASON.to_owned(),
        });
    }
    ensure_for_platform(home, NodePlatform::host(), &dist_base(), held)
}

/// Whether this process manages Node at all: a supported platform outside the fixture skip gate.
pub fn is_managed_host() -> bool {
    !fixture_skip() && NodePlatform::host().node_arch().is_some()
}

/// Take the root's lock without waiting, so `acps serve` holds it before its listener binds and no
/// request can spawn an agent ahead of the startup install. `None` when this host gets no managed
/// Node or another process is already installing.
pub fn try_lock_for_startup(home: &Path) -> Result<Option<ExclusiveFileLock>> {
    if !is_managed_host() {
        return Ok(None);
    }
    let root = managed_root(home);
    create_dir(&root)?;
    try_acquire_exclusive_lock_file(&root.join(LOCK_FILE_NAME))
}

fn ensure_for_platform(
    home: &Path,
    platform: NodePlatform<'_>,
    dist_base: &str,
    held: Option<ExclusiveFileLock>,
) -> Result<NodeRuntimeStatus> {
    let Some(node_arch) = platform.node_arch() else {
        let status = platform.unsupported();
        tracing::warn!(
            ?status,
            "managed Node.js is unavailable; using the host Node.js"
        );
        return Ok(status);
    };
    let root = managed_root(home);
    let _lock = match held {
        Some(lock) => lock,
        None => {
            create_dir(&root)?;
            acquire_exclusive_lock_file(&root.join(LOCK_FILE_NAME))?
        }
    };
    let version = match installed_version(home) {
        Some(version) => version,
        None => {
            if let Some(reason) = recent_install_failure(&root) {
                return Err(install_failed(format!(
                    "an install attempt failed less than {}s ago, so this one is skipped: {reason}",
                    FAILED_INSTALL_COOLDOWN.as_secs()
                )));
            }
            match install_latest(home, &root, node_arch, dist_base) {
                Ok(version) => {
                    clear_install_failure(&root);
                    version
                }
                Err(error) => {
                    record_install_failure(&root, &error);
                    return Err(error);
                }
            }
        }
    };
    link_tools(home);
    Ok(NodeRuntimeStatus::Ready { version })
}

/// The recorded reason of a failed install still inside [`FAILED_INSTALL_COOLDOWN`]. A marker whose
/// age cannot be read (clock skew, unreadable metadata) does not hold back a retry.
fn recent_install_failure(root: &Path) -> Option<String> {
    let marker = root.join(FAILED_INSTALL_MARKER_NAME);
    let age = match std::fs::metadata(&marker).and_then(|metadata| {
        metadata
            .modified()?
            .elapsed()
            .map_err(std::io::Error::other)
    }) {
        Ok(age) => age,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            tracing::warn!(%error, path = %marker.display(), "could not read the failed managed Node.js install marker's age; retrying the install");
            return None;
        }
    };
    if age >= FAILED_INSTALL_COOLDOWN {
        return None;
    }
    match std::fs::read_to_string(&marker) {
        Ok(reason) => Some(reason),
        Err(error) => {
            tracing::warn!(%error, path = %marker.display(), "could not read the failed managed Node.js install reason");
            Some(FAILED_INSTALL_REASON_UNREADABLE.to_owned())
        }
    }
}

fn record_install_failure(root: &Path, error: &StackError) {
    let marker = root.join(FAILED_INSTALL_MARKER_NAME);
    if let Err(source) = std::fs::write(&marker, error.to_string()) {
        tracing::warn!(error = %source, path = %marker.display(), "could not record the failed managed Node.js install");
    }
}

fn clear_install_failure(root: &Path) {
    let marker = root.join(FAILED_INSTALL_MARKER_NAME);
    match std::fs::remove_file(&marker) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            tracing::warn!(%error, path = %marker.display(), "could not clear the failed managed Node.js install marker");
        }
    }
}

/// Wait out any in-flight install, then report whether a managed release is present. Never
/// downloads, and never creates the root, so agent spawns in unmanaged processes touch nothing.
pub fn wait_ready(home: &Path) -> NodeRuntimeStatus {
    if fixture_skip() {
        return NodeRuntimeStatus::Unsupported {
            reason: FIXTURE_SKIP_REASON.to_owned(),
        };
    }
    wait_ready_for_platform(home, NodePlatform::host())
}

fn wait_ready_for_platform(home: &Path, platform: NodePlatform<'_>) -> NodeRuntimeStatus {
    if platform.node_arch().is_none() {
        return platform.unsupported();
    }
    let lock_path = managed_root(home).join(LOCK_FILE_NAME);
    if !lock_path.is_file() {
        return NodeRuntimeStatus::NotReady;
    }
    match acquire_exclusive_lock_file(&lock_path) {
        Ok(lock) => drop(lock),
        Err(error) => {
            tracing::warn!(%error, "could not wait on the managed Node.js lock; checking the install as-is");
        }
    }
    match installed_version(home) {
        Some(version) => NodeRuntimeStatus::Ready { version },
        None => NodeRuntimeStatus::NotReady,
    }
}

/// The installed managed release's `vX.Y.Z` when `current` names a [`NODE_MAJOR`] release that
/// has a `bin/node`. Stat-only: no lock and no process spawn.
pub fn installed_version(home: &Path) -> Option<String> {
    let root = managed_root(home);
    let target = std::fs::read_link(root.join(CURRENT_LINK_NAME)).ok()?;
    let release_name = target.file_name()?.to_str()?;
    let version = release_version(release_name)?;
    if !major_matches(version) {
        return None;
    }
    root.join(CURRENT_LINK_NAME)
        .join("bin")
        .join("node")
        .is_file()
        .then(|| version.to_owned())
}

/// `node-v26.10.0-linux-x64` to `v26.10.0`.
fn release_version(release_name: &str) -> Option<&str> {
    let rest = release_name.strip_prefix("node-")?;
    let (version, _platform) = rest.split_once('-')?;
    Some(version)
}

fn major_matches(version: &str) -> bool {
    version
        .strip_prefix('v')
        .and_then(|rest| rest.split('.').next())
        .and_then(|major| major.parse::<u32>().ok())
        == Some(NODE_MAJOR)
}

fn install_latest(home: &Path, root: &Path, node_arch: &str, dist_base: &str) -> Result<String> {
    let releases = root.join(RELEASES_DIR_NAME);
    create_dir(&releases)?;
    let staging = tempfile::Builder::new()
        .prefix(STAGING_PREFIX)
        .tempdir_in(&releases)
        .map_err(|source| install_failed(format!("create staging dir: {source}")))?;
    let channel = format!("{dist_base}/latest-v{NODE_MAJOR}.x");
    let schemes = allowed_schemes(dist_base);

    let checksums_path = staging.path().join(CHECKSUMS_FILE_NAME);
    fetch_with_retry(
        &format!("{channel}/{CHECKSUMS_FILE_NAME}"),
        &checksums_path,
        &DownloadOpts {
            allowed_schemes: schemes.clone(),
            max_bytes: CHECKSUMS_MAX_BYTES,
            ..DownloadOpts::default()
        },
    )?;
    let checksums = std::fs::read_to_string(&checksums_path)
        .map_err(|source| install_failed(format!("read {CHECKSUMS_FILE_NAME}: {source}")))?;
    let (archive_name, expected_sha256) =
        select_archive(&checksums, node_arch).ok_or_else(|| {
            install_failed(format!(
                "{CHECKSUMS_FILE_NAME} lists no Node.js {NODE_MAJOR} archive for linux-{node_arch}"
            ))
        })?;

    let archive_path = staging.path().join(&archive_name);
    fetch_with_retry(
        &format!("{channel}/{archive_name}"),
        &archive_path,
        &DownloadOpts {
            allowed_schemes: schemes,
            max_bytes: ARCHIVE_MAX_BYTES,
            expected_sha256: Some(expected_sha256),
            ..DownloadOpts::default()
        },
    )
    .map_err(|error| match error {
        StackError::SafeDownloadChecksumMismatch { expected, actual } => {
            StackError::NodeRuntimeChecksumMismatch {
                archive: archive_name.clone(),
                expected,
                actual,
            }
        }
        other => other,
    })?;

    let release_name = archive_name
        .strip_suffix(ARCHIVE_SUFFIX)
        .unwrap_or(&archive_name)
        .to_owned();
    let unpack_dir = staging.path().join(UNPACK_DIR_NAME);
    create_dir(&unpack_dir)?;
    unpack(&archive_path, &unpack_dir)?;
    let unpacked_release = unpack_dir.join(&release_name);
    if !unpacked_release.is_dir() {
        return Err(install_failed(format!(
            "{archive_name} did not contain `{release_name}/`"
        )));
    }
    write_npm_builtin_config(&unpacked_release, home)?;
    let version = probe_version(&unpacked_release.join("bin").join("node"))?;

    let release_dir = releases.join(&release_name);
    if std::fs::symlink_metadata(&release_dir).is_ok() {
        std::fs::remove_dir_all(&release_dir).map_err(|source| {
            install_failed(format!(
                "replace stale release {}: {source}",
                release_dir.display()
            ))
        })?;
    }
    std::fs::rename(&unpacked_release, &release_dir).map_err(|source| {
        install_failed(format!(
            "move release into {}: {source}",
            release_dir.display()
        ))
    })?;
    let current = root.join(CURRENT_LINK_NAME);
    let previous_release = std::fs::read_link(&current)
        .ok()
        .and_then(|target| target.file_name().map(|name| name.to_owned()));
    replace_symlink_atomically(&Path::new(RELEASES_DIR_NAME).join(&release_name), &current)?;
    drop(staging);
    prune_releases(&releases, &release_name, previous_release.as_deref());
    tracing::info!(%version, "installed managed Node.js");
    Ok(version)
}

fn allowed_schemes(dist_base: &str) -> Vec<String> {
    // Plain http is reachable only through the fixture dist-base override, which serves loopback.
    if dist_base.starts_with("http://") {
        vec!["http".to_owned(), "https".to_owned()]
    } else {
        vec!["https".to_owned()]
    }
}

fn fetch_with_retry(url: &str, dest: &Path, opts: &DownloadOpts) -> Result<()> {
    let mut attempt = 1;
    loop {
        match download_to_file(url, dest, opts) {
            Ok(_) => return Ok(()),
            Err(error) if attempt < FETCH_ATTEMPTS && is_transient(&error) => {
                let delay = crate::time_util::exponential_backoff_delay(
                    attempt,
                    FETCH_BACKOFF_BASE,
                    FETCH_BACKOFF_MAX,
                    FETCH_BACKOFF_MAX_EXPONENT,
                );
                tracing::warn!(%error, attempt, "managed Node.js download failed; retrying");
                std::thread::sleep(delay);
                attempt += 1;
            }
            Err(error @ StackError::SafeDownloadChecksumMismatch { .. }) => return Err(error),
            Err(error) => return Err(install_failed(format!("download {url}: {error}"))),
        }
    }
}

fn is_transient(error: &StackError) -> bool {
    match error {
        StackError::SafeDownloadFailed { .. } => true,
        StackError::SafeDownloadHttpStatus { status, .. } => *status >= 500 || *status == 429,
        _ => false,
    }
}

/// The archive name and sha256 for `node_arch` on [`NODE_MAJOR`] from a `SHASUMS256.txt` body.
/// Names are matched exactly, so no listed name can smuggle a path separator into the staging dir.
fn select_archive(checksums: &str, node_arch: &str) -> Option<(String, String)> {
    let suffix = format!("-linux-{node_arch}{ARCHIVE_SUFFIX}");
    checksums.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let sha256 = fields.next()?;
        let name = fields.next()?.trim_start_matches('*');
        let version = name.strip_prefix("node-")?.strip_suffix(&suffix)?;
        let digits_and_dots = version
            .strip_prefix('v')?
            .chars()
            .all(|character| character.is_ascii_digit() || character == '.');
        let valid_sha = sha256.len() == SHA256_HEX_LEN
            && sha256
                .chars()
                .all(|character| character.is_ascii_hexdigit());
        (digits_and_dots && major_matches(version) && valid_sha)
            .then(|| (name.to_owned(), sha256.to_ascii_lowercase()))
    })
}

fn unpack(archive: &Path, dest: &Path) -> Result<()> {
    let file = std::fs::File::open(archive)
        .map_err(|source| install_failed(format!("open {}: {source}", archive.display())))?;
    tar::Archive::new(flate2::read::GzDecoder::new(file))
        .unpack(dest)
        .map_err(|source| install_failed(format!("unpack {}: {source}", archive.display())))
}

fn write_npm_builtin_config(release: &Path, home: &Path) -> Result<()> {
    let path = NPM_BUILTIN_CONFIG_PATH
        .iter()
        .fold(release.to_path_buf(), |path, part| path.join(part));
    let Some(parent) = path.parent() else {
        return Err(install_failed(format!(
            "npm config path {} has no parent",
            path.display()
        )));
    };
    if !parent.is_dir() {
        return Err(install_failed(format!(
            "release has no bundled npm at {}",
            parent.display()
        )));
    }
    let content = format!("prefix={}\n", npm_global_prefix(home).display());
    std::fs::write(&path, content)
        .map_err(|source| install_failed(format!("write {}: {source}", path.display())))
}

/// Run the unpacked `node --version` so a wrong-architecture or libc-incompatible build fails
/// before it becomes `current`.
fn probe_version(node: &Path) -> Result<String> {
    let mut command = std::process::Command::new(node);
    command.arg("--version").env_clear();
    let outcome = run_captured(
        &mut command,
        VERSION_PROBE_TIMEOUT,
        VERSION_PROBE_STREAM_CAP,
    )
    .map_err(|source| install_failed(format!("run {} --version: {source}", node.display())))?;
    let stdout = match outcome {
        CaptureOutcome::Exited { status, stdout, .. } if status.success() => stdout,
        CaptureOutcome::Exited {
            status,
            stderr_tail,
            ..
        } => {
            return Err(install_failed(format!(
                "{} --version exited with {status}: {}",
                node.display(),
                stderr_tail.trim()
            )));
        }
        CaptureOutcome::TimedOut { mut child, .. }
        | CaptureOutcome::WaitFailed { mut child, .. } => {
            kill_process_group(&mut child);
            if let Err(error) = child.wait() {
                tracing::debug!(%error, "managed Node.js version probe reap failed");
            }
            return Err(install_failed(format!(
                "{} --version did not finish",
                node.display()
            )));
        }
    };
    let version = stdout.trim().to_owned();
    if !major_matches(&version) {
        return Err(install_failed(format!(
            "unpacked Node.js reports `{version}`, expected v{NODE_MAJOR}.x"
        )));
    }
    Ok(version)
}

/// Remove every release except the new one and the one it replaced, which processes started
/// before the swap may still be running from. Failures are logged; stale releases are harmless.
fn prune_releases(releases: &Path, keep: &str, previous: Option<&std::ffi::OsStr>) {
    let entries = match std::fs::read_dir(releases) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::warn!(%error, "could not list managed Node.js releases for pruning");
            return;
        }
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name == keep || Some(name.as_os_str()) == previous {
            continue;
        }
        let path = entry.path();
        if let Err(error) = std::fs::remove_dir_all(&path) {
            tracing::warn!(%error, path = %path.display(), "could not prune a managed Node.js release");
        }
    }
}

/// Point `~/.local/bin/{node,npm,npx}` at the managed tools. Symlinks, including stale ones into
/// the legacy masked root, are replaced; a regular file is the operator's and is left alone.
/// Failures are logged: the managed `bin` precedes `~/.local/bin` on every runtime PATH anyway.
fn link_tools(home: &Path) {
    let local_bin = crate::runtime::install::local_bin_dir(home);
    if let Err(source) = std::fs::create_dir_all(&local_bin) {
        tracing::warn!(error = %source, "could not create ~/.local/bin for managed Node.js links");
        return;
    }
    let bin_dir = managed_bin_dir(home);
    for tool in MANAGED_TOOLS {
        let target = bin_dir.join(tool);
        if !target.exists() {
            continue;
        }
        let link = local_bin.join(tool);
        match std::fs::symlink_metadata(&link) {
            Ok(metadata) if !metadata.file_type().is_symlink() => {
                tracing::warn!(path = %link.display(), "leaving an existing non-symlink in place of the managed Node.js link");
                continue;
            }
            Ok(_) if std::fs::read_link(&link).is_ok_and(|existing| existing == target) => {
                continue;
            }
            _ => {}
        }
        if let Err(error) = replace_symlink_atomically(&target, &link) {
            tracing::warn!(%error, tool, "could not link a managed Node.js tool into ~/.local/bin");
        }
    }
}

fn create_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path).map_err(|source| StackError::DirectoryCreate {
        path: path.to_path_buf(),
        source,
    })
}

fn install_failed(reason: String) -> StackError {
    StackError::NodeRuntimeInstallFailed { reason }
}

/// The daemon's view of its startup ensure, read by the health report.
#[derive(Debug, Clone)]
pub enum NodeRuntimeOutcome {
    /// This process does not manage Node (CLI and in-process harnesses).
    Unmanaged,
    Pending,
    /// The startup ensure finished; an error keeps only its public message.
    Settled(std::result::Result<NodeRuntimeStatus, String>),
}

/// Shared, cheaply clonable handle to [`NodeRuntimeOutcome`].
#[derive(Debug, Clone)]
pub struct NodeRuntimeState {
    outcome: Arc<RwLock<NodeRuntimeOutcome>>,
}

impl Default for NodeRuntimeState {
    fn default() -> Self {
        Self {
            outcome: Arc::new(RwLock::new(NodeRuntimeOutcome::Unmanaged)),
        }
    }
}

impl NodeRuntimeState {
    pub fn set(&self, outcome: NodeRuntimeOutcome) {
        *self.outcome.write().unwrap_or_else(PoisonError::into_inner) = outcome;
    }

    pub fn get(&self) -> NodeRuntimeOutcome {
        self.outcome
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[cfg(test)]
mod tests;
