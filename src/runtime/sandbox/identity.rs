//! The workload identity: the separate local user the agent, ACP terminals and
//! mediated commands run as, so a workload cannot rewrite what the runtime owns.

use super::*;

use crate::ownership::PasswdEntry;

/// A `[workspace.sandbox].workload_user` resolved to its passwd entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkloadIdentity {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub home: PathBuf,
}

/// `[workspace.sandbox]` together with the identity it resolves to: what every
/// wrapped spawn carries.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SandboxProfile {
    pub config: SandboxConfig,
    pub identity: Option<WorkloadIdentity>,
}

impl SandboxProfile {
    /// Resolve `workload_user` against the local passwd database.
    pub fn resolve(config: &SandboxConfig) -> Result<Self> {
        Self::resolve_with(
            config,
            (
                crate::ownership::process_euid(),
                crate::ownership::process_egid(),
            ),
            crate::ownership::lookup_user,
        )
    }

    pub(crate) fn resolve_with(
        config: &SandboxConfig,
        (runtime_euid, runtime_egid): (u32, u32),
        lookup: impl FnOnce(&str) -> std::io::Result<Option<PasswdEntry>>,
    ) -> Result<Self> {
        let Some(name) = config.workload_user.as_deref() else {
            return Ok(Self {
                config: config.clone(),
                identity: None,
            });
        };
        let entry = lookup(name)
            .map_err(|source| StackError::WorkloadUserLookupFailed {
                name: name.to_owned(),
                source,
            })?
            .ok_or_else(|| StackError::WorkloadUserUnresolved {
                name: name.to_owned(),
            })?;
        if entry.uid == 0 {
            return Err(StackError::WorkloadUserIsRoot {
                name: name.to_owned(),
            });
        }
        if entry.uid == runtime_euid {
            return Err(StackError::WorkloadUserIsRuntime {
                name: name.to_owned(),
                uid: entry.uid,
            });
        }
        // The workload runs with this primary gid, so it must not grant group access to what root
        // or the runtime owns.
        if entry.gid == 0 || entry.gid == runtime_egid {
            return Err(StackError::WorkloadUserSharesGroup {
                name: name.to_owned(),
                gid: entry.gid,
            });
        }
        Ok(Self {
            config: config.clone(),
            identity: Some(WorkloadIdentity {
                name: name.to_owned(),
                uid: entry.uid,
                gid: entry.gid,
                home: entry.home,
            }),
        })
    }

    /// The HOME the workload sees: the identity's home, or the runtime's own when none is declared.
    pub fn workload_home<'a>(&'a self, runtime_home: &'a Path) -> &'a Path {
        self.identity
            .as_ref()
            .map_or(runtime_home, |identity| identity.home.as_path())
    }

    /// Where workload-owned file I/O runs: with the identity's filesystem credentials when one is
    /// declared, otherwise with the process's own.
    pub fn executor(&self) -> crate::workload_fs::Executor {
        match &self.identity {
            Some(identity) => {
                crate::workload_fs::Executor::Workload(crate::workload_fs::FsCredentials {
                    uid: identity.uid,
                    gid: identity.gid,
                })
            }
            None => crate::workload_fs::Executor::Process,
        }
    }

    /// How workload file I/O treats links: refused with an identity, followed without one, where
    /// the workload already runs as the runtime. `contained` keeps resolved paths below the root.
    pub fn link_policy(&self, contained: bool) -> crate::workload_fs::LinkPolicy {
        match self.identity {
            Some(_) => crate::workload_fs::LinkPolicy::Refuse,
            None => crate::workload_fs::LinkPolicy::Follow { contained },
        }
    }

    /// Whether a copy into workload-owned space may read a hard-linked source file. With an
    /// identity, a link the workload planted could alias a file only the runtime can read.
    pub fn accepts_hard_links(&self) -> bool {
        self.identity.is_none()
    }

    /// The directories of `dirs` the workload identity cannot write; all of them without an
    /// identity, where the workload is the runtime and the distinction is moot.
    pub fn without_workload_writable(&self, dirs: Vec<PathBuf>) -> Vec<PathBuf> {
        if self.identity.is_none() {
            return dirs;
        }
        let (absolute, relative): (Vec<PathBuf>, Vec<PathBuf>) =
            dirs.into_iter().partition(|dir| dir.is_absolute());
        for dir in &relative {
            tracing::warn!(dir = %dir.display(), "dropping a relative PATH entry");
        }
        let writable = match crate::workload_fs::workload_writable_components(
            &self.executor(),
            &absolute,
        ) {
            Ok(writable) => writable,
            Err(error) => {
                tracing::warn!(%error, "PATH writability could not be checked; dropping every entry");
                return Vec::new();
            }
        };
        absolute
            .into_iter()
            .zip(writable)
            .filter_map(|(dir, writable)| match writable {
                None => Some(dir),
                Some(writable) => {
                    tracing::warn!(
                        dir = %dir.display(),
                        writable = %writable.display(),
                        "dropping a PATH entry the workload user can write"
                    );
                    None
                }
            })
            .collect()
    }

    /// The uid and gid the wrapper drops to.
    pub fn drop_ids(&self) -> (u32, u32) {
        match &self.identity {
            Some(identity) => (identity.uid, identity.gid),
            None => (
                crate::ownership::process_euid(),
                crate::ownership::process_egid(),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUNTIME_UID: u32 = 1001;
    const RUNTIME_GID: u32 = 1001;
    const RUNTIME_IDS: (u32, u32) = (RUNTIME_UID, RUNTIME_GID);
    const WORKLOAD_GID: u32 = 2001;

    fn declared(name: &str) -> SandboxConfig {
        SandboxConfig {
            mode: SandboxMode::Unshare,
            workload_user: Some(name.to_owned()),
            ..SandboxConfig::default()
        }
    }

    fn entry(uid: u32) -> PasswdEntry {
        entry_with_gid(uid, WORKLOAD_GID)
    }

    fn entry_with_gid(uid: u32, gid: u32) -> PasswdEntry {
        PasswdEntry {
            uid,
            gid,
            home: PathBuf::from("/home/agent"),
        }
    }

    #[test]
    fn declared_user_resolves_uid_gid_and_home() {
        let profile = SandboxProfile::resolve_with(&declared("agent"), RUNTIME_IDS, |name| {
            assert_eq!(name, "agent");
            Ok(Some(entry(2001)))
        })
        .expect("resolve");
        let identity = profile.identity.clone().expect("identity");
        assert_eq!((identity.uid, identity.gid), (2001, 2001));
        assert_eq!(profile.drop_ids(), (2001, 2001));
        assert_eq!(
            profile.workload_home(Path::new("/home/runtime")),
            Path::new("/home/agent")
        );
    }

    #[test]
    fn refusals_carry_named_codes() {
        let unresolved =
            SandboxProfile::resolve_with(&declared("ghost"), RUNTIME_IDS, |_| Ok(None))
                .expect_err("unresolved");
        assert_eq!(unresolved.error_code(), "sandbox.workload_user_unresolved");

        let root =
            SandboxProfile::resolve_with(&declared("root"), RUNTIME_IDS, |_| Ok(Some(entry(0))))
                .expect_err("root");
        assert_eq!(root.error_code(), "sandbox.workload_user_is_root");

        let same = SandboxProfile::resolve_with(&declared("runtime"), RUNTIME_IDS, |_| {
            Ok(Some(entry(RUNTIME_UID)))
        })
        .expect_err("runtime uid");
        assert_eq!(same.error_code(), "sandbox.workload_user_is_runtime");

        for shared_gid in [0, RUNTIME_GID] {
            let shared = SandboxProfile::resolve_with(&declared("agent"), RUNTIME_IDS, |_| {
                Ok(Some(entry_with_gid(2001, shared_gid)))
            })
            .expect_err("shared primary group");
            assert_eq!(shared.error_code(), "sandbox.workload_user_shares_group");
        }

        let lookup = SandboxProfile::resolve_with(&declared("agent"), RUNTIME_IDS, |_| {
            Err(std::io::Error::other("nss unavailable"))
        })
        .expect_err("lookup failure");
        assert_eq!(lookup.error_code(), "sandbox.workload_user_lookup_failed");
    }
}
