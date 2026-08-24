//! Supervisor for network-isolated sandbox spawns (`acps __sandbox-supervise`): it
//! owns the per-spawn network namespace and runs the operator provider's
//! `setup`/`teardown` around the workload. Setup failure is fail-closed (the
//! workload never execs), and the provider runs with a cleared environment so agent
//! env and secrets never reach it.

use crate::error::{Result, StackError};

// CONSTANTS

/// Provider protocol version, exposed as `ACPS_SANDBOX_NETWORK_PROTOCOL`.
pub const NETWORK_PROTOCOL_VERSION: &str = "1";
pub const ENV_NETWORK_PROTOCOL: &str = "ACPS_SANDBOX_NETWORK_PROTOCOL";
pub const ENV_NETWORK_ID: &str = "ACPS_SANDBOX_NETWORK_ID";
pub const ENV_NETWORK_NAMESPACE: &str = "ACPS_SANDBOX_NETWORK_NAMESPACE";
pub const ENV_NETWORK_PID: &str = "ACPS_SANDBOX_NETWORK_PID";

/// Supervisor exit code when provider `setup` failed or timed out (the workload
/// was never executed).
pub const SETUP_FAILED_EXIT: i32 = 120;
/// Supervisor exit code when the workload succeeded but provider `teardown` failed.
pub const TEARDOWN_FAILED_EXIT: i32 = 121;

/// Sync handshake bytes: the in-namespace helper sends `READY` once masking is done,
/// the supervisor sends `RELEASE` once provider setup succeeded.
const READY_BYTE: u8 = b'R';
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const RELEASE_BYTE: u8 = b'G';

/// Signal readiness to the supervisor, then block until it releases the workload.
/// EOF means the supervisor died or provider setup failed: fail closed, never exec.
pub fn wait_for_release(fd: i32) -> Result<()> {
    write_byte(fd, READY_BYTE)?;
    match read_byte(fd)? {
        Some(_) => {
            set_cloexec(fd)?;
            Ok(())
        }
        None => Err(StackError::SandboxFailed {
            reason: "the network sandbox supervisor closed the sync channel before releasing \
                     the workload (provider setup failed or the supervisor died)"
                .to_owned(),
        }),
    }
}

fn write_byte(fd: i32, byte: u8) -> Result<()> {
    loop {
        // SAFETY: the buffer is a valid single byte for the duration of the call.
        let rc = unsafe { libc::write(fd, std::ptr::from_ref(&byte).cast(), 1) };
        if rc == 1 {
            return Ok(());
        }
        let errno = std::io::Error::last_os_error();
        if errno.kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        return Err(StackError::SandboxFailed {
            reason: format!("write to sandbox sync fd {fd} failed: {errno}"),
        });
    }
}

/// Blocking single-byte read; `Ok(None)` is EOF (peer closed without writing).
fn read_byte(fd: i32) -> Result<Option<u8>> {
    let mut byte = 0u8;
    loop {
        // SAFETY: the buffer is a valid single byte for the duration of the call.
        let rc = unsafe { libc::read(fd, std::ptr::from_mut(&mut byte).cast(), 1) };
        match rc {
            1 => return Ok(Some(byte)),
            0 => return Ok(None),
            _ => {
                let errno = std::io::Error::last_os_error();
                if errno.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(StackError::SandboxFailed {
                    reason: format!("read from sandbox sync fd {fd} failed: {errno}"),
                });
            }
        }
    }
}

fn set_cloexec(fd: i32) -> Result<()> {
    // SAFETY: fcntl on an owned fd with valid F_SETFD arguments.
    let rc = unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    if rc != 0 {
        let errno = std::io::Error::last_os_error();
        return Err(StackError::SandboxFailed {
            reason: format!("marking fd {fd} close-on-exec failed: {errno}"),
        });
    }
    Ok(())
}

/// Entry point for `acps __sandbox-supervise`. Terminates the process itself,
/// mirroring the workload's exit or signal, on every path after the chain spawns;
/// an `Err` means the supervisor could not start at all.
#[cfg(target_os = "linux")]
pub fn run_supervise(raw_args: Vec<String>) -> Result<()> {
    linux::run(raw_args)
}

#[cfg(not(target_os = "linux"))]
pub fn run_supervise(_raw_args: Vec<String>) -> Result<()> {
    Err(StackError::SandboxFailed {
        reason: "network isolation is only supported on Linux".to_owned(),
    })
}

/// Entry point for the provider process-group monitor: EOF on the liveness fd means
/// the sandbox supervisor died and the whole provider group must be killed at once.
#[cfg(target_os = "linux")]
pub fn run_provider_supervise(raw_args: Vec<String>) -> Result<()> {
    linux::run_provider_supervise(raw_args)
}

#[cfg(not(target_os = "linux"))]
pub fn run_provider_supervise(_raw_args: Vec<String>) -> Result<()> {
    Err(StackError::SandboxFailed {
        reason: "network isolation is only supported on Linux".to_owned(),
    })
}

#[cfg(target_os = "linux")]
pub(super) fn preflight_pidfd_support() -> std::result::Result<(), String> {
    linux::preflight_pidfd_support()
}

#[cfg(not(target_os = "linux"))]
pub(super) fn preflight_pidfd_support() -> std::result::Result<(), String> {
    Err("network-isolated sandboxing requires Linux pidfd support".to_owned())
}

#[cfg(target_os = "linux")]
mod linux;
