//! Per-spawn cgroup v2 containment for `off`-mode workloads that run under a workload
//! identity. Without a pid namespace, a runtime lacking CAP_KILL cannot signal a
//! different-uid process tree, and a descendant that left the process group escapes
//! group kills; writing `cgroup.kill` stops every process in the cgroup regardless.

use super::*;

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

// CONSTANTS

const CGROUP_MOUNT: &str = "/sys/fs/cgroup";
const SELF_CGROUP_FILE: &str = "/proc/self/cgroup";
/// Unified-hierarchy entries in `/proc/self/cgroup` read `0::<path>`.
const UNIFIED_HIERARCHY_PREFIX: &str = "0::";
/// Workload cgroups are named `acps-<owner pid>-<sequence>`, so a sweep can tell a
/// dead owner's leftovers from a live process's workloads.
const WORKLOAD_CGROUP_PREFIX: &str = "acps-";
const PREFLIGHT_CGROUP_SEQUENCE: &str = "preflight";
const CGROUP_PROCS_FILE: &str = "cgroup.procs";
const CGROUP_KILL_FILE: &str = "cgroup.kill";
const CGROUP_EVENTS_FILE: &str = "cgroup.events";
const POPULATED_FALSE_LINE: &str = "populated 0";
/// Writing `0` to `cgroup.procs` moves the writing process itself.
const MIGRATE_SELF: &[u8] = b"0";
const KILL_ALL: &[u8] = b"1";
const DRAIN_POLL_INTERVAL: Duration = Duration::from_millis(20);
const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

static NEXT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Whether wrapped spawns under `profile` are contained in a per-spawn cgroup: `off`
/// with an identity is the one mode with no namespace to tear down.
pub fn uses_workload_cgroups(profile: &SandboxProfile) -> bool {
    profile.config.mode == SandboxMode::Off && profile.identity.is_some()
}

/// The runtime's own cgroup directory; workload cgroups are created beneath it.
fn own_cgroup_dir() -> std::result::Result<PathBuf, String> {
    let membership = std::fs::read_to_string(SELF_CGROUP_FILE)
        .map_err(|error| format!("reading {SELF_CGROUP_FILE} failed: {error}"))?;
    let relative = membership
        .lines()
        .find_map(|line| line.strip_prefix(UNIFIED_HIERARCHY_PREFIX))
        .ok_or_else(|| {
            format!(
                "{SELF_CGROUP_FILE} has no cgroup v2 (`0::`) entry; a cgroup v2 host is required"
            )
        })?;
    Ok(Path::new(CGROUP_MOUNT).join(relative.trim_start_matches('/')))
}

#[cfg(target_os = "linux")]
fn require_cgroup2_mount() -> std::result::Result<(), String> {
    let mount = CString::new(CGROUP_MOUNT).map_err(|error| error.to_string())?;
    // SAFETY: `statfs` writes into the zeroed struct; the path is a valid C string.
    let mut stats: libc::statfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statfs(mount.as_ptr(), &mut stats) };
    if rc != 0 {
        return Err(format!(
            "statfs {CGROUP_MOUNT} failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    if stats.f_type != libc::CGROUP2_SUPER_MAGIC {
        return Err(format!("{CGROUP_MOUNT} is not a cgroup v2 mount"));
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn require_cgroup2_mount() -> std::result::Result<(), String> {
    Err("per-workload cgroups require Linux cgroup v2".to_owned())
}

/// Whether this host can contain `off`-mode workloads: cgroup v2, a runtime cgroup the
/// runtime may create children in, and `cgroup.kill` (Linux 5.14+).
pub fn preflight() -> std::result::Result<(), String> {
    require_cgroup2_mount()?;
    let own = own_cgroup_dir()?;
    let probe = own.join(format!(
        "{WORKLOAD_CGROUP_PREFIX}{}-{PREFLIGHT_CGROUP_SEQUENCE}",
        std::process::id()
    ));
    std::fs::create_dir(&probe).map_err(|error| {
        format!(
            "creating a workload cgroup under {} failed: {error}; delegate the runtime's cgroup \
             to the runtime user (for example systemd Delegate=yes)",
            own.display()
        )
    })?;
    let has_kill = probe.join(CGROUP_KILL_FILE).exists();
    let removed = std::fs::remove_dir(&probe);
    if let Err(error) = removed {
        return Err(format!(
            "removing the preflight cgroup {} failed: {error}",
            probe.display()
        ));
    }
    if !has_kill {
        return Err("cgroup.kill is unavailable; Linux 5.14 or newer is required".to_owned());
    }
    Ok(())
}

/// Kill and remove workload cgroups whose owning process is gone, such as those a crashed
/// runtime left behind. Returns how many were removed.
pub fn sweep_stale() -> usize {
    let own = match own_cgroup_dir() {
        Ok(own) => own,
        Err(reason) => {
            tracing::warn!(%reason, "skipping the stale workload cgroup sweep");
            return 0;
        }
    };
    let entries = match std::fs::read_dir(&own) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::warn!(%error, cgroup = %own.display(), "listing workload cgroups failed");
            return 0;
        }
    };
    let mut removed = 0;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                tracing::warn!(%error, cgroup = %own.display(), "reading a workload cgroup entry failed");
                continue;
            }
        };
        let name = entry.file_name();
        let Some(owner) = name
            .to_str()
            .and_then(|name| name.strip_prefix(WORKLOAD_CGROUP_PREFIX))
            .and_then(|rest| rest.split('-').next())
            .and_then(|pid| pid.parse::<i32>().ok())
        else {
            continue;
        };
        if !sweepable(owner) {
            continue;
        }
        let path = entry.path();
        match kill_and_remove(&path) {
            Ok(()) => removed += 1,
            Err(error) => {
                tracing::warn!(%error, cgroup = %path.display(), "removing a stale workload cgroup failed");
            }
        }
    }
    removed
}

/// The sweep runs before this process spawns anything, so cgroups carrying its own pid were left
/// by an earlier runtime that had the same pid, as in a container restart.
fn sweepable(owner: i32) -> bool {
    owner == std::process::id() as i32 || !process_alive(owner)
}

fn process_alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks for existence and permission.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn write_control(path: &Path, value: &[u8]) -> std::io::Result<()> {
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)?
        .write_all(value)
}

fn populated(cgroup: &Path) -> std::io::Result<bool> {
    let events = std::fs::read_to_string(cgroup.join(CGROUP_EVENTS_FILE))?;
    Ok(!events
        .lines()
        .any(|line| line.trim() == POPULATED_FALSE_LINE))
}

fn kill_and_remove(cgroup: &Path) -> std::io::Result<()> {
    write_control(&cgroup.join(CGROUP_KILL_FILE), KILL_ALL)?;
    let deadline = Instant::now() + DRAIN_TIMEOUT;
    while populated(cgroup)? {
        if Instant::now() >= deadline {
            return Err(std::io::Error::other(
                "the cgroup still holds processes after cgroup.kill",
            ));
        }
        std::thread::sleep(DRAIN_POLL_INTERVAL);
    }
    std::fs::remove_dir(cgroup)
}

/// One wrapped spawn's cgroup. The child joins it before exec; dropping the guard kills
/// whatever is still inside and removes it.
#[derive(Debug)]
pub struct WorkloadCgroup {
    path: PathBuf,
    procs: std::fs::File,
}

impl WorkloadCgroup {
    /// A fresh cgroup when `profile` calls for one, otherwise `None`.
    pub fn for_profile(profile: &SandboxProfile) -> std::io::Result<Option<Self>> {
        if !uses_workload_cgroups(profile) {
            return Ok(None);
        }
        let own = own_cgroup_dir().map_err(std::io::Error::other)?;
        let path = own.join(format!(
            "{WORKLOAD_CGROUP_PREFIX}{}-{}",
            std::process::id(),
            NEXT_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path)?;
        let procs = match std::fs::OpenOptions::new()
            .write(true)
            .open(path.join(CGROUP_PROCS_FILE))
        {
            Ok(procs) => procs,
            Err(error) => {
                if let Err(remove_error) = std::fs::remove_dir(&path) {
                    tracing::warn!(error = %remove_error, cgroup = %path.display(), "removing an unused workload cgroup failed");
                }
                return Err(error);
            }
        };
        Ok(Some(Self { path, procs }))
    }

    /// Move the child into this cgroup between fork and exec, so the workload never runs
    /// outside it. The guard must outlive the spawn.
    #[cfg(unix)]
    pub fn enter_before_exec(&self, command: &mut tokio::process::Command) {
        use std::os::fd::AsRawFd;
        let procs_fd = self.procs.as_raw_fd();
        // SAFETY: `write` is async-signal-safe and the fd stays open across the spawn because
        // the guard owns it.
        unsafe {
            command.pre_exec(move || {
                let written =
                    libc::write(procs_fd, MIGRATE_SELF.as_ptr().cast(), MIGRATE_SELF.len());
                if written == MIGRATE_SELF.len() as isize {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            });
        }
    }

    /// SIGKILL every process in the cgroup, descendants that left the process group included.
    pub fn kill(&self) {
        if let Err(error) = write_control(&self.path.join(CGROUP_KILL_FILE), KILL_ALL) {
            tracing::warn!(%error, cgroup = %self.path.display(), "cgroup.kill failed");
        }
    }
}

impl Drop for WorkloadCgroup {
    fn drop(&mut self) {
        // The kill is inline so it lands even if the process exits before the reaper thread runs;
        // draining can take a scheduler tick per process, so it runs off the caller's thread.
        self.kill();
        let path = self.path.clone();
        let spawned = std::thread::Builder::new()
            .name("acps-cgroup-reap".to_owned())
            .spawn(move || {
                if let Err(error) = kill_and_remove(&path) {
                    tracing::warn!(%error, cgroup = %path.display(), "removing a workload cgroup failed");
                }
            });
        if let Err(error) = spawned {
            tracing::warn!(%error, cgroup = %self.path.display(), "spawning the cgroup reaper failed; the cgroup is left for the next startup sweep");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> WorkloadIdentity {
        WorkloadIdentity {
            name: "agent".to_owned(),
            uid: 2001,
            gid: 2001,
            home: PathBuf::from("/home/agent"),
        }
    }

    #[test]
    fn only_off_with_an_identity_uses_workload_cgroups() {
        let with = |mode: SandboxMode, identity: Option<WorkloadIdentity>| SandboxProfile {
            config: SandboxConfig {
                mode,
                ..SandboxConfig::default()
            },
            identity,
        };
        assert!(uses_workload_cgroups(&with(
            SandboxMode::Off,
            Some(identity())
        )));
        assert!(!uses_workload_cgroups(&with(SandboxMode::Off, None)));
        assert!(!uses_workload_cgroups(&with(
            SandboxMode::Unshare,
            Some(identity())
        )));
        assert!(
            WorkloadCgroup::for_profile(&with(SandboxMode::Unshare, Some(identity())))
                .expect("no cgroup needed")
                .is_none()
        );
    }

    #[test]
    fn stale_cgroups_are_those_of_dead_owners_or_an_earlier_holder_of_our_pid() {
        assert!(!sweepable(std::os::unix::process::parent_id() as i32));
        assert!(sweepable(std::process::id() as i32));
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn true");
        let pid = child.id() as i32;
        child.wait().expect("reap true");
        assert!(sweepable(pid));
    }
}
