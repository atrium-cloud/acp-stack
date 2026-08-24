//! Isolation backends (`off`/`unshare`/`bwrap`/`custom`) that wrap each agent
//! spawn so an untrusted workload cannot reach the daemon's secrets or socket.

#[cfg(target_os = "linux")]
use std::ffi::CString;
#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::{SandboxConfig, SandboxMode};
use crate::error::{Result, StackError};
use crate::extensions::NetworkProviderExtension;

pub mod supervise;

// CONSTANTS

/// Internal subcommand the `unshare` wrapper re-invokes (`acps __sandbox-exec`).
pub const SANDBOX_EXEC_SUBCOMMAND: &str = "__sandbox-exec";

/// Internal subcommand that supervises a network-isolated spawn (`acps __sandbox-supervise`).
pub const SANDBOX_SUPERVISE_SUBCOMMAND: &str = "__sandbox-supervise";

/// Internal subcommand that keeps a provider and its descendants in a liveness-monitored process group.
pub const SANDBOX_PROVIDER_SUPERVISE_SUBCOMMAND: &str = "__sandbox-provider-supervise";

/// Fixed child fd the spawn sites dup the daemon's stderr onto, so supervisor diagnostics reach the operator even when the workload's stderr is a captured pipe.
pub const SANDBOX_DIAG_FD: i32 = 3;

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

#[cfg(target_os = "linux")]
const CAP_SYS_ADMIN_BIT: u32 = 21;

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

/// Wrap `program`/`args` according to `sandbox`; a declared `network` extension (`unshare` only) also moves the spawn into an isolated network namespace.
#[allow(clippy::too_many_arguments)]
pub fn wrap(
    sandbox: &SandboxConfig,
    network: Option<&NetworkProviderExtension>,
    program: &Path,
    args: &[String],
    home: &Path,
    workspace_root: &Path,
    uid: u32,
    gid: u32,
) -> Result<WrappedCommand> {
    match sandbox.mode {
        SandboxMode::Off => Ok(WrappedCommand {
            program: program.to_path_buf(),
            args: args.to_vec(),
        }),
        SandboxMode::Unshare => wrap_unshare(sandbox, network, program, args, home, uid, gid),
        SandboxMode::Bwrap => Ok(wrap_bwrap(sandbox, program, args, home, workspace_root)),
        SandboxMode::Custom => wrap_custom(sandbox, program, args),
    }
}

fn wrap_unshare(
    sandbox: &SandboxConfig,
    network: Option<&NetworkProviderExtension>,
    program: &Path,
    args: &[String],
    home: &Path,
    uid: u32,
    gid: u32,
) -> Result<WrappedCommand> {
    let self_exe = std::env::current_exe().map_err(|source| StackError::SandboxFailed {
        reason: format!("cannot resolve the acps executable for the sandbox helper: {source}"),
    })?;
    let Some(network) = network else {
        // Host networking: the pre-network wrapper, byte for byte.
        return Ok(WrappedCommand {
            program: resolve_bin("unshare"),
            args: unshare_chain_args(sandbox, program, args, home, uid, gid, &self_exe, false),
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
        sandbox, program, args, home, uid, gid, &self_exe, true,
    ));
    Ok(WrappedCommand {
        program: self_exe,
        args: out,
    })
}

/// The argv passed to `unshare`: namespace flags, masking helper, privilege-drop chain, workload.
/// `--sync-fd` is absent here and injected by the supervisor at runtime, because the fd number does not exist yet.
#[allow(clippy::too_many_arguments)]
fn unshare_chain_args(
    sandbox: &SandboxConfig,
    program: &Path,
    args: &[String],
    home: &Path,
    uid: u32,
    gid: u32,
    self_exe: &Path,
    isolated_network: bool,
) -> Vec<String> {
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
    out.push("--".to_owned());
    out.push(resolve_bin("setpriv").to_string_lossy().into_owned());
    out.push(format!("--reuid={uid}"));
    out.push(format!("--regid={gid}"));
    out.extend(SETPRIV_DROP_FLAGS.iter().map(|s| s.to_string()));
    out.push("--".to_owned());
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

/// Whether the configured backend can run on this host; `serve` startup is fail-closed on `Err(reason)`.
pub fn preflight(
    sandbox: &SandboxConfig,
    network: Option<&NetworkProviderExtension>,
) -> std::result::Result<(), String> {
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
    let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
        return false;
    };
    for line in status.lines() {
        if let Some(hex) = line.strip_prefix("CapEff:")
            && let Ok(bits) = u64::from_str_radix(hex.trim(), 16)
        {
            return (bits >> CAP_SYS_ADMIN_BIT) & 1 == 1;
        }
    }
    false
}

#[cfg(not(target_os = "linux"))]
fn host_has_cap_sys_admin() -> bool {
    false
}

/// `acps __sandbox-exec --mask <dir>… -- <cmd> <args…>`: masks each directory with a fresh `tmpfs` inside the `unshare` namespaces, then execs the privilege-drop chain. Never returns on success.
pub fn run_exec(raw_args: Vec<String>) -> Result<()> {
    let mut masks: Vec<String> = Vec::new();
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

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(mode: SandboxMode) -> SandboxConfig {
        SandboxConfig {
            mode,
            ..Default::default()
        }
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
            &cfg(SandboxMode::Off),
            None,
            Path::new("/home/u/.local/bin/claude"),
            &["acp".to_owned()],
            Path::new("/home/u"),
            Path::new("/home/u/ws"),
            1001,
            1001,
        )
        .unwrap();
        assert_eq!(w.program, PathBuf::from("/home/u/.local/bin/claude"));
        assert_eq!(w.args, vec!["acp".to_owned()]);
    }

    #[test]
    fn unshare_masks_sensitive_dirs_and_drops_privs() {
        let w = wrap(
            &cfg(SandboxMode::Unshare),
            None,
            Path::new("/home/u/.local/bin/claude"),
            &["acp".to_owned()],
            Path::new("/home/u"),
            Path::new("/home/u/ws"),
            1001,
            1001,
        )
        .unwrap();
        let line = run(&w);
        assert!(w.program.ends_with("unshare"));
        assert!(line.contains("--mount-proc"));
        assert!(line.contains(SANDBOX_EXEC_SUBCOMMAND));
        assert!(line.contains("--mask /home/u/.config/acp-stack"));
        assert!(line.contains("--mask /home/u/.local/share/acp-stack"));
        assert!(line.contains("--reuid=1001"));
        assert!(line.contains("--no-new-privs"));
        assert!(line.trim_end().ends_with("/home/u/.local/bin/claude acp"));
    }

    #[test]
    fn bwrap_masks_with_tmpfs_and_binds_workspace() {
        let w = wrap(
            &cfg(SandboxMode::Bwrap),
            None,
            Path::new("/home/u/.local/bin/claude"),
            &["acp".to_owned()],
            Path::new("/home/u"),
            Path::new("/home/u/ws"),
            1001,
            1001,
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
            &c,
            None,
            Path::new("/bin/claude"),
            &["acp".to_owned()],
            Path::new("/home/u"),
            Path::new("/home/u/ws"),
            1001,
            1001,
        )
        .unwrap();
        assert_eq!(w.program, PathBuf::from("systemd-run"));
        assert_eq!(w.args, vec!["--scope", "/bin/claude", "acp"]);

        let err = wrap(
            &cfg(SandboxMode::Custom),
            None,
            Path::new("/bin/claude"),
            &[],
            Path::new("/home/u"),
            Path::new("/home/u/ws"),
            1001,
            1001,
        );
        assert!(err.is_err());
    }

    #[test]
    fn host_network_wrapper_is_byte_identical_to_legacy() {
        // Frozen argv: drift here is a regression for every existing unshare deployment.
        let sandbox = cfg(SandboxMode::Unshare);
        let w = wrap(
            &sandbox,
            None,
            Path::new("/home/u/.local/bin/claude"),
            &["acp".to_owned()],
            Path::new("/home/u"),
            Path::new("/home/u/ws"),
            1001,
            1001,
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
                "--reuid=1001",
                "--regid=1001",
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
            &sandbox,
            Some(&network),
            Path::new("/home/u/.local/bin/claude"),
            &["acp".to_owned()],
            Path::new("/home/u"),
            Path::new("/home/u/ws"),
            1001,
            1001,
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
        assert!(line.contains("--reuid=1001"));
        assert!(line.trim_end().ends_with("/home/u/.local/bin/claude acp"));
        // The sync fd is injected by the supervisor at runtime, never baked into the wrapper argv.
        assert!(!line.contains("--sync-fd"));
    }

    #[test]
    fn isolated_network_without_provider_is_deny_all() {
        let sandbox = cfg(SandboxMode::Unshare);
        let network = network_extension(Vec::new(), None);
        let w = wrap(
            &sandbox,
            Some(&network),
            Path::new("/home/u/.local/bin/claude"),
            &["acp".to_owned()],
            Path::new("/home/u"),
            Path::new("/home/u/ws"),
            1001,
            1001,
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
        assert!(preflight(&cfg(SandboxMode::Off), None).is_ok());
        assert!(preflight(&cfg(SandboxMode::Custom), None).is_err());
    }
}
