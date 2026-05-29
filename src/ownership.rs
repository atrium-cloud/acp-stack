//! Runtime ownership and permission inspection helpers.
//!
//! `acps security check` uses these to surface paths owned by the wrong uid,
//! paths with looser-than-expected permissions, and a workspace root that
//! isn't writable by the daemon process. Future deployment automation
//! (Docker, systemd installer) is expected to reuse `resolve_runtime_user_uid`
//! when validating an install was set up correctly.

use std::ffi::CString;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};

/// Each `PathKind` carries its own expectation for the mode bits we surface.
/// Directory paths owned by the runtime user are kept at 0o700; sensitive
/// files at 0o600. The workspace root is created by the operator (or by an
/// installer) and we deliberately don't pin its mode — only that the daemon
/// can write into it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathKind {
    ConfigDir,
    ConfigFile,
    StateDir,
    StateDb,
    AgeKey,
    SecretStore,
    WorkspaceRoot,
}

impl PathKind {
    /// Mode that `runtime.path_mode_loose` checks against. `WorkspaceRoot`
    /// returns `None` because the workspace is operator-provisioned and may
    /// legitimately be group-readable or world-traversable on shared hosts.
    pub fn expected_mode(self) -> Option<u32> {
        match self {
            PathKind::ConfigDir | PathKind::StateDir => Some(0o700),
            PathKind::ConfigFile | PathKind::StateDb | PathKind::AgeKey | PathKind::SecretStore => {
                Some(0o600)
            }
            PathKind::WorkspaceRoot => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            PathKind::ConfigDir => "config directory",
            PathKind::ConfigFile => "config file",
            PathKind::StateDir => "state directory",
            PathKind::StateDb => "state database",
            PathKind::AgeKey => "age key",
            PathKind::SecretStore => "encrypted secret store",
            PathKind::WorkspaceRoot => "workspace root",
        }
    }
}

#[derive(Debug, Clone)]
pub struct PathPosture {
    pub path: PathBuf,
    pub kind: PathKind,
    pub uid: u32,
    pub mode: u32,
    /// `true` when the inspected path is itself a symlink. The runtime never
    /// creates symlinks at managed paths (`fs_util::create_dir_owner_only`
    /// refuses to follow them), so a symlink here is operator-introduced or
    /// the result of external tampering. The security check uses this flag
    /// to vary the remediation, because `chmod` and `chown` without
    /// symlink-aware flags follow the link and mutate the wrong target.
    pub is_symlink: bool,
}

/// Read uid and permission mode for `path`. Uses `symlink_metadata` so a
/// symlink at this location is treated as the symlink itself — letting a
/// symlink substitute for a runtime-managed path would route writes outside
/// the security-managed tree, matching the policy in
/// `fs_util::create_dir_owner_only`.
#[cfg(unix)]
pub fn inspect(path: &Path, kind: PathKind) -> std::io::Result<PathPosture> {
    let metadata = std::fs::symlink_metadata(path)?;
    Ok(PathPosture {
        path: path.to_path_buf(),
        kind,
        uid: metadata.uid(),
        mode: metadata.permissions().mode() & 0o777,
        is_symlink: metadata.file_type().is_symlink(),
    })
}

#[cfg(not(unix))]
pub fn inspect(path: &Path, kind: PathKind) -> std::io::Result<PathPosture> {
    let metadata = std::fs::symlink_metadata(path)?;
    Ok(PathPosture {
        path: path.to_path_buf(),
        kind,
        uid: 0,
        mode: 0,
        is_symlink: metadata.file_type().is_symlink(),
    })
}

#[cfg(unix)]
pub fn process_euid() -> u32 {
    // SAFETY: `geteuid` is async-signal-safe and has no documented failure mode.
    unsafe { libc::geteuid() }
}

#[cfg(not(unix))]
pub fn process_euid() -> u32 {
    0
}

/// Resolve a username (e.g. `"acp"`) to a uid via `getpwnam_r`. Returns
/// `Ok(None)` when the user does not exist, which the caller maps to "skip the
/// runtime.user_mismatch finding rather than failing the check".
#[cfg(unix)]
pub fn resolve_runtime_user_uid(name: &str) -> std::io::Result<Option<u32>> {
    let cstr = CString::new(name)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidInput, err))?;
    let mut buf: Vec<u8> = vec![0; 1024];
    loop {
        let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        let ret = unsafe {
            libc::getpwnam_r(
                cstr.as_ptr(),
                &mut pwd,
                buf.as_mut_ptr().cast::<libc::c_char>(),
                buf.len(),
                &mut result,
            )
        };
        match ret {
            0 => {
                if result.is_null() {
                    return Ok(None);
                }
                return Ok(Some(pwd.pw_uid));
            }
            libc::ERANGE => {
                // Buffer too small; double it and retry. Cap at 1 MiB so a
                // pathological NSS plugin can't drive us into unbounded
                // growth.
                if buf.len() >= 1 << 20 {
                    return Err(std::io::Error::other(
                        "getpwnam_r buffer would exceed 1 MiB",
                    ));
                }
                buf.resize(buf.len() * 2, 0);
            }
            errno => return Err(std::io::Error::from_raw_os_error(errno)),
        }
    }
}

#[cfg(not(unix))]
pub fn resolve_runtime_user_uid(_name: &str) -> std::io::Result<Option<u32>> {
    Ok(None)
}

#[cfg(unix)]
pub fn current_username() -> std::io::Result<Option<String>> {
    let uid = process_euid();
    let mut buf: Vec<u8> = vec![0; 1024];
    loop {
        let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        let ret = unsafe {
            libc::getpwuid_r(
                uid,
                &mut pwd,
                buf.as_mut_ptr().cast::<libc::c_char>(),
                buf.len(),
                &mut result,
            )
        };
        match ret {
            0 => {
                if result.is_null() {
                    return Ok(None);
                }
                let name = unsafe { std::ffi::CStr::from_ptr(pwd.pw_name) }
                    .to_string_lossy()
                    .into_owned();
                return Ok((!name.is_empty()).then_some(name));
            }
            libc::ERANGE => {
                if buf.len() >= 1 << 20 {
                    return Err(std::io::Error::other(
                        "getpwuid_r buffer would exceed 1 MiB",
                    ));
                }
                buf.resize(buf.len() * 2, 0);
            }
            errno => return Err(std::io::Error::from_raw_os_error(errno)),
        }
    }
}

#[cfg(not(unix))]
pub fn current_username() -> std::io::Result<Option<String>> {
    Ok(std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .ok()
        .filter(|value| !value.trim().is_empty()))
}

/// Probe writability by trying to create a temp file inside `path`. Cleans up
/// on drop (`NamedTempFile`), so this leaves no artifact. Returns `false` if
/// the directory does not exist or the daemon cannot write into it.
pub fn workspace_writable(path: &Path) -> bool {
    if !path.is_dir() {
        return false;
    }
    tempfile::NamedTempFile::new_in(path).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs_util::{create_dir_owner_only, write_new_file_owner_only};

    #[test]
    fn inspect_directory_reports_path_and_kind() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let dir = tempdir.path().join("acp-config");
        create_dir_owner_only(&dir).expect("create");

        let posture = inspect(&dir, PathKind::ConfigDir).expect("inspect");
        assert_eq!(posture.path, dir);
        assert_eq!(posture.kind, PathKind::ConfigDir);
        assert!(!posture.is_symlink);
        #[cfg(unix)]
        {
            assert_eq!(posture.mode, 0o700);
            assert_eq!(posture.uid, process_euid());
        }
    }

    #[cfg(unix)]
    #[test]
    fn inspect_symlink_reports_is_symlink_true() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let target = tempdir.path().join("real-dir");
        create_dir_owner_only(&target).expect("create real");
        let link = tempdir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");

        let posture = inspect(&link, PathKind::ConfigDir).expect("inspect");
        assert!(
            posture.is_symlink,
            "inspect must report is_symlink=true when the managed path is a symlink"
        );
    }

    #[test]
    fn inspect_file_reports_owner_only_mode() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = tempdir.path().join("age.key");
        write_new_file_owner_only(&path, b"x").expect("write");

        let posture = inspect(&path, PathKind::AgeKey).expect("inspect");
        #[cfg(unix)]
        assert_eq!(posture.mode, 0o600);
    }

    #[test]
    fn inspect_missing_path_returns_error() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let missing = tempdir.path().join("does-not-exist");
        let result = inspect(&missing, PathKind::ConfigDir);
        assert!(result.is_err());
    }

    #[test]
    fn workspace_writable_true_for_writable_tempdir() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        assert!(workspace_writable(tempdir.path()));
    }

    #[test]
    fn workspace_writable_false_for_missing_path() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let missing = tempdir.path().join("nope");
        assert!(!workspace_writable(&missing));
    }

    #[cfg(unix)]
    #[test]
    fn workspace_writable_false_for_read_only_dir() {
        // Skip the test when running as root: 0o555 doesn't block writes for
        // the superuser.
        if process_euid() == 0 {
            return;
        }
        let tempdir = tempfile::tempdir().expect("tempdir");
        let readonly = tempdir.path().join("ro");
        std::fs::create_dir(&readonly).expect("create");
        std::fs::set_permissions(&readonly, std::fs::Permissions::from_mode(0o555)).expect("chmod");
        let probe = readonly.join("probe");
        if std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe)
            .is_ok()
        {
            std::fs::remove_file(&probe).expect("remove probe");
            std::fs::set_permissions(&readonly, std::fs::Permissions::from_mode(0o700))
                .expect("restore mode");
            return;
        }
        assert!(!workspace_writable(&readonly));
        // Restore mode so the test framework can clean up.
        std::fs::set_permissions(&readonly, std::fs::Permissions::from_mode(0o700))
            .expect("restore mode");
    }

    #[cfg(unix)]
    #[test]
    fn resolve_runtime_user_uid_for_root_is_zero() {
        let uid = resolve_runtime_user_uid("root").expect("getpwnam_r");
        assert_eq!(uid, Some(0));
    }

    #[test]
    fn resolve_runtime_user_uid_for_missing_user_is_none() {
        let uid = resolve_runtime_user_uid("definitely-no-such-user-xyz-acp-stack-test")
            .expect("getpwnam_r returns Ok even when user is missing");
        assert!(uid.is_none());
    }

    #[test]
    fn expected_mode_for_workspace_root_is_none() {
        assert_eq!(PathKind::WorkspaceRoot.expected_mode(), None);
        assert_eq!(PathKind::ConfigDir.expected_mode(), Some(0o700));
        assert_eq!(PathKind::ConfigFile.expected_mode(), Some(0o600));
        assert_eq!(PathKind::StateDb.expected_mode(), Some(0o600));
        assert_eq!(PathKind::SecretStore.expected_mode(), Some(0o600));
        assert_eq!(PathKind::ConfigFile.label(), "config file");
        assert_eq!(PathKind::StateDb.label(), "state database");
    }
}
