//! Isolation backends for the agent harness and mediated shells.
//!
//! The daemon holds the runtime's secrets (the age key, decrypted secret store),
//! its config, and its local control socket. When the workload is untrusted
//! (a prompt-injected or malicious agent), running it as the same process tree as
//! the daemon means any in-runtime policy is bypassable and the secrets are one
//! `cat` away. A sandbox backend wraps each spawn so the workload runs isolated.
//! The `unshare` and `bwrap` backends mask the daemon's sensitive paths and socket
//! directly; the `custom` backend delegates isolation to an operator-supplied
//! wrapper, so there the masking guarantee is the operator's responsibility.
//!
//! No single OS mechanism is portable, so the backend is selected by
//! `[workspace.sandbox].mode`:
//!
//! * `off` — no wrapping (single-process behavior, unchanged).
//! * `unshare` — new mount/pid/ipc/uts namespaces via `unshare(1)`, a fresh
//!   `/proc`, the sensitive paths masked with `tmpfs`, then all capabilities and
//!   `no_new_privs` dropped via `setpriv(1)` before exec. Requires the daemon to
//!   hold `CAP_SYS_ADMIN` (privileged container) — the masking `mount(2)` is done
//!   by the [`run_exec`] helper, which the wrapper re-invokes inside the namespaces.
//! * `bwrap` — `bubblewrap` with the same masking, for hosts with unprivileged
//!   user namespaces.
//! * `custom` — an operator-supplied wrapper argv, for any other environment
//!   (`systemd-run`, `firejail`, …).
//!
//! Regardless of backend, the set of the daemon's own sensitive paths is derived
//! here from the runtime path helpers (never from operator config), so an operator
//! cannot misconfigure away the protection.

#[cfg(target_os = "linux")]
use std::ffi::CString;
#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::{SandboxConfig, SandboxMode};
use crate::error::{Result, StackError};

// CONSTANTS

/// Internal subcommand the `unshare` wrapper re-invokes (`acps __sandbox-exec`).
/// Hidden from `--help`; it performs the in-namespace `tmpfs` masking that no
/// stock tool can do without euid 0, then execs the privilege-drop chain.
pub const SANDBOX_EXEC_SUBCOMMAND: &str = "__sandbox-exec";

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

/// `setpriv` flags that drop every capability set plus `no_new_privs` so a
/// setuid binary (e.g. `sudo`) inside the sandbox cannot regain privilege.
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

/// Capability bit for `CAP_SYS_ADMIN`, which the `unshare` backend needs to
/// create namespaces and mount a fresh `/proc` + the masking `tmpfs`.
#[cfg(target_os = "linux")]
const CAP_SYS_ADMIN_BIT: u32 = 21;

/// A spawn command after sandbox wrapping: the program to exec and its full argv.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrappedCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
}

/// The daemon's own paths that must be unreadable from inside the sandbox: the
/// config dir (config + `age.key`) and the state dir (secret store, state db, and
/// the local control socket). Operator `mask_paths` are appended.
pub fn sensitive_mask_paths(home: &Path, sandbox: &SandboxConfig) -> Vec<PathBuf> {
    let mut paths = vec![
        crate::secrets::config_dir(home),
        crate::secrets::state_dir(home),
    ];
    paths.extend(sandbox.mask_paths.iter().map(PathBuf::from));
    paths
}

/// Wrap `program`/`args` according to `sandbox`. With `mode = off` the command is
/// returned unchanged. The caller still sets cwd/env (secrets via `Command::env`,
/// which every backend forwards to the harness) and stdio.
pub fn wrap(
    sandbox: &SandboxConfig,
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
        SandboxMode::Unshare => wrap_unshare(sandbox, program, args, home, uid, gid),
        SandboxMode::Bwrap => Ok(wrap_bwrap(sandbox, program, args, home, workspace_root)),
        SandboxMode::Custom => wrap_custom(sandbox, program, args),
    }
}

fn wrap_unshare(
    sandbox: &SandboxConfig,
    program: &Path,
    args: &[String],
    home: &Path,
    uid: u32,
    gid: u32,
) -> Result<WrappedCommand> {
    let self_exe = std::env::current_exe().map_err(|source| StackError::SandboxFailed {
        reason: format!("cannot resolve the acps executable for the sandbox helper: {source}"),
    })?;
    let mut out: Vec<String> = UNSHARE_FLAGS.iter().map(|s| s.to_string()).collect();
    out.push("--".to_owned());
    // The masking helper runs inside the namespaces while still holding caps.
    out.push(self_exe.to_string_lossy().into_owned());
    out.push(SANDBOX_EXEC_SUBCOMMAND.to_owned());
    for path in sensitive_mask_paths(home, sandbox) {
        out.push("--mask".to_owned());
        out.push(path.to_string_lossy().into_owned());
    }
    out.push("--".to_owned());
    // Privilege drop, then the real harness.
    out.push(resolve_bin("setpriv").to_string_lossy().into_owned());
    out.push(format!("--reuid={uid}"));
    out.push(format!("--regid={gid}"));
    out.extend(SETPRIV_DROP_FLAGS.iter().map(|s| s.to_string()));
    out.push("--".to_owned());
    out.push(program.to_string_lossy().into_owned());
    out.extend(args.iter().cloned());
    Ok(WrappedCommand {
        program: resolve_bin("unshare"),
        args: out,
    })
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

/// [`find_bin`], falling back to the bare `name` (resolved against PATH at exec
/// time) so the harness command still works when the daemon's PATH is narrowed.
fn resolve_bin(name: &str) -> PathBuf {
    find_bin(name).unwrap_or_else(|| PathBuf::from(name))
}

/// Whether the configured backend can actually run on this host. `Ok(())` for
/// `off` and for a usable backend; `Err(reason)` names exactly what is missing.
/// Consumed by `serve` startup (fail-closed: the daemon refuses to start when a
/// configured backend is unusable) and the security self-check (which surfaces
/// the reason as a finding), instead of letting the first agent spawn fail
/// indirectly.
pub fn preflight(sandbox: &SandboxConfig) -> std::result::Result<(), String> {
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

/// Whether this host could run the `unshare` backend (binaries present and
/// `CAP_SYS_ADMIN` held). Used to nudge operators who left `mode = off` on a host
/// that is capable of sandboxing.
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
        if let Some(hex) = line.strip_prefix("CapEff:") {
            if let Ok(bits) = u64::from_str_radix(hex.trim(), 16) {
                return (bits >> CAP_SYS_ADMIN_BIT) & 1 == 1;
            }
        }
    }
    false
}

#[cfg(not(target_os = "linux"))]
fn host_has_cap_sys_admin() -> bool {
    false
}

/// `acps __sandbox-exec --mask <dir>… -- <cmd> <args…>`: runs inside the
/// namespaces created by `unshare`, still holding `CAP_SYS_ADMIN`. Masks each
/// `--mask` directory with a fresh `tmpfs` (a direct `mount(2)`, which needs the
/// capability but not euid 0), then execs the command — which is the `setpriv`
/// privilege-drop chain ending in the harness. Never returns on success.
pub fn run_exec(raw_args: Vec<String>) -> Result<()> {
    let mut masks: Vec<String> = Vec::new();
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
    // Replace this process with the privilege-drop chain; env is inherited.
    let error = Command::new(&command[0]).args(&command[1..]).exec();
    Err(StackError::SandboxFailed {
        reason: format!("exec `{}` failed: {error}", command[0]),
    })
}

/// Mount a fresh empty `tmpfs` over `path`, hiding its contents inside the mount
/// namespace. A missing path is nothing to protect, so it is skipped. Any other
/// failure is fatal — fail closed rather than run the workload unmasked.
#[cfg(target_os = "linux")]
fn mask_with_tmpfs(path: &Path) -> Result<()> {
    if !path.exists() {
        // Nothing to protect, but warn (to the agent's stderr / daemon log) so a
        // typo in an operator `mask_paths` entry is visible rather than silently
        // leaving that path unprotected.
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
    // SAFETY: all pointers are valid C strings for the duration of the call; a
    // null `data` is valid for tmpfs.
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

    fn run(c: &WrappedCommand) -> String {
        let mut parts = vec![c.program.to_string_lossy().into_owned()];
        parts.extend(c.args.clone());
        parts.join(" ")
    }

    #[test]
    fn off_is_passthrough() {
        let w = wrap(
            &cfg(SandboxMode::Off),
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
        // Both sensitive dirs are masked, derived not from config.
        assert!(line.contains("--mask /home/u/.config/acp-stack"));
        assert!(line.contains("--mask /home/u/.local/share/acp-stack"));
        // Privilege drop + the real harness at the end.
        assert!(line.contains("--reuid=1001"));
        assert!(line.contains("--no-new-privs"));
        assert!(line.trim_end().ends_with("/home/u/.local/bin/claude acp"));
    }

    #[test]
    fn bwrap_masks_with_tmpfs_and_binds_workspace() {
        let w = wrap(
            &cfg(SandboxMode::Bwrap),
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

        // Empty wrapper is rejected fail-fast.
        let err = wrap(
            &cfg(SandboxMode::Custom),
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
    fn sandbox_exec_requires_command() {
        let err = run_exec(vec!["--mask".to_owned(), "/tmp/x".to_owned()]);
        assert!(err.is_err());
    }

    #[test]
    fn preflight_off_is_ok_custom_requires_wrapper() {
        assert!(preflight(&cfg(SandboxMode::Off)).is_ok());
        // Custom with an empty wrapper is unusable regardless of host.
        assert!(preflight(&cfg(SandboxMode::Custom)).is_err());
    }
}
