//! Isolation backends (`off`/`unshare`/`bwrap`/`custom`) that wrap each agent
//! spawn so an untrusted workload cannot reach the daemon's secrets or socket.

#[cfg(target_os = "linux")]
use std::ffi::CString;
#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(target_os = "linux")]
use rustix::thread::{CapabilitySet, CapabilitySets};

use crate::config::{SandboxConfig, SandboxMode};
use crate::error::{Result, StackError};
use crate::extensions::NetworkProviderExtension;

pub mod cgroup;
mod identity;
pub mod supervise;

pub use cgroup::WorkloadCgroup;
pub use identity::{SandboxProfile, WorkloadIdentity};

// CONSTANTS

/// Internal subcommand the `unshare` wrapper re-invokes (`acps __sandbox-exec`).
pub const SANDBOX_EXEC_SUBCOMMAND: &str = "__sandbox-exec";

/// Internal subcommand that supervises a network-isolated spawn (`acps __sandbox-supervise`).
pub const SANDBOX_SUPERVISE_SUBCOMMAND: &str = "__sandbox-supervise";

/// Internal subcommand that keeps a provider and its descendants in a liveness-monitored process group.
pub const SANDBOX_PROVIDER_SUPERVISE_SUBCOMMAND: &str = "__sandbox-provider-supervise";

/// The wrapped chain's own helpers, which keep the capabilities the runtime hands them.
const SANDBOX_HELPER_SUBCOMMANDS: [&str; 3] = [
    SANDBOX_EXEC_SUBCOMMAND,
    SANDBOX_SUPERVISE_SUBCOMMAND,
    SANDBOX_PROVIDER_SUPERVISE_SUBCOMMAND,
];

/// Fixed child fd the spawn sites dup the daemon's stderr onto, so supervisor diagnostics reach the operator even when the workload's stderr is a captured pipe.
pub const SANDBOX_DIAG_FD: i32 = 3;

/// `mkdtemp` template for the directory where the `__sandbox-exec` helper stages the empty file it binds
/// over a masked non-directory path. Each spawn gets its own directory, torn down before the workload execs.
#[cfg(target_os = "linux")]
const MASK_FILE_STAGING_TEMPLATE: &str = ".acps-sandbox-mask-XXXXXX";
#[cfg(target_os = "linux")]
const MASK_FILE_STAGING_NAME: &str = "empty";

const UNSHARE_FLAGS: &[&str] = &[
    "--mount",
    "--uts",
    "--ipc",
    "--pid",
    "--fork",
    "--mount-proc",
    "--kill-child",
    "--propagation",
    "private",
];

/// Drops every capability set plus `no_new_privs`, so a setuid binary inside the sandbox cannot regain privilege.
const SETPRIV_DROP_FLAGS: &[&str] = &[
    "--clear-groups",
    "--inh-caps=-all",
    "--ambient-caps=-all",
    "--bounding-set=-all",
    "--no-new-privs",
];

/// Re-arms SIGKILL-on-parent-death after the uid change: the kernel clears the
/// death signal `unshare --kill-child` set as soon as the effective uid changes,
/// which would let a re-uid'd workload outlive its namespace's `unshare`.
const SETPRIV_PARENT_DEATH_FLAGS: &[&str] = &["--pdeathsig", "KILL"];

/// Capabilities a re-uid'ing chain needs: `--regid`/`--clear-groups`, `--reuid`, and
/// `--bounding-set`.
#[cfg(target_os = "linux")]
const IDENTITY_CAPABILITIES: [(CapabilitySet, &str); 3] = [
    (CapabilitySet::SETUID, "CAP_SETUID"),
    (CapabilitySet::SETGID, "CAP_SETGID"),
    (CapabilitySet::SETPCAP, "CAP_SETPCAP"),
];

const BWRAP_BASE_FLAGS: &[&str] = &[
    "--ro-bind",
    "/",
    "/",
    "--dev",
    "/dev",
    "--proc",
    "/proc",
    "--unshare-pid",
    "--unshare-ipc",
    "--unshare-uts",
    "--die-with-parent",
    "--new-session",
];

const STANDARD_BIN_DIRS: &[&str] = &["/usr/bin", "/bin", "/usr/local/bin", "/usr/sbin", "/sbin"];

/// A spawn command after sandbox wrapping: the program to exec and its full argv.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrappedCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
}

/// The daemon's own paths that must be unreadable from inside the sandbox; derived from the runtime path helpers, never from operator config.
pub fn sensitive_mask_paths(home: &Path, sandbox: &SandboxConfig) -> Vec<PathBuf> {
    let mut paths = vec![
        crate::secrets::config_dir(home),
        crate::secrets::state_dir(home),
    ];
    paths.extend(sandbox.mask_paths.iter().map(PathBuf::from));
    paths
}

/// Wrap `program`/`args` according to `profile`; a declared `network` extension (`unshare` only) also moves the spawn into an isolated network namespace.
/// `home` is the runtime home whose config and state directories get masked.
pub fn wrap(
    profile: &SandboxProfile,
    network: Option<&NetworkProviderExtension>,
    program: &Path,
    args: &[String],
    home: &Path,
    workspace_root: &Path,
) -> Result<WrappedCommand> {
    let sandbox = &profile.config;
    match sandbox.mode {
        SandboxMode::Off => Ok(wrap_off(profile, program, args)),
        SandboxMode::Unshare => wrap_unshare(profile, network, program, args, home),
        SandboxMode::Bwrap => Ok(wrap_bwrap(sandbox, program, args, home, workspace_root)),
        SandboxMode::Custom => wrap_custom(sandbox, program, args),
    }
}

/// `off` is a verbatim passthrough unless an identity is declared, which adds only the
/// privilege drop: no namespaces, no masks.
fn wrap_off(profile: &SandboxProfile, program: &Path, args: &[String]) -> WrappedCommand {
    let Some(identity) = &profile.identity else {
        return WrappedCommand {
            program: program.to_path_buf(),
            args: args.to_vec(),
        };
    };
    // No parent-death signal here: the parent is a runtime thread, and the death
    // signal fires when that thread exits.
    let mut argv = setpriv_drop_argv(identity.uid, identity.gid, false);
    let setpriv = PathBuf::from(argv.remove(0));
    argv.push(program.to_string_lossy().into_owned());
    argv.extend(args.iter().cloned());
    WrappedCommand {
        program: setpriv,
        args: argv,
    }
}

/// `setpriv` argv, program first and ending in `--`, that drops to `uid`/`gid` with
/// every capability set cleared and `no_new_privs` set.
fn setpriv_drop_argv(uid: u32, gid: u32, parent_death_kill: bool) -> Vec<String> {
    let mut out = vec![
        resolve_bin("setpriv").to_string_lossy().into_owned(),
        format!("--reuid={uid}"),
        format!("--regid={gid}"),
    ];
    out.extend(SETPRIV_DROP_FLAGS.iter().map(|s| s.to_string()));
    if parent_death_kill {
        out.extend(SETPRIV_PARENT_DEATH_FLAGS.iter().map(|s| s.to_string()));
    }
    out.push("--".to_owned());
    out
}

fn wrap_unshare(
    profile: &SandboxProfile,
    network: Option<&NetworkProviderExtension>,
    program: &Path,
    args: &[String],
    home: &Path,
) -> Result<WrappedCommand> {
    let sandbox = &profile.config;
    let self_exe = std::env::current_exe().map_err(|source| StackError::SandboxFailed {
        reason: format!("cannot resolve the acps executable for the sandbox helper: {source}"),
    })?;
    let Some(network) = network else {
        if sandbox.require_network_provider {
            return Err(StackError::NetworkProviderRequired);
        }
        // Host networking: the pre-network wrapper, byte for byte.
        return Ok(WrappedCommand {
            program: resolve_bin("unshare"),
            args: unshare_chain_args(profile, program, args, home, &self_exe, false),
        });
    };
    let mut out: Vec<String> = vec![
        SANDBOX_SUPERVISE_SUBCOMMAND.to_owned(),
        "--diag-fd".to_owned(),
        SANDBOX_DIAG_FD.to_string(),
    ];
    out.extend(network.supervise_argv_fragment());
    out.push("--".to_owned());
    out.push(resolve_bin("unshare").to_string_lossy().into_owned());
    out.extend(unshare_chain_args(
        profile, program, args, home, &self_exe, true,
    ));
    Ok(WrappedCommand {
        program: self_exe,
        args: out,
    })
}

/// The argv passed to `unshare`: namespace flags, masking helper, privilege-drop chain, workload.
/// `--sync-fd` is absent here and injected by the supervisor at runtime, because the fd number does not exist yet.
fn unshare_chain_args(
    profile: &SandboxProfile,
    program: &Path,
    args: &[String],
    home: &Path,
    self_exe: &Path,
    isolated_network: bool,
) -> Vec<String> {
    let sandbox = &profile.config;
    let (uid, gid) = profile.drop_ids();
    let mut out: Vec<String> = Vec::new();
    if isolated_network {
        out.push("--net".to_owned());
    }
    out.extend(UNSHARE_FLAGS.iter().map(|s| s.to_string()));
    out.push("--".to_owned());
    // Masking must run inside the namespaces while caps are still held, i.e. before the setpriv drop below.
    out.push(self_exe.to_string_lossy().into_owned());
    out.push(SANDBOX_EXEC_SUBCOMMAND.to_owned());
    for path in sensitive_mask_paths(home, sandbox) {
        out.push("--mask".to_owned());
        out.push(path.to_string_lossy().into_owned());
    }
    for path in &sandbox.mask_files {
        out.push("--mask-file".to_owned());
        out.push(path.clone());
    }
    out.push("--".to_owned());
    out.extend(setpriv_drop_argv(uid, gid, profile.identity.is_some()));
    out.push(program.to_string_lossy().into_owned());
    out.extend(args.iter().cloned());
    out
}

fn wrap_bwrap(
    sandbox: &SandboxConfig,
    program: &Path,
    args: &[String],
    home: &Path,
    workspace_root: &Path,
) -> WrappedCommand {
    let mut out: Vec<String> = BWRAP_BASE_FLAGS.iter().map(|s| s.to_string()).collect();
    for path in sensitive_mask_paths(home, sandbox) {
        out.push("--tmpfs".to_owned());
        out.push(path.to_string_lossy().into_owned());
    }
    out.push("--bind".to_owned());
    out.push(workspace_root.to_string_lossy().into_owned());
    out.push(workspace_root.to_string_lossy().into_owned());
    for allow in &sandbox.allow_paths {
        out.push("--bind".to_owned());
        out.push(allow.clone());
        out.push(allow.clone());
    }
    out.push("--".to_owned());
    out.push(program.to_string_lossy().into_owned());
    out.extend(args.iter().cloned());
    WrappedCommand {
        program: resolve_bin("bwrap"),
        args: out,
    }
}

fn wrap_custom(sandbox: &SandboxConfig, program: &Path, args: &[String]) -> Result<WrappedCommand> {
    let (wrapper_program, wrapper_rest) =
        sandbox
            .wrapper
            .split_first()
            .ok_or_else(|| StackError::SandboxFailed {
                reason: "[workspace.sandbox] mode = \"custom\" requires a non-empty `wrapper` argv"
                    .to_owned(),
            })?;
    let mut out: Vec<String> = wrapper_rest.to_vec();
    out.push(program.to_string_lossy().into_owned());
    out.extend(args.iter().cloned());
    Ok(WrappedCommand {
        program: PathBuf::from(wrapper_program),
        args: out,
    })
}

/// Installs the daemon's stderr at [`SANDBOX_DIAG_FD`] in a `__sandbox-supervise` child.
/// The sandbox-config gate keeps a workload whose own argv merely starts with the subcommand token from ever receiving the daemon's stderr.
/// The returned handle MUST stay open across the spawn.
#[cfg(unix)]
pub fn wire_supervise_diag_fd(
    sandbox: &SandboxConfig,
    network: Option<&NetworkProviderExtension>,
    command: &mut tokio::process::Command,
    args: &[String],
) -> std::io::Result<Option<std::os::fd::OwnedFd>> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    if sandbox.mode != SandboxMode::Unshare || network.is_none() {
        return Ok(None);
    }
    if args.first().map(String::as_str) != Some(SANDBOX_SUPERVISE_SUBCOMMAND) {
        return Ok(None);
    }
    // SAFETY: duplicating our own stderr; the result is immediately owned. The minimum-fd floor keeps the dup off
    // SANDBOX_DIAG_FD itself, where dup2(fd, fd) would no-op and leave close-on-exec set, closing the fd at exec.
    let raw = unsafe {
        libc::fcntl(
            libc::STDERR_FILENO,
            libc::F_DUPFD_CLOEXEC,
            SANDBOX_DIAG_FD + 1,
        )
    };
    if raw < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: raw is a fresh fd owned solely by this handle.
    let stderr_dup = unsafe { OwnedFd::from_raw_fd(raw) };
    let dup_fd = stderr_dup.as_raw_fd();
    // SAFETY: dup2 is async-signal-safe; dup_fd outlives the spawn because the caller holds the handle across it.
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(dup_fd, SANDBOX_DIAG_FD) == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    Ok(Some(stderr_dup))
}

/// First existing `<dir>/<name>` among the standard bin dirs or PATH.
fn find_bin(name: &str) -> Option<PathBuf> {
    for dir in STANDARD_BIN_DIRS {
        let candidate = Path::new(dir).join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// [`find_bin`], falling back to the bare `name` so exec-time PATH resolution still applies.
fn resolve_bin(name: &str) -> PathBuf {
    find_bin(name).unwrap_or_else(|| PathBuf::from(name))
}

/// Whether the configured backend and workload identity can run on this host; `serve` startup is fail-closed on `Err(reason)`.
pub fn preflight(
    profile: &SandboxProfile,
    network: Option<&NetworkProviderExtension>,
) -> std::result::Result<(), String> {
    let sandbox = &profile.config;
    if profile.identity.is_some() {
        preflight_identity(sandbox.mode)?;
    }
    if cgroup::uses_workload_cgroups(profile) {
        cgroup::preflight()?;
    }
    match sandbox.mode {
        SandboxMode::Off => Ok(()),
        SandboxMode::Unshare => {
            require_bin("unshare")?;
            require_bin("setpriv")?;
            if !host_has_cap_sys_admin() {
                return Err(
                    "mode \"unshare\" requires CAP_SYS_ADMIN; run the daemon in a privileged \
                     container or choose another sandbox mode"
                        .to_owned(),
                );
            }
            if let Some(network) = network {
                supervise::preflight_pidfd_support()?;
                // Nothing else is required for isolated networking: `--net` is covered by CAP_SYS_ADMIN,
                // and tools like `ip`/`nsenter` are the provider's own dependencies.
                if let Some(provider) = network.provider.first()
                    && !Path::new(provider).is_file()
                {
                    return Err(format!(
                        "network-provider extension `{}` executable `{provider}` was not found",
                        network.name
                    ));
                }
            }
            Ok(())
        }
        SandboxMode::Bwrap => {
            require_bin("bwrap")?;
            Ok(())
        }
        SandboxMode::Custom => {
            let program = sandbox.wrapper.first().ok_or_else(|| {
                "mode \"custom\" requires a non-empty [workspace.sandbox].wrapper".to_owned()
            })?;
            let found = if Path::new(program).is_absolute() {
                Path::new(program).is_file()
            } else {
                find_bin(program).is_some()
            };
            if !found {
                return Err(format!("custom sandbox wrapper `{program}` was not found"));
            }
            Ok(())
        }
    }
}

/// The runtime runs the network provider before every isolated spawn, so with a workload identity
/// its executable chain, and every existing absolute path in its arguments (an interpreted
/// provider's script), must be out of the identity's reach.
pub fn verify_provider_executable(
    profile: &SandboxProfile,
    network: Option<&NetworkProviderExtension>,
) -> Result<()> {
    if profile.identity.is_none() {
        return Ok(());
    }
    let Some(network) = network else {
        return Ok(());
    };
    let Some((executable, arguments)) = network.provider.split_first() else {
        return Ok(());
    };
    let executor = profile.executor();
    let vetted_arguments = arguments
        .iter()
        .filter(|argument| Path::new(argument).is_absolute());
    for (index, path) in std::iter::once(executable)
        .chain(vetted_arguments)
        .enumerate()
    {
        // An argument may name a path that does not exist yet, which the identity must not be
        // able to create either.
        let writable = if index == 0 {
            crate::workload_fs::exec_chain_writable_component(&executor, Path::new(path))?
        } else {
            crate::workload_fs::future_path_writable_component(&executor, Path::new(path))?
        };
        if let Some(writable) = writable {
            let subject = if index == 0 {
                format!("network provider `{path}`")
            } else {
                format!("network provider `{executable}` argument `{path}`")
            };
            return Err(StackError::WorkloadWritableExecutable {
                subject,
                path: writable,
            });
        }
    }
    Ok(())
}

/// Re-uid'ing the workload needs `setpriv`, the capabilities its drop chain uses, and under
/// `unshare` a `setpriv` that can re-arm the parent-death signal.
fn preflight_identity(mode: SandboxMode) -> std::result::Result<(), String> {
    let setpriv = find_bin("setpriv").ok_or_else(|| {
        "a workload identity requires `setpriv`, not found in standard bin dirs or PATH".to_owned()
    })?;
    crate::workload_fs::preflight_access_checks()?;
    let missing = missing_identity_capabilities();
    if !missing.is_empty() {
        return Err(format!(
            "a workload identity requires {} in the runtime's permitted and inheritable sets; \
             grant them as ambient capabilities or remove [workspace.sandbox].workload_user",
            missing.join(", ")
        ));
    }
    if mode == SandboxMode::Unshare && !setpriv_supports_parent_death_signal(&setpriv) {
        return Err(format!(
            "`{}` does not support --pdeathsig (util-linux 2.33 or newer is required)",
            setpriv.display()
        ));
    }
    Ok(())
}

fn setpriv_supports_parent_death_signal(setpriv: &Path) -> bool {
    match Command::new(setpriv).arg("--help").output() {
        Ok(output) => {
            String::from_utf8_lossy(&output.stdout).contains(SETPRIV_PARENT_DEATH_FLAGS[0])
        }
        Err(error) => {
            tracing::warn!(%error, setpriv = %setpriv.display(), "probing setpriv --help failed");
            false
        }
    }
}

/// The drop runs in an exec'd `setpriv`, which a non-root runtime only hands capabilities to by
/// raising them into the ambient set, and that needs each one permitted and inheritable.
#[cfg(target_os = "linux")]
fn missing_identity_capabilities() -> Vec<&'static str> {
    let held = if crate::ownership::process_euid() == 0 {
        host_capabilities().effective
    } else {
        raisable_capabilities()
    };
    IDENTITY_CAPABILITIES
        .into_iter()
        .filter(|(capability, _)| !held.contains(*capability))
        .map(|(_, name)| name)
        .collect()
}

#[cfg(not(target_os = "linux"))]
fn missing_identity_capabilities() -> Vec<&'static str> {
    vec!["CAP_SETUID", "CAP_SETGID", "CAP_SETPCAP (Linux only)"]
}

/// Whether this host could run the `unshare` backend (binaries present and `CAP_SYS_ADMIN` held).
pub fn host_supports_unshare() -> bool {
    find_bin("unshare").is_some() && find_bin("setpriv").is_some() && host_has_cap_sys_admin()
}

fn require_bin(name: &str) -> std::result::Result<(), String> {
    if find_bin(name).is_some() {
        Ok(())
    } else {
        Err(format!(
            "sandbox backend requires `{name}`, not found in standard bin dirs or PATH"
        ))
    }
}

#[cfg(target_os = "linux")]
fn host_has_cap_sys_admin() -> bool {
    host_capabilities()
        .effective
        .contains(CapabilitySet::SYS_ADMIN)
}

/// The calling thread's capability sets, which every runtime thread shares; unreadable sets read
/// as empty.
#[cfg(target_os = "linux")]
fn host_capabilities() -> CapabilitySets {
    rustix::thread::capabilities(None).unwrap_or_else(|error| {
        tracing::warn!(%error, "reading the capability sets failed");
        CapabilitySets {
            effective: CapabilitySet::empty(),
            permitted: CapabilitySet::empty(),
            inheritable: CapabilitySet::empty(),
        }
    })
}

#[cfg(not(target_os = "linux"))]
fn host_has_cap_sys_admin() -> bool {
    false
}

/// Clear the ambient capability set before any thread exists. Ambient capabilities survive exec,
/// so otherwise every installer, probe and script the runtime runs would inherit what a non-root
/// runtime holds for its sandbox; [`prepare_workload_spawn`] hands them to the wrapped spawns
/// only. The `__sandbox-*` helpers are that wrapped chain and keep theirs.
pub fn clear_ambient_capabilities_at_startup() -> Result<()> {
    let subcommand = std::env::args_os().nth(1);
    if subcommand.is_some_and(|name| {
        SANDBOX_HELPER_SUBCOMMANDS
            .iter()
            .any(|helper| name == *helper)
    }) {
        return Ok(());
    }
    clear_ambient_capabilities()
}

#[cfg(target_os = "linux")]
fn clear_ambient_capabilities() -> Result<()> {
    match rustix::thread::clear_ambient_capability_set() {
        // A kernel without ambient capabilities has none to clear.
        Ok(()) | Err(rustix::io::Errno::INVAL) => Ok(()),
        Err(source) => Err(StackError::SandboxFailed {
            reason: format!("clearing the ambient capability set failed: {source}"),
        }),
    }
}

#[cfg(not(target_os = "linux"))]
fn clear_ambient_capabilities() -> Result<()> {
    Ok(())
}

/// Ready a wrapped workload spawn for exec. Every mode but an `off` passthrough gets the
/// capabilities cleared at startup back, because its wrapper chain needs them and drops them
/// before the workload runs; `off` with a workload identity also joins a fresh cgroup. The
/// returned guard must stay with the child.
pub fn prepare_workload_spawn(
    profile: &SandboxProfile,
    command: &mut tokio::process::Command,
) -> std::io::Result<Option<WorkloadCgroup>> {
    if profile.config.mode != SandboxMode::Off || profile.identity.is_some() {
        raise_ambient_capabilities(command);
    }
    let cgroup = WorkloadCgroup::for_profile(profile)?;
    #[cfg(unix)]
    if let Some(cgroup) = &cgroup {
        cgroup.enter_before_exec(command);
    }
    Ok(cgroup)
}

/// Every capability the runtime may hand to a child through the ambient set.
#[cfg(target_os = "linux")]
fn raisable_capabilities() -> CapabilitySet {
    let held = host_capabilities();
    held.permitted & held.inheritable
}

#[cfg(target_os = "linux")]
fn raise_ambient_capabilities(command: &mut tokio::process::Command) {
    let raisable = raisable_capabilities().bits();
    if raisable == 0 {
        return;
    }
    // SAFETY: the prctl raises are raw syscalls, async-signal-safe, and change only the forked
    // child's ambient set.
    unsafe {
        command.pre_exec(move || {
            // One bit at a time: kernels may report capabilities rustix has no name for.
            for bit in (0..u64::BITS).filter(|bit| (raisable >> bit) & 1 == 1) {
                rustix::thread::configure_capability_in_ambient_set(
                    CapabilitySet::from_bits_retain(1 << bit),
                    true,
                )?;
            }
            Ok(())
        });
    }
}

#[cfg(not(target_os = "linux"))]
fn raise_ambient_capabilities(_command: &mut tokio::process::Command) {}

/// `acps __sandbox-exec --mask <dir>… --mask-file <path>… -- <cmd> <args…>`: masks each directory with a fresh `tmpfs` and each non-directory path with an empty read-only file inside the `unshare` namespaces, then execs the privilege-drop chain. Never returns on success.
pub fn run_exec(raw_args: Vec<String>) -> Result<()> {
    let mut masks: Vec<String> = Vec::new();
    let mut mask_files: Vec<String> = Vec::new();
    let mut sync_fd: Option<i32> = None;
    let mut command: Vec<String> = Vec::new();
    let mut iter = raw_args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--mask" => {
                let value = iter.next().ok_or_else(|| StackError::SandboxFailed {
                    reason: "--mask requires a path argument".to_owned(),
                })?;
                masks.push(value);
            }
            "--mask-file" => {
                let value = iter.next().ok_or_else(|| StackError::SandboxFailed {
                    reason: "--mask-file requires a path argument".to_owned(),
                })?;
                mask_files.push(value);
            }
            "--sync-fd" => {
                let value = iter.next().ok_or_else(|| StackError::SandboxFailed {
                    reason: "--sync-fd requires an fd number".to_owned(),
                })?;
                let fd = value
                    .parse::<i32>()
                    .map_err(|_| StackError::SandboxFailed {
                        reason: format!("--sync-fd expects an fd number, got `{value}`"),
                    })?;
                sync_fd = Some(fd);
            }
            "--" => {
                command = iter.collect();
                break;
            }
            other => {
                return Err(StackError::SandboxFailed {
                    reason: format!("unexpected sandbox-exec argument `{other}`"),
                });
            }
        }
    }
    if command.is_empty() {
        return Err(StackError::SandboxFailed {
            reason: "sandbox-exec requires a command after `--`".to_owned(),
        });
    }
    for path in &masks {
        mask_with_tmpfs(Path::new(path))?;
    }
    for path in &mask_files {
        mask_with_empty_file(Path::new(path))?;
    }
    // Fail-closed gate: block until the supervisor confirms provider setup, so the workload never runs
    // with a half-configured namespace. A dead supervisor means EOF here and no exec at all.
    if let Some(fd) = sync_fd {
        supervise::wait_for_release(fd)?;
    }
    let error = Command::new(&command[0]).args(&command[1..]).exec();
    Err(StackError::SandboxFailed {
        reason: format!("exec `{}` failed: {error}", command[0]),
    })
}

/// Mount a fresh empty `tmpfs` over `path`. A missing path is skipped; any other failure is fatal rather than run the workload unmasked.
#[cfg(target_os = "linux")]
fn mask_with_tmpfs(path: &Path) -> Result<()> {
    if !path.exists() {
        eprintln!(
            "acps sandbox: mask path {} does not exist; skipping",
            path.display()
        );
        return Ok(());
    }
    let target =
        CString::new(path.as_os_str().as_bytes()).map_err(|_| StackError::SandboxFailed {
            reason: format!("mask path {} contains a NUL byte", path.display()),
        })?;
    let fstype = CString::new("tmpfs").expect("static string has no NUL");
    // SAFETY: all pointers are valid C strings for the duration of the call; a null `data` is valid for tmpfs.
    let rc = unsafe {
        libc::mount(
            fstype.as_ptr(),
            target.as_ptr(),
            fstype.as_ptr(),
            0,
            std::ptr::null(),
        )
    };
    if rc != 0 {
        let errno = std::io::Error::last_os_error();
        return Err(StackError::SandboxFailed {
            reason: format!("mask {} with tmpfs failed: {errno}", path.display()),
        });
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn mask_with_tmpfs(_path: &Path) -> Result<()> {
    Err(StackError::SandboxFailed {
        reason: "tmpfs masking is only supported on Linux".to_owned(),
    })
}

/// Bind an empty read-only regular file over `path`. A missing path is skipped; any other failure is fatal rather than run the workload unmasked.
/// The empty file is staged on a private tmpfs that is detached again once the bind holds it, so the helper leaves no file behind for the workload to find.
#[cfg(target_os = "linux")]
fn mask_with_empty_file(path: &Path) -> Result<()> {
    if !path.exists() {
        eprintln!(
            "acps sandbox: mask file {} does not exist; skipping",
            path.display()
        );
        return Ok(());
    }
    if path.is_dir() {
        return Err(StackError::SandboxFailed {
            reason: format!(
                "mask file {} is a directory; declare it under [workspace.sandbox].mask_paths",
                path.display()
            ),
        });
    }
    let staging = stage_mask_file_dir()?;
    let staging_target = match mount_mask_file_staging(&staging) {
        Ok(target) => target,
        Err(error) => {
            // Nothing is mounted yet, so the empty mkdtemp directory is all there is to undo.
            if let Err(remove_error) = std::fs::remove_dir(&staging) {
                eprintln!(
                    "acps sandbox: remove mask-file staging dir {} failed: {remove_error}",
                    staging.display()
                );
            }
            return Err(error);
        }
    };
    let bound = bind_empty_file_over(&staging, path);
    // The bind keeps the tmpfs alive, so detaching the staging mount and removing its
    // now-empty host directory cannot take the mask away. A bind failure outranks a
    // cleanup failure, so the operator sees the reason the mask did not hold.
    match (bound, unstage_empty_file(&staging, &staging_target)) {
        (Ok(()), unstaged) => unstaged,
        (Err(bind_error), Ok(())) => Err(bind_error),
        (Err(bind_error), Err(unstage_error)) => {
            eprintln!("acps sandbox: {unstage_error}");
            Err(bind_error)
        }
    }
}

/// Mount the private staging tmpfs and hand back the C path the teardown needs.
#[cfg(target_os = "linux")]
fn mount_mask_file_staging(staging: &Path) -> Result<CString> {
    let staging_target = c_path(staging)?;
    let fstype = CString::new("tmpfs").expect("static string has no NUL");
    // SAFETY: all pointers are valid C strings for the duration of the call; a null `data` is valid for tmpfs.
    let rc = unsafe {
        libc::mount(
            fstype.as_ptr(),
            staging_target.as_ptr(),
            fstype.as_ptr(),
            0,
            std::ptr::null(),
        )
    };
    if rc != 0 {
        let errno = std::io::Error::last_os_error();
        return Err(StackError::SandboxFailed {
            reason: format!(
                "mount mask-file staging tmpfs at {} failed: {errno}",
                staging.display()
            ),
        });
    }
    Ok(staging_target)
}

/// A staging directory this spawn alone owns. The mount namespace is private but the
/// directory entry is not, so a fixed path would let concurrent sandboxed spawns tear
/// down each other's in-flight staging.
#[cfg(target_os = "linux")]
fn stage_mask_file_dir() -> Result<PathBuf> {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let template = std::env::temp_dir().join(MASK_FILE_STAGING_TEMPLATE);
    let mut raw = template.into_os_string().into_vec();
    if raw.contains(&0) {
        return Err(StackError::SandboxFailed {
            reason: "mask-file staging template contains a NUL byte".to_owned(),
        });
    }
    raw.push(0);
    // SAFETY: `raw` is a writable, NUL-terminated buffer ending in the six template
    // characters mkdtemp replaces in place; it outlives the call.
    let created = unsafe { libc::mkdtemp(raw.as_mut_ptr().cast::<libc::c_char>()) };
    if created.is_null() {
        let errno = std::io::Error::last_os_error();
        return Err(StackError::SandboxFailed {
            reason: format!("create mask-file staging dir failed: {errno}"),
        });
    }
    raw.pop();
    Ok(PathBuf::from(OsString::from_vec(raw)))
}

#[cfg(target_os = "linux")]
fn unstage_empty_file(staging: &Path, staging_target: &CString) -> Result<()> {
    // SAFETY: the pointer is a valid C string for the duration of the call.
    let rc = unsafe { libc::umount2(staging_target.as_ptr(), libc::MNT_DETACH) };
    if rc != 0 {
        let errno = std::io::Error::last_os_error();
        return Err(StackError::SandboxFailed {
            reason: format!(
                "detach mask-file staging tmpfs at {} failed: {errno}",
                staging.display()
            ),
        });
    }
    std::fs::remove_dir(staging).map_err(|source| StackError::SandboxFailed {
        reason: format!(
            "remove mask-file staging dir {} failed: {source}",
            staging.display()
        ),
    })
}

/// Create the empty file on the staged tmpfs and bind it read-only over `target`.
#[cfg(target_os = "linux")]
fn bind_empty_file_over(staging: &Path, target: &Path) -> Result<()> {
    let source = staging.join(MASK_FILE_STAGING_NAME);
    std::fs::File::create(&source).map_err(|error| StackError::SandboxFailed {
        reason: format!("create mask file {} failed: {error}", source.display()),
    })?;
    let source_c = c_path(&source)?;
    let target_c = c_path(target)?;
    // SAFETY: both pointers are valid C strings for the duration of the call; MS_BIND ignores `fstype` and `data`.
    let rc = unsafe {
        libc::mount(
            source_c.as_ptr(),
            target_c.as_ptr(),
            std::ptr::null(),
            libc::MS_BIND,
            std::ptr::null(),
        )
    };
    if rc != 0 {
        let errno = std::io::Error::last_os_error();
        return Err(StackError::SandboxFailed {
            reason: format!(
                "mask {} with an empty file failed: {errno}",
                target.display()
            ),
        });
    }
    // SAFETY: the target pointer stays valid; a bind remount takes its flags from `flags` alone.
    let rc = unsafe {
        libc::mount(
            std::ptr::null(),
            target_c.as_ptr(),
            std::ptr::null(),
            libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY,
            std::ptr::null(),
        )
    };
    if rc != 0 {
        let errno = std::io::Error::last_os_error();
        return Err(StackError::SandboxFailed {
            reason: format!(
                "remount mask of {} read-only failed: {errno}",
                target.display()
            ),
        });
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn c_path(path: &Path) -> Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| StackError::SandboxFailed {
        reason: format!("sandbox path {} contains a NUL byte", path.display()),
    })
}

#[cfg(not(target_os = "linux"))]
fn mask_with_empty_file(_path: &Path) -> Result<()> {
    Err(StackError::SandboxFailed {
        reason: "empty-file masking is only supported on Linux".to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const WORKLOAD_UID: u32 = 2001;
    const WORKLOAD_GID: u32 = 2002;

    fn cfg(mode: SandboxMode) -> SandboxConfig {
        SandboxConfig {
            mode,
            ..Default::default()
        }
    }

    fn of(sandbox: &SandboxConfig) -> SandboxProfile {
        SandboxProfile {
            config: sandbox.clone(),
            identity: None,
        }
    }

    fn profile(mode: SandboxMode) -> SandboxProfile {
        of(&cfg(mode))
    }

    fn identity_profile(mode: SandboxMode) -> SandboxProfile {
        SandboxProfile {
            config: SandboxConfig {
                workload_user: Some("agent".to_owned()),
                ..cfg(mode)
            },
            identity: Some(WorkloadIdentity {
                name: "agent".to_owned(),
                uid: WORKLOAD_UID,
                gid: WORKLOAD_GID,
                home: PathBuf::from("/home/agent"),
            }),
        }
    }

    fn runtime_reuid() -> String {
        format!("--reuid={}", crate::ownership::process_euid())
    }

    fn network_extension(
        provider: Vec<String>,
        provider_timeout: Option<&str>,
    ) -> NetworkProviderExtension {
        NetworkProviderExtension {
            name: "egress".to_owned(),
            provider,
            provider_timeout: provider_timeout.map(str::to_owned),
            provider_stderr: crate::config::SandboxProviderStderr::default(),
            workload_env: std::collections::BTreeMap::new(),
        }
    }

    fn run(c: &WrappedCommand) -> String {
        let mut parts = vec![c.program.to_string_lossy().into_owned()];
        parts.extend(c.args.clone());
        parts.join(" ")
    }

    #[test]
    fn off_is_passthrough() {
        let w = wrap(
            &profile(SandboxMode::Off),
            None,
            Path::new("/home/u/.local/bin/claude"),
            &["acp".to_owned()],
            Path::new("/home/u"),
            Path::new("/home/u/ws"),
        )
        .unwrap();
        assert_eq!(w.program, PathBuf::from("/home/u/.local/bin/claude"));
        assert_eq!(w.args, vec!["acp".to_owned()]);
    }

    #[test]
    fn unshare_masks_sensitive_dirs_and_drops_privs() {
        let w = wrap(
            &profile(SandboxMode::Unshare),
            None,
            Path::new("/home/u/.local/bin/claude"),
            &["acp".to_owned()],
            Path::new("/home/u"),
            Path::new("/home/u/ws"),
        )
        .unwrap();
        let line = run(&w);
        assert!(w.program.ends_with("unshare"));
        assert!(line.contains("--mount-proc"));
        assert!(line.contains(SANDBOX_EXEC_SUBCOMMAND));
        assert!(line.contains("--mask /home/u/.config/acp-stack"));
        assert!(line.contains("--mask /home/u/.local/share/acp-stack"));
        assert!(line.contains(&runtime_reuid()));
        assert!(line.contains("--no-new-privs"));
        assert!(!line.contains("--pdeathsig"));
        assert!(line.trim_end().ends_with("/home/u/.local/bin/claude acp"));
    }

    #[test]
    fn unshare_with_identity_drops_to_the_workload_and_rearms_parent_death() {
        let w = wrap(
            &identity_profile(SandboxMode::Unshare),
            None,
            Path::new("/home/u/.local/bin/claude"),
            &["acp".to_owned()],
            Path::new("/home/u"),
            Path::new("/home/u/ws"),
        )
        .unwrap();
        let line = run(&w);
        assert!(w.program.ends_with("unshare"));
        assert!(line.contains(&format!("--reuid={WORKLOAD_UID} --regid={WORKLOAD_GID}")));
        // Masks derive from the runtime home, never the workload home.
        assert!(line.contains("--mask /home/u/.config/acp-stack"));
        assert!(!line.contains("/home/agent"));
        assert!(line.contains("--no-new-privs --pdeathsig KILL -- /home/u/.local/bin/claude acp"));
    }

    #[test]
    fn off_with_identity_drops_privileges_without_namespaces() {
        let w = wrap(
            &identity_profile(SandboxMode::Off),
            None,
            Path::new("/home/u/.local/bin/claude"),
            &["acp".to_owned()],
            Path::new("/home/u"),
            Path::new("/home/u/ws"),
        )
        .unwrap();
        assert_eq!(w.program, resolve_bin("setpriv"));
        let mut expected = vec![
            format!("--reuid={WORKLOAD_UID}"),
            format!("--regid={WORKLOAD_GID}"),
        ];
        expected.extend(SETPRIV_DROP_FLAGS.iter().map(|s| s.to_string()));
        expected.extend(["--", "/home/u/.local/bin/claude", "acp"].map(str::to_owned));
        assert_eq!(w.args, expected);
    }

    #[test]
    fn required_network_provider_refuses_a_host_network_spawn() {
        let mut sandbox = cfg(SandboxMode::Unshare);
        sandbox.require_network_provider = true;
        let error = wrap(
            &of(&sandbox),
            None,
            Path::new("/home/u/.local/bin/claude"),
            &["acp".to_owned()],
            Path::new("/home/u"),
            Path::new("/home/u/ws"),
        )
        .expect_err("no provider declared");
        assert_eq!(error.error_code(), "sandbox.network_provider_required");

        let network = network_extension(Vec::new(), None);
        let w = wrap(
            &of(&sandbox),
            Some(&network),
            Path::new("/home/u/.local/bin/claude"),
            &["acp".to_owned()],
            Path::new("/home/u"),
            Path::new("/home/u/ws"),
        )
        .expect("a declared provider satisfies the requirement");
        assert_eq!(w.args[0], SANDBOX_SUPERVISE_SUBCOMMAND);
    }

    #[test]
    fn unshare_masks_declared_files_after_the_directory_masks() {
        let mut sandbox = cfg(SandboxMode::Unshare);
        sandbox.mask_paths = vec!["/var/lib/network-egress".to_owned()];
        sandbox.mask_files = vec!["/run/host-control.sock".to_owned()];
        let w = wrap(
            &of(&sandbox),
            None,
            Path::new("/home/u/.local/bin/claude"),
            &["acp".to_owned()],
            Path::new("/home/u"),
            Path::new("/home/u/ws"),
        )
        .unwrap();
        let line = run(&w);
        assert!(
            line.contains("--mask /var/lib/network-egress --mask-file /run/host-control.sock --")
        );
        let mask_file_index = w
            .args
            .iter()
            .position(|arg| arg == "--mask-file")
            .expect("the file mask reaches the helper argv");
        let setpriv_index = w
            .args
            .iter()
            .position(|arg| *arg == runtime_reuid())
            .expect("the privilege drop follows");
        assert!(
            mask_file_index < setpriv_index,
            "masking must run while caps are still held"
        );
    }

    /// Config validation rejects `mask_files` under bwrap and custom, so a declaration
    /// that reaches `off` is the only non-unshare case the wrapper sees.
    #[test]
    fn off_mode_skips_mask_files() {
        let mut off = cfg(SandboxMode::Off);
        off.mask_files = vec!["/run/host-control.sock".to_owned()];
        let w = wrap(
            &of(&off),
            None,
            Path::new("/home/u/.local/bin/claude"),
            &["acp".to_owned()],
            Path::new("/home/u"),
            Path::new("/home/u/ws"),
        )
        .unwrap();
        assert!(!run(&w).contains("/run/host-control.sock"));
    }

    /// Two sandboxed spawns can stage a file mask at the same time, so a shared
    /// directory entry would let one spawn's teardown hit the other's staging.
    #[cfg(target_os = "linux")]
    #[test]
    fn mask_file_staging_dirs_are_unique_per_spawn() {
        let first = stage_mask_file_dir().expect("staging dir");
        let second = stage_mask_file_dir().expect("staging dir");
        assert_ne!(first, second);
        assert!(first.is_dir() && second.is_dir());
        std::fs::remove_dir(&first).expect("cleanup");
        std::fs::remove_dir(&second).expect("cleanup");
    }

    #[test]
    fn sandbox_exec_rejects_a_mask_file_without_a_path() {
        let err = run_exec(vec!["--mask-file".to_owned()]);
        assert!(err.is_err());
    }

    #[test]
    fn bwrap_masks_with_tmpfs_and_binds_workspace() {
        let w = wrap(
            &profile(SandboxMode::Bwrap),
            None,
            Path::new("/home/u/.local/bin/claude"),
            &["acp".to_owned()],
            Path::new("/home/u"),
            Path::new("/home/u/ws"),
        )
        .unwrap();
        let line = run(&w);
        assert!(w.program.ends_with("bwrap"));
        assert!(line.contains("--tmpfs /home/u/.config/acp-stack"));
        assert!(line.contains("--tmpfs /home/u/.local/share/acp-stack"));
        assert!(line.contains("--bind /home/u/ws /home/u/ws"));
        assert!(line.contains("--unshare-pid"));
    }

    #[test]
    fn custom_prepends_wrapper_and_requires_one() {
        let mut c = cfg(SandboxMode::Custom);
        c.wrapper = vec!["systemd-run".to_owned(), "--scope".to_owned()];
        let w = wrap(
            &of(&c),
            None,
            Path::new("/bin/claude"),
            &["acp".to_owned()],
            Path::new("/home/u"),
            Path::new("/home/u/ws"),
        )
        .unwrap();
        assert_eq!(w.program, PathBuf::from("systemd-run"));
        assert_eq!(w.args, vec!["--scope", "/bin/claude", "acp"]);

        let err = wrap(
            &profile(SandboxMode::Custom),
            None,
            Path::new("/bin/claude"),
            &[],
            Path::new("/home/u"),
            Path::new("/home/u/ws"),
        );
        assert!(err.is_err());
    }

    #[test]
    fn host_network_wrapper_is_byte_identical_to_legacy() {
        // Frozen argv: drift here is a regression for every existing unshare deployment.
        let sandbox = cfg(SandboxMode::Unshare);
        let w = wrap(
            &of(&sandbox),
            None,
            Path::new("/home/u/.local/bin/claude"),
            &["acp".to_owned()],
            Path::new("/home/u"),
            Path::new("/home/u/ws"),
        )
        .unwrap();
        let self_exe = std::env::current_exe().unwrap();
        let mut expected: Vec<String> = UNSHARE_FLAGS.iter().map(|s| s.to_string()).collect();
        expected.extend(
            [
                "--",
                &self_exe.to_string_lossy(),
                SANDBOX_EXEC_SUBCOMMAND,
                "--mask",
                "/home/u/.config/acp-stack",
                "--mask",
                "/home/u/.local/share/acp-stack",
                "--",
                &resolve_bin("setpriv").to_string_lossy(),
                &runtime_reuid(),
                &format!("--regid={}", crate::ownership::process_egid()),
            ]
            .map(str::to_owned),
        );
        expected.extend(SETPRIV_DROP_FLAGS.iter().map(|s| s.to_string()));
        expected.extend(["--", "/home/u/.local/bin/claude", "acp"].map(str::to_owned));
        assert_eq!(w.program, resolve_bin("unshare"));
        assert_eq!(w.args, expected);
        assert!(!run(&w).contains("--net"));
    }

    #[test]
    fn isolated_network_wraps_with_supervisor_and_net() {
        let sandbox = cfg(SandboxMode::Unshare);
        let network = network_extension(
            vec![
                "/usr/local/libexec/provider".to_owned(),
                "--config".to_owned(),
                "/etc/provider.toml".to_owned(),
            ],
            Some("45s"),
        );
        let w = wrap(
            &of(&sandbox),
            Some(&network),
            Path::new("/home/u/.local/bin/claude"),
            &["acp".to_owned()],
            Path::new("/home/u"),
            Path::new("/home/u/ws"),
        )
        .unwrap();
        let line = run(&w);
        assert_eq!(w.program, std::env::current_exe().unwrap());
        assert_eq!(w.args[0], SANDBOX_SUPERVISE_SUBCOMMAND);
        assert!(line.contains(&format!("--diag-fd {SANDBOX_DIAG_FD}")));
        assert!(line.contains("--provider-timeout 45s"));
        assert!(line.contains("--provider-stderr daemon"));
        assert!(line.contains("--provider-arg /usr/local/libexec/provider"));
        assert!(line.contains("--provider-arg --config"));
        assert!(line.contains("--provider-arg /etc/provider.toml"));
        assert!(line.contains("--net --mount"));
        assert!(line.contains(SANDBOX_EXEC_SUBCOMMAND));
        assert!(line.contains("--mask /home/u/.config/acp-stack"));
        assert!(line.contains(&runtime_reuid()));
        assert!(line.trim_end().ends_with("/home/u/.local/bin/claude acp"));
        // The sync fd is injected by the supervisor at runtime, never baked into the wrapper argv.
        assert!(!line.contains("--sync-fd"));
    }

    #[test]
    fn isolated_network_without_provider_is_deny_all() {
        let sandbox = cfg(SandboxMode::Unshare);
        let network = network_extension(Vec::new(), None);
        let w = wrap(
            &of(&sandbox),
            Some(&network),
            Path::new("/home/u/.local/bin/claude"),
            &["acp".to_owned()],
            Path::new("/home/u"),
            Path::new("/home/u/ws"),
        )
        .unwrap();
        let line = run(&w);
        assert_eq!(w.args[0], SANDBOX_SUPERVISE_SUBCOMMAND);
        assert!(line.contains("--net"));
        assert!(line.contains("--provider-timeout 30s"));
        assert!(!line.contains("--provider-arg"));
    }

    #[test]
    fn sandbox_exec_requires_command() {
        let err = run_exec(vec!["--mask".to_owned(), "/tmp/x".to_owned()]);
        assert!(err.is_err());
    }

    #[test]
    fn sandbox_exec_rejects_malformed_sync_fd() {
        let err = run_exec(vec![
            "--sync-fd".to_owned(),
            "not-a-number".to_owned(),
            "--".to_owned(),
            "/bin/true".to_owned(),
        ]);
        assert!(err.is_err());
    }

    #[test]
    fn preflight_off_is_ok_custom_requires_wrapper() {
        assert!(preflight(&profile(SandboxMode::Off), None).is_ok());
        assert!(preflight(&profile(SandboxMode::Custom), None).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires CAP_SETUID/CAP_SETGID and ACPS_TEST_WORKLOAD_USER"]
    fn provider_check_covers_absolute_script_arguments() {
        use std::os::unix::fs::PermissionsExt as _;

        let name = std::env::var("ACPS_TEST_WORKLOAD_USER").expect("ACPS_TEST_WORKLOAD_USER");
        let entry = crate::ownership::lookup_user(&name)
            .expect("passwd lookup")
            .expect("the workload user exists");
        let profile = SandboxProfile {
            identity: Some(WorkloadIdentity {
                name: name.clone(),
                uid: entry.uid,
                gid: entry.gid,
                home: entry.home,
            }),
            config: SandboxConfig {
                workload_user: Some(name),
                ..cfg(SandboxMode::Unshare)
            },
        };
        // The checkout's ancestors stand in for runtime-owned dirs; a temp dir's would not.
        let root = tempfile::Builder::new()
            .prefix(".acps-provider-")
            .tempdir_in(env!("CARGO_MANIFEST_DIR"))
            .expect("fixture root");
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o755))
            .expect("chmod root");
        let scripts = root.path().join("scripts");
        std::fs::create_dir(&scripts).expect("scripts dir");
        let script = scripts.join("setup-net.sh");
        std::fs::write(&script, b"#!/bin/sh\n").expect("script");
        let provider = vec![
            "/bin/sh".to_owned(),
            "--verbose".to_owned(),
            root.path().join("created-later.sock").display().to_string(),
            script.display().to_string(),
        ];
        let network = network_extension(provider, None);

        std::fs::set_permissions(&scripts, std::fs::Permissions::from_mode(0o777))
            .expect("chmod scripts");
        let error = verify_provider_executable(&profile, Some(&network))
            .expect_err("a workload-writable script argument is refused");
        assert_eq!(error.error_code(), "sandbox.workload_writable_executable");

        std::fs::set_permissions(&scripts, std::fs::Permissions::from_mode(0o755))
            .expect("chmod scripts");
        verify_provider_executable(&profile, Some(&network))
            .expect("flags are skipped; the script and the missing path are out of reach");

        // A missing argument still counts when the identity could create it.
        let drop_dir = root.path().join("drop");
        std::fs::create_dir(&drop_dir).expect("drop dir");
        std::fs::set_permissions(&drop_dir, std::fs::Permissions::from_mode(0o777))
            .expect("chmod drop");
        let pending = network_extension(
            vec![
                "/bin/sh".to_owned(),
                drop_dir.join("absent.sh").display().to_string(),
            ],
            None,
        );
        let error = verify_provider_executable(&profile, Some(&pending))
            .expect_err("a missing argument in a workload-writable dir is refused");
        assert_eq!(error.error_code(), "sandbox.workload_writable_executable");
    }
}
