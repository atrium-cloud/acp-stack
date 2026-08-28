use std::path::{Path, PathBuf};

use tokio::net::{UnixListener, UnixStream};

use super::router::build_local_router;
use crate::api::{self, AppState};
use crate::error::{Result, StackError};
use crate::fs_util::{create_dir_owner_only, home_dir, parent_dir, set_owner_only_dir};

/// Default socket path: `~/.local/share/acp-stack/acps-local.sock`.
pub fn default_socket_path() -> Result<PathBuf> {
    Ok(socket_path_for_home(&home_dir()?))
}

fn socket_path_for_home(home: &Path) -> PathBuf {
    home.join(".local/share/acp-stack/acps-local.sock")
}

/// Unlinks the socket file on drop, but only when the inode at the path still
/// matches the bound one — otherwise a second daemon that took over the path
/// would have its live socket unlinked.
pub struct SocketGuard {
    path: PathBuf,
    inode: Option<u64>,
}

impl SocketGuard {
    fn new(path: PathBuf, inode: Option<u64>) -> Self {
        Self { path, inode }
    }
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let current_inode = match current_inode(&self.path) {
            Ok(inode) => inode,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return,
            Err(err) => {
                tracing::warn!(error = %err, path = %self.path.display(), "stat local socket on shutdown");
                return;
            }
        };
        if self.inode != Some(current_inode) {
            tracing::warn!(
                path = %self.path.display(),
                bound_inode = ?self.inode,
                current_inode,
                "local socket inode changed since bind; refusing to unlink (another daemon may own it)",
            );
            return;
        }
        if let Err(err) = std::fs::remove_file(&self.path)
            && err.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(error = %err, path = %self.path.display(), "failed to unlink local socket");
        }
    }
}

/// Bound listener with its cleanup guard.
pub struct BoundLocalListener {
    listener: UnixListener,
    guard: SocketGuard,
}

/// Whether the listener may chmod (`0o700`) an already-existing socket parent
/// directory.
pub enum ParentPolicy {
    /// Daemon-managed parent: create if missing, chmod to `0o700` if existing.
    RepairOwnerOnly,
    /// Operator-configured parent: created `0o700`, but an existing one is only
    /// validated — startup fails rather than silently widening a shared dir.
    ValidateOwnerOnly,
}

/// Prepare the parent directory, clear a stale socket inode, bind a
/// `UnixListener`, and chmod it to `0o600`; refuses to bind when a live daemon
/// already owns the socket.
pub async fn bind_local(
    socket_path: &Path,
    parent_policy: ParentPolicy,
) -> Result<BoundLocalListener> {
    let parent = parent_dir(socket_path)?.to_path_buf();
    prepare_parent_dir(&parent, parent_policy)?;
    handle_existing_socket(socket_path).await?;

    let listener =
        UnixListener::bind(socket_path).map_err(|source| StackError::ServeIo { source })?;
    set_socket_owner_only(socket_path)?;
    let inode = current_inode(socket_path).ok();
    let guard = SocketGuard::new(socket_path.to_path_buf(), inode);

    tracing::info!(path = %socket_path.display(), "local UDS bound");
    Ok(BoundLocalListener { listener, guard })
}

/// Run the accept loop until shutdown, consuming the `SocketGuard` so the
/// socket file is unlinked on exit or task abort.
pub async fn serve_local(state: AppState, bound: BoundLocalListener) -> Result<()> {
    let BoundLocalListener { listener, guard } = bound;
    let _guard = guard;
    let app = build_local_router(state);
    axum::serve(listener, app)
        .with_graceful_shutdown(api::shutdown_signal())
        .await
        .map_err(|source| StackError::ServeIo { source })
}

fn prepare_parent_dir(parent: &Path, policy: ParentPolicy) -> Result<()> {
    if !parent.exists() {
        // Fresh creation is 0o700 regardless of policy: there is no
        // operator-managed mode to preserve yet.
        return create_dir_owner_only(parent);
    }
    match policy {
        ParentPolicy::RepairOwnerOnly => set_owner_only_dir(parent),
        ParentPolicy::ValidateOwnerOnly => validate_parent_dir_owner_only(parent),
    }
}

#[cfg(unix)]
fn validate_parent_dir_owner_only(parent: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;
    let metadata =
        std::fs::symlink_metadata(parent).map_err(|source| StackError::ServeIo { source })?;
    if metadata.file_type().is_symlink() {
        return Err(StackError::ServeIo {
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "local socket parent is a symlink: {} (refusing to follow into an unverified directory)",
                    parent.display()
                ),
            ),
        });
    }
    if !metadata.is_dir() {
        return Err(StackError::ServeIo {
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "local socket parent is not a directory: {}",
                    parent.display()
                ),
            ),
        });
    }
    let mode = metadata.permissions().mode();
    // Any non-owner permission bit is a hard reject: even read access lets
    // other local users discover the socket inode and enumerate clients.
    if mode & 0o077 != 0 {
        return Err(StackError::ServeIo {
            source: std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "local socket parent {} has mode {:o}; require owner-only (0o700) for a custom socket_path",
                    parent.display(),
                    mode & 0o777
                ),
            ),
        });
    }
    let euid = unsafe { libc::geteuid() } as u64;
    if metadata.uid() as u64 != euid {
        return Err(StackError::ServeIo {
            source: std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "local socket parent {} is not owned by the runtime user (uid {} != {})",
                    parent.display(),
                    metadata.uid(),
                    euid
                ),
            ),
        });
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_parent_dir_owner_only(_parent: &Path) -> Result<()> {
    Ok(())
}

/// Detect whether an existing socket at `path` is live (refuse to bind) or
/// stale (unlink and continue), probing with a single `UnixStream::connect`.
/// Non-socket inodes are always rejected: a misconfigured `socket_path` must
/// never destroy user data at the configured location.
async fn handle_existing_socket(path: &Path) -> Result<()> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => return Err(StackError::ServeIo { source }),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        if !metadata.file_type().is_socket() {
            return Err(StackError::ServeIo {
                source: std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    format!(
                        "configured local socket path is occupied by a non-socket: {}",
                        path.display()
                    ),
                ),
            });
        }
    }
    let original_inode = inode_of(&metadata);
    match UnixStream::connect(path).await {
        Ok(_stream) => Err(StackError::ServeIo {
            source: std::io::Error::new(
                std::io::ErrorKind::AddrInUse,
                format!(
                    "another local listener is already accepting on {}; refusing to take over",
                    path.display()
                ),
            ),
        }),
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
            ) =>
        {
            // Re-stat before unlinking: a startup that raced us between the
            // probe and here changed the inode, and their socket must survive.
            let live_inode = match current_inode(path) {
                Ok(inode) => Some(inode),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(source) => return Err(StackError::ServeIo { source }),
            };
            if live_inode.is_some() && live_inode != original_inode {
                return Err(StackError::ServeIo {
                    source: std::io::Error::new(
                        std::io::ErrorKind::AddrInUse,
                        format!(
                            "local socket at {} was replaced concurrently; refusing to unlink another daemon's socket",
                            path.display()
                        ),
                    ),
                });
            }
            if live_inode.is_none() {
                return Ok(());
            }
            std::fs::remove_file(path).map_err(|source| StackError::ServeIo { source })
        }
        Err(source) => Err(StackError::ServeIo { source }),
    }
}

#[cfg(unix)]
fn inode_of(metadata: &std::fs::Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    Some(metadata.ino())
}

#[cfg(not(unix))]
fn inode_of(_metadata: &std::fs::Metadata) -> Option<u64> {
    None
}

#[cfg(unix)]
fn current_inode(path: &Path) -> std::io::Result<u64> {
    use std::os::unix::fs::MetadataExt;
    Ok(std::fs::symlink_metadata(path)?.ino())
}

#[cfg(not(unix))]
fn current_inode(_path: &Path) -> std::io::Result<u64> {
    Ok(0)
}

#[cfg(unix)]
fn set_socket_owner_only(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(path, perms).map_err(|source| StackError::ServeIo { source })
}

#[cfg(not(unix))]
fn set_socket_owner_only(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_socket_path_is_under_local_share() {
        let path = socket_path_for_home(Path::new("/srv/home"));
        let display = path.display().to_string();
        assert!(
            display.ends_with("/.local/share/acp-stack/acps-local.sock"),
            "{display}"
        );
    }
}
