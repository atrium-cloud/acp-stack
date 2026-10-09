//! Filesystem access on behalf of the workload identity, the Unix uid/gid the sandboxed Agent runs
//! as.
//!
//! - [`Executor`] runs a job with the process's own credentials or, for a workload identity, on a
//!   dedicated thread holding the workload's filesystem credentials.
//! - The walker ([`Anchor`], [`read_file`], [`write_file_atomic`], and siblings) walks below its
//!   anchor by its [`LinkPolicy`]. Walks on a workload thread follow links, so the identity's own
//!   credentials decide what a link reaches; walks with the runtime's credentials over a tree the
//!   workload can write never follow a symlink and refuse hard-linked targets.
//! - [`handoff_tree`] moves a runtime-staged tree into a workload-owned destination.
//! - [`workload_writable_components`] and [`check_exec_chain`] report what the workload identity
//!   can write or execute.
//!
//! The credential switch uses raw syscalls because glibc's setgroups/setresuid wrappers broadcast
//! to every thread in the process. Two invariants keep it sound:
//! - Only job closures run on a workload thread: setfsuid can return to the thread's real uid
//!   without a capability, so any other code there could regain the runtime's file access.
//! - Nothing in-process may call a setxid libc function: glibc would broadcast it and reset every
//!   thread's fsuid, including workload threads mid-job.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;

use crate::error::{Result, StackError};

mod handoff;
mod walk;

pub use handoff::{HandoffOptions, HandoffSummary, SymlinkPolicy, handoff_tree};
pub use walk::{
    Anchor, EntryInfo, EntryKind, LinkPolicy, WriteOptions, copy_file, create_dir_all, list_dir,
    open_file, read_file, read_file_with_info, read_link, remove_empty_dir, remove_file,
    remove_link_target, remove_tree, rename, stat, stat_followed, symlink, write_file_atomic,
    write_file_new,
};

// === CONSTANTS ===

/// Default bound for one workload filesystem job.
pub const DEFAULT_JOB_TIMEOUT: Duration = Duration::from_secs(60);
/// Matches the kernel's MAXSYMLINKS.
const SYMLINK_CHAIN_MAX_HOPS: usize = 40;
const NOT_DIRECTORY_REASON: &str = "a path component is not a directory";
/// File mode for runtime-owned writes made without a workload identity.
pub const OWNER_ONLY_FILE_MODE: u32 = 0o600;
/// Directory mode for runtime-owned writes made without a workload identity.
pub const OWNER_ONLY_DIR_MODE: u32 = 0o700;
/// File mode for workload-owned writes; the process umask narrows it.
pub const UMASK_FILE_MODE: u32 = 0o666;
/// Directory mode for workload-owned writes; the process umask narrows it.
pub const UMASK_DIR_MODE: u32 = 0o777;

const WORKLOAD_THREAD_NAME: &str = "acps-workload-fs";
const ACCESS_CHECK_OPERATION: &str = "access check";

/// An invalid id makes setfsuid/setfsgid return the current value without changing it.
#[cfg(target_os = "linux")]
const QUERY_FS_ID: u32 = u32::MAX;

/// Filesystem uid and gid a workload job runs with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FsCredentials {
    pub uid: u32,
    pub gid: u32,
}

/// Where a filesystem job runs and with which credentials.
#[derive(Clone, Debug)]
pub enum Executor {
    /// Inline (or on the blocking pool for async callers) with the process's own credentials.
    Process,
    /// On a dedicated thread holding only the workload's filesystem credentials.
    Workload(FsCredentials),
}

impl Executor {
    /// Run `job` and return its result. `Process` runs it inline on the caller thread, where
    /// `timeout` does not apply. `Workload` runs it on a fresh credential-switched thread and
    /// abandons that thread once `timeout` elapses.
    pub fn run<T: Send + 'static>(
        &self,
        timeout: Duration,
        job: impl FnOnce() -> Result<T> + Send + 'static,
    ) -> Result<T> {
        match self {
            Executor::Process => job(),
            Executor::Workload(credentials) => {
                let (sender, receiver) = std::sync::mpsc::channel();
                spawn_workload_thread(*credentials, job, move |outcome| {
                    if sender.send(outcome).is_err() {
                        tracing::warn!(
                            "workload filesystem job finished after its caller stopped waiting"
                        );
                    }
                })?;
                match receiver.recv_timeout(timeout) {
                    Ok(outcome) => outcome,
                    Err(RecvTimeoutError::Timeout) => {
                        Err(StackError::WorkloadFsTimeout { timeout })
                    }
                    Err(RecvTimeoutError::Disconnected) => Err(job_ended_without_result()),
                }
            }
        }
    }

    /// Async variant of [`Executor::run`] that never blocks the tokio worker. `Process` uses the
    /// blocking pool, which is safe because it changes no credentials; `Workload` never touches
    /// the pool because reused pool threads would leak the switched credentials.
    pub async fn run_async<T: Send + 'static>(
        &self,
        timeout: Duration,
        job: impl FnOnce() -> Result<T> + Send + 'static,
    ) -> Result<T> {
        match self {
            Executor::Process => {
                match tokio::time::timeout(timeout, tokio::task::spawn_blocking(job)).await {
                    Ok(Ok(outcome)) => outcome,
                    Ok(Err(join_error)) => Err(StackError::WorkloadFsExecutorFailed {
                        reason: format!("blocking job did not complete: {join_error}"),
                    }),
                    Err(_elapsed) => Err(StackError::WorkloadFsTimeout { timeout }),
                }
            }
            Executor::Workload(credentials) => {
                let (sender, receiver) = tokio::sync::oneshot::channel();
                spawn_workload_thread(*credentials, job, move |outcome| {
                    if sender.send(outcome).is_err() {
                        tracing::warn!(
                            "workload filesystem job finished after its caller stopped waiting"
                        );
                    }
                })?;
                match tokio::time::timeout(timeout, receiver).await {
                    Ok(Ok(outcome)) => outcome,
                    Ok(Err(_closed)) => Err(job_ended_without_result()),
                    Err(_elapsed) => Err(StackError::WorkloadFsTimeout { timeout }),
                }
            }
        }
    }
}

fn spawn_workload_thread<T, F, D>(credentials: FsCredentials, job: F, deliver: D) -> Result<()>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
    D: FnOnce(Result<T>) + Send + 'static,
{
    let spawned = std::thread::Builder::new()
        .name(WORKLOAD_THREAD_NAME.to_owned())
        .spawn(move || {
            let outcome = enter_workload_credentials(credentials).and_then(|()| job());
            deliver(outcome);
        });
    match spawned {
        // Dropping the handle detaches the thread, so a job stuck in a hung FUSE or NFS call never
        // holds the caller past its timeout.
        Ok(handle) => {
            drop(handle);
            Ok(())
        }
        Err(source) => Err(StackError::WorkloadFsExecutorFailed {
            reason: format!("failed to spawn the workload thread: {source}"),
        }),
    }
}

fn job_ended_without_result() -> StackError {
    StackError::WorkloadFsExecutorFailed {
        reason: "the workload thread ended without a result (the job panicked)".to_owned(),
    }
}

#[cfg(target_os = "linux")]
fn credentials_failed(step: &str, source: impl std::fmt::Display) -> StackError {
    StackError::WorkloadFsCredentialsFailed {
        reason: format!("{step}: {source}"),
    }
}

/// Switch the calling thread, and only it, to the workload's filesystem credentials, then verify
/// the switch before any job code runs. rustix issues the raw per-thread syscalls.
#[cfg(target_os = "linux")]
fn enter_workload_credentials(credentials: FsCredentials) -> Result<()> {
    use rustix::thread::{CapabilitySet, CapabilitySets};

    rustix::thread::set_thread_groups(&[])
        .map_err(|source| credentials_failed("setgroups", source))?;
    // SAFETY: setfsgid/setfsuid are per-thread syscalls. They return the previous id rather than
    // an error, so verify_thread_credentials checks the outcome.
    unsafe {
        libc::setfsgid(credentials.gid);
        libc::setfsuid(credentials.uid);
    }
    let none = CapabilitySets {
        effective: CapabilitySet::empty(),
        permitted: CapabilitySet::empty(),
        inheritable: CapabilitySet::empty(),
    };
    rustix::thread::set_capabilities(None, none)
        .map_err(|source| credentials_failed("capset", source))?;
    verify_thread_credentials(credentials)
}

#[cfg(target_os = "linux")]
fn verify_thread_credentials(credentials: FsCredentials) -> Result<()> {
    // SAFETY: an invalid id leaves the filesystem ids unchanged and returns the current values.
    let (fsuid, fsgid) = unsafe { (libc::setfsuid(QUERY_FS_ID), libc::setfsgid(QUERY_FS_ID)) };
    let (fsuid, fsgid) = (fsuid as u32, fsgid as u32);
    if fsuid != credentials.uid || fsgid != credentials.gid {
        return Err(StackError::WorkloadFsCredentialsFailed {
            reason: format!(
                "thread fsuid/fsgid is {fsuid}/{fsgid}, expected {}/{}",
                credentials.uid, credentials.gid
            ),
        });
    }
    let held = rustix::thread::capabilities(None)
        .map_err(|source| credentials_failed("capget", source))?;
    if !held.effective.is_empty() || !held.permitted.is_empty() {
        return Err(StackError::WorkloadFsCredentialsFailed {
            reason: format!(
                "thread still holds capabilities: effective {:#x}, permitted {:#x}",
                held.effective.bits(),
                held.permitted.bits()
            ),
        });
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn enter_workload_credentials(_credentials: FsCredentials) -> Result<()> {
    Err(StackError::WorkloadFsCredentialsFailed {
        reason: "workload filesystem credentials require Linux".to_owned(),
    })
}

fn path_cstring(path: &Path) -> Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| StackError::WorkloadFsInvalidPath {
        path: path.to_path_buf(),
        reason: "contains a NUL byte",
    })
}

fn require_absolute(path: &Path) -> Result<()> {
    if path.is_absolute() {
        return Ok(());
    }
    Err(StackError::WorkloadFsInvalidPath {
        path: path.to_path_buf(),
        reason: "must be absolute",
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Access {
    Granted,
    Denied,
    Missing,
}

/// Access check against the calling thread's filesystem credentials and effective capabilities.
fn check_access(path: &Path, mode: libc::c_int) -> Result<Access> {
    let c_path = path_cstring(path)?;
    if effective_access(&c_path, mode) == 0 {
        return Ok(Access::Granted);
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::EACCES | libc::EPERM | libc::EROFS) => Ok(Access::Denied),
        Some(libc::ENOENT | libc::ENOTDIR) => Ok(Access::Missing),
        _ => Err(StackError::WorkloadFsIo {
            path: path.to_path_buf(),
            operation: ACCESS_CHECK_OPERATION,
            source: error,
        }),
    }
}

/// Raw faccessat2 because glibc emulates `AT_EACCESS` with the effective uid when the syscall is
/// missing, which would ignore a workload thread's fsuid.
#[cfg(target_os = "linux")]
fn effective_access(path: &std::ffi::CStr, mode: libc::c_int) -> libc::c_long {
    // SAFETY: `path` is NUL-terminated and outlives the call.
    unsafe {
        libc::syscall(
            libc::SYS_faccessat2,
            libc::AT_FDCWD,
            path.as_ptr(),
            mode,
            libc::AT_EACCESS,
        )
    }
}

#[cfg(not(target_os = "linux"))]
fn effective_access(path: &std::ffi::CStr, mode: libc::c_int) -> libc::c_long {
    // SAFETY: `path` is NUL-terminated and outlives the call.
    libc::c_long::from(unsafe {
        libc::faccessat(libc::AT_FDCWD, path.as_ptr(), mode, libc::AT_EACCESS)
    })
}

/// Whether this kernel answers the access checks the workload identity relies on: `faccessat2`
/// arrived in Linux 5.8, and without it every check fails with ENOSYS.
pub fn preflight_access_checks() -> std::result::Result<(), String> {
    let root = path_cstring(Path::new("/")).map_err(|error| error.to_string())?;
    if effective_access(&root, libc::F_OK) == 0 {
        return Ok(());
    }
    Err(format!(
        "workload access checks are unavailable ({}); a workload identity requires Linux 5.8 or newer",
        std::io::Error::last_os_error()
    ))
}

fn first_writable(candidates: &[PathBuf]) -> Result<Option<PathBuf>> {
    for candidate in candidates {
        if check_access(candidate, libc::W_OK)? == Access::Granted {
            return Ok(Some(candidate.clone()));
        }
    }
    Ok(None)
}

/// For each of `paths`, in order and as one job, the first of the path and its ancestors up to `/`
/// that the executor's identity can write; missing components are skipped.
pub fn workload_writable_components(
    executor: &Executor,
    paths: &[PathBuf],
) -> Result<Vec<Option<PathBuf>>> {
    let mut candidate_sets = Vec::with_capacity(paths.len());
    for path in paths {
        require_absolute(path)?;
        candidate_sets.push(path.ancestors().map(Path::to_path_buf).collect::<Vec<_>>());
    }
    executor.run(DEFAULT_JOB_TIMEOUT, move || {
        candidate_sets
            .iter()
            .map(|candidates| first_writable(candidates))
            .collect()
    })
}

/// Every hop from `path` through its final-component symlinks, ending with the first non-symlink,
/// or with the first missing hop when `missing_ends_chain`. Relative link targets resolve against
/// the link's parent directory.
fn symlink_chain(path: &Path, max_hops: usize, missing_ends_chain: bool) -> Result<Vec<PathBuf>> {
    require_absolute(path)?;
    let mut chain = vec![path.to_path_buf()];
    loop {
        let current = &chain[chain.len() - 1];
        let metadata = match std::fs::symlink_metadata(current) {
            Ok(metadata) => metadata,
            Err(source) if missing_ends_chain && source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(chain);
            }
            Err(source) => return Err(chain_failure(current, "lstat", source)),
        };
        if !metadata.file_type().is_symlink() {
            return Ok(chain);
        }
        if chain.len() > max_hops {
            return Err(StackError::WorkloadFsSymlinkLoop {
                path: path.to_path_buf(),
                max_hops,
            });
        }
        let target = std::fs::read_link(current)
            .map_err(|source| chain_failure(current, "readlink", source))?;
        let next = match current.parent() {
            Some(parent) if target.is_relative() => parent.join(target),
            _ => target,
        };
        if chain.contains(&next) {
            return Err(StackError::WorkloadFsSymlinkLoop {
                path: path.to_path_buf(),
                max_hops,
            });
        }
        chain.push(next);
    }
}

fn chain_failure(path: &Path, operation: &'static str, source: std::io::Error) -> StackError {
    match source.raw_os_error() {
        Some(libc::ENOENT) => {
            return StackError::WorkloadFsNotFound {
                path: path.to_path_buf(),
            };
        }
        Some(libc::ELOOP) => {
            return StackError::WorkloadFsSymlinkLoop {
                path: path.to_path_buf(),
                max_hops: SYMLINK_CHAIN_MAX_HOPS,
            };
        }
        Some(libc::ENOTDIR) => {
            return StackError::WorkloadFsInvalidPath {
                path: path.to_path_buf(),
                reason: NOT_DIRECTORY_REASON,
            };
        }
        _ => {}
    }
    StackError::WorkloadFsIo {
        path: path.to_path_buf(),
        operation,
        source,
    }
}

/// Refuse an executable the executor's identity could not run or could swap: the final target of
/// `path`'s symlink chain must be readable and executable, and no hop, no lexical ancestor of a
/// hop, and no ancestor of a hop's canonical parent may be writable. The canonical parents cover
/// directory symlinks among the ancestors (merged-usr `/bin -> usr/bin` would otherwise leave
/// `/usr` unchecked).
pub fn check_exec_chain(executor: &Executor, path: &Path) -> Result<()> {
    let (target, candidates) = exec_chain_candidates(path, false)?;
    executor.run(DEFAULT_JOB_TIMEOUT, move || {
        if check_access(&target, libc::R_OK | libc::X_OK)? != Access::Granted {
            return Err(StackError::WorkloadFsWorkloadUnreadable { path: target });
        }
        match first_writable(&candidates)? {
            Some(writable) => Err(StackError::WorkloadFsWorkloadWritable { path: writable }),
            None => Ok(()),
        }
    })
}

/// The first path the executor's identity could write to swap the executable `path` resolves
/// to, under the same hop and ancestor rules as [`check_exec_chain`], for executables the runtime
/// itself runs.
pub fn exec_chain_writable_component(executor: &Executor, path: &Path) -> Result<Option<PathBuf>> {
    let (_target, candidates) = exec_chain_candidates(path, false)?;
    executor.run(DEFAULT_JOB_TIMEOUT, move || first_writable(&candidates))
}

/// [`exec_chain_writable_component`] for a path the runtime reads later and that may not exist
/// yet: the chain ends at the first missing hop, whose existing ancestors are still checked, so
/// the identity cannot create the file in a directory it can write.
pub fn future_path_writable_component(executor: &Executor, path: &Path) -> Result<Option<PathBuf>> {
    let (_target, candidates) = exec_chain_candidates(path, true)?;
    executor.run(DEFAULT_JOB_TIMEOUT, move || first_writable(&candidates))
}

/// The final target of `path`'s symlink chain, and every path whose writability would let an
/// identity swap that target. With `missing_ends_chain`, a missing hop ends the chain and its
/// parent is canonicalized through the deepest ancestor that exists.
fn exec_chain_candidates(path: &Path, missing_ends_chain: bool) -> Result<(PathBuf, Vec<PathBuf>)> {
    let chain = symlink_chain(path, SYMLINK_CHAIN_MAX_HOPS, missing_ends_chain)?;
    let mut candidates: Vec<PathBuf> = Vec::new();
    let mut add = |candidate: &Path| {
        if !candidates.iter().any(|known| known == candidate) {
            candidates.push(candidate.to_path_buf());
        }
    };
    for hop in &chain {
        hop.ancestors().for_each(&mut add);
        if let Some(parent) = hop.parent() {
            let existing = if missing_ends_chain {
                parent
                    .ancestors()
                    .find(|ancestor| ancestor.exists())
                    .unwrap_or(parent)
            } else {
                parent
            };
            let canonical_parent = std::fs::canonicalize(existing)
                .map_err(|source| chain_failure(existing, "canonicalize", source))?;
            canonical_parent.ancestors().for_each(&mut add);
        }
    }
    let target = chain.last().cloned().unwrap_or_else(|| path.to_path_buf());
    Ok((target, candidates))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    fn is_root() -> bool {
        crate::ownership::process_euid() == 0
    }

    #[tokio::test]
    async fn process_executor_async_jobs_time_out() {
        let error = Executor::Process
            .run_async(Duration::from_millis(10), || {
                std::thread::sleep(Duration::from_millis(300));
                Ok(())
            })
            .await
            .expect_err("timeout");
        assert!(matches!(error, StackError::WorkloadFsTimeout { .. }));
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn workload_executor_requires_linux_and_never_runs_the_job() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let ran = Arc::new(AtomicBool::new(false));
        let ran_in_job = Arc::clone(&ran);
        let executor = Executor::Workload(FsCredentials { uid: 1, gid: 1 });
        let error = executor
            .run(DEFAULT_JOB_TIMEOUT, move || {
                ran_in_job.store(true, Ordering::SeqCst);
                Ok(())
            })
            .expect_err("credentials error");
        assert!(matches!(
            error,
            StackError::WorkloadFsCredentialsFailed { .. }
        ));
        assert!(!ran.load(Ordering::SeqCst));
    }

    #[test]
    fn writable_components_report_the_first_writable_path_or_ancestor() {
        if is_root() {
            return;
        }
        let tempdir = tempfile::tempdir().expect("tempdir");
        let writable_file = tempdir.path().join("file");
        std::fs::write(&writable_file, b"x").expect("write");
        let read_only = tempdir.path().join("read-only");
        std::fs::create_dir(&read_only).expect("dir");
        let read_only_file = read_only.join("file");
        std::fs::write(&read_only_file, b"x").expect("write");
        std::fs::set_permissions(&read_only_file, std::fs::Permissions::from_mode(0o444))
            .expect("chmod");
        std::fs::set_permissions(&read_only, std::fs::Permissions::from_mode(0o555))
            .expect("chmod");

        let found = workload_writable_components(
            &Executor::Process,
            &[
                writable_file.clone(),
                read_only_file,
                read_only.join("missing/deeper"),
            ],
        );
        std::fs::set_permissions(&read_only, std::fs::Permissions::from_mode(0o755))
            .expect("restore");

        let parent = Some(tempdir.path().to_path_buf());
        assert_eq!(
            found.expect("check"),
            vec![Some(writable_file), parent.clone(), parent]
        );
        assert!(matches!(
            workload_writable_components(&Executor::Process, &[PathBuf::from("relative")]),
            Err(StackError::WorkloadFsInvalidPath { .. })
        ));
    }

    #[test]
    fn symlink_chain_resolves_relative_and_absolute_hops() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let root = tempdir.path();
        std::fs::create_dir(root.join("bin")).expect("bin");
        std::fs::create_dir(root.join("real")).expect("real");
        let target = root.join("real/agent");
        std::fs::write(&target, b"#!/bin/sh\n").expect("target");
        std::os::unix::fs::symlink("../real/agent", root.join("bin/relative")).expect("relative");
        std::os::unix::fs::symlink(root.join("bin/relative"), root.join("entry"))
            .expect("absolute");

        let chain =
            symlink_chain(&root.join("entry"), SYMLINK_CHAIN_MAX_HOPS, false).expect("chain");
        assert_eq!(
            chain,
            vec![
                root.join("entry"),
                root.join("bin/relative"),
                root.join("bin/../real/agent"),
            ]
        );
        assert_eq!(
            std::fs::canonicalize(&chain[2]).expect("canonical"),
            std::fs::canonicalize(&target).expect("canonical")
        );
    }

    #[test]
    fn symlink_chain_refuses_loops_and_overlong_chains() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let root = tempdir.path();
        std::os::unix::fs::symlink("b", root.join("a")).expect("a");
        std::os::unix::fs::symlink("a", root.join("b")).expect("b");
        let looped =
            symlink_chain(&root.join("a"), SYMLINK_CHAIN_MAX_HOPS, false).expect_err("loop");
        assert!(matches!(looped, StackError::WorkloadFsSymlinkLoop { .. }));

        std::fs::write(root.join("end"), b"x").expect("end");
        std::os::unix::fs::symlink("end", root.join("hop2")).expect("hop2");
        std::os::unix::fs::symlink("hop2", root.join("hop1")).expect("hop1");
        let capped = symlink_chain(&root.join("hop1"), 1, false).expect_err("cap");
        assert!(matches!(capped, StackError::WorkloadFsSymlinkLoop { .. }));
        assert_eq!(
            symlink_chain(&root.join("hop1"), 2, false)
                .expect("two hops")
                .len(),
            3
        );
    }

    #[test]
    fn missing_hops_end_the_chain_and_keep_their_existing_ancestors() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let root = std::fs::canonicalize(tempdir.path()).expect("canonical root");
        std::fs::create_dir(root.join("links")).expect("links");
        std::fs::create_dir(root.join("drop")).expect("drop");
        std::os::unix::fs::symlink(root.join("drop/script.sh"), root.join("links/dangling"))
            .expect("dangling");

        assert!(matches!(
            exec_chain_candidates(&root.join("links/dangling"), false),
            Err(StackError::WorkloadFsNotFound { .. })
        ));
        let (_, dangling) =
            exec_chain_candidates(&root.join("links/dangling"), true).expect("dangling chain");
        assert!(dangling.contains(&root.join("links")));
        assert!(dangling.contains(&root.join("drop")));

        let (_, absent) = exec_chain_candidates(&root.join("drop/absent/deeper/file"), true)
            .expect("absent chain");
        assert!(absent.contains(&root.join("drop/absent")));
        assert!(absent.contains(&root.join("drop")));
    }

    #[test]
    fn exec_chain_refuses_a_target_without_execute_permission() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let target = tempdir.path().join("agent");
        std::fs::write(&target, b"#!/bin/sh\n").expect("write");
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).expect("chmod");

        let error = check_exec_chain(&Executor::Process, &target).expect_err("not executable");
        assert!(matches!(
            error,
            StackError::WorkloadFsWorkloadUnreadable { .. }
        ));
    }

    #[test]
    fn exec_chain_refuses_a_writable_hop_ancestor() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let target = tempdir.path().join("agent");
        std::fs::write(&target, b"#!/bin/sh\n").expect("write");
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        let error = check_exec_chain(&Executor::Process, &target).expect_err("writable");
        assert!(matches!(
            error,
            StackError::WorkloadFsWorkloadWritable { .. }
        ));
    }

    #[test]
    fn exec_chain_accepts_a_system_binary() {
        if is_root() {
            return;
        }
        check_exec_chain(&Executor::Process, Path::new("/bin/sh")).expect("system shell");
    }

    #[cfg(target_os = "linux")]
    fn lookup_user(name: &str) -> (u32, u32) {
        let c_name = CString::new(name).expect("user name");
        let mut buffer = vec![0u8; 16 * 1024];
        // SAFETY: zeroed passwd is a valid out-parameter for getpwnam_r.
        let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: every pointer refers to a live buffer of the stated length.
        let status = unsafe {
            libc::getpwnam_r(
                c_name.as_ptr(),
                &mut entry,
                buffer.as_mut_ptr().cast::<libc::c_char>(),
                buffer.len(),
                &mut result,
            )
        };
        assert_eq!(status, 0, "getpwnam_r failed for {name}");
        assert!(!result.is_null(), "no such user: {name}");
        (entry.pw_uid, entry.pw_gid)
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires CAP_SETUID/CAP_SETGID and ACPS_TEST_WORKLOAD_USER"]
    fn workload_executor_switches_filesystem_credentials_for_the_job_only() {
        use std::os::unix::fs::MetadataExt as _;

        let user = std::env::var("ACPS_TEST_WORKLOAD_USER").expect("ACPS_TEST_WORKLOAD_USER");
        let (uid, gid) = lookup_user(&user);
        let caller_capabilities = rustix::thread::capabilities(None).expect("caller capabilities");

        let tempdir = tempfile::tempdir().expect("tempdir");
        std::fs::set_permissions(tempdir.path(), std::fs::Permissions::from_mode(0o777))
            .expect("open tempdir to the workload user");
        let private = tempdir.path().join("private");
        std::fs::write(&private, b"secret").expect("private");
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o600))
            .expect("chmod private");

        let executor = Executor::Workload(FsCredentials { uid, gid });
        let root = tempdir.path().to_path_buf();
        executor
            .run(DEFAULT_JOB_TIMEOUT, move || {
                let anchor = Anchor::open(&root)?;
                write_file_new(
                    &anchor,
                    Path::new("created.txt"),
                    b"hello",
                    &WriteOptions {
                        create_parents: false,
                        file_mode: UMASK_FILE_MODE,
                        dir_mode: UMASK_DIR_MODE,
                        require_existing_owner: None,
                    },
                )
            })
            .expect("workload write");
        let created = std::fs::symlink_metadata(tempdir.path().join("created.txt"))
            .expect("created metadata");
        assert_eq!(created.uid(), uid);
        assert_eq!(created.gid(), gid);

        let root = tempdir.path().to_path_buf();
        let denied = executor.run(DEFAULT_JOB_TIMEOUT, move || {
            let anchor = Anchor::open(&root)?;
            read_file(&anchor, Path::new("private"), 1024)
        });
        assert!(
            matches!(
                &denied,
                Err(StackError::WorkloadFsIo { source, .. })
                    if source.kind() == std::io::ErrorKind::PermissionDenied
            ),
            "workload read of a runtime-private file must be denied: {denied:?}"
        );

        assert_eq!(
            rustix::thread::capabilities(None).expect("caller capabilities after"),
            caller_capabilities
        );
    }
}
