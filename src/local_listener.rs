//! Unix-domain-socket listener that serves the `acpctl` local agent CLI.
//!
//! The local listener exposes an *explicit allowlist* of operations from
//! `api.rs`, mounted on a separate `Router`. Access control is filesystem-
//! permission-based: the socket file is mode `0600` inside a parent directory
//! that the daemon ensures is mode `0700`. There is no API key on this
//! transport — the `tag_local` middleware stamps every request with
//! `KeyKind::Local` so reused handlers attribute writes to `source = "local"`
//! and `enforce_tier` on the public router rejects any Local tag that ever
//! leaks across listeners.
//!
//! The router intentionally omits routes whose semantics are forbidden to
//! agents per `docs/specs/acpctl/acpctl.md:42-52`: no secret reads, no API key
//! rotation, no permission approve/deny, no config import, no admin agent
//! lifecycle controls.

use std::path::{Path, PathBuf};

use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{get, post};
use tokio::net::{UnixListener, UnixStream};
use tower_http::limit::RequestBodyLimitLayer;

use crate::api::{
    self, AppState, commands_submit_handler, config_export_handler, deps_check_handler,
    files_content_get_handler, files_content_put_handler, files_list_handler, logs_events_handler,
    permissions_pending_handler, security_check_handler, status_handler,
};
use crate::auth::KeyKind;
use crate::error::{Result, StackError};
use crate::fs_util::{create_dir_owner_only, home_dir, parent_dir, set_owner_only_dir};

/// Default socket path: `~/.local/share/acp-stack/acpctl.sock`.
pub fn default_socket_path() -> Result<PathBuf> {
    let home = home_dir()?;
    Ok(home.join(".local/share/acp-stack/acpctl.sock"))
}

/// Build the Axum router that the UDS listener serves. Mounts only the 10
/// allowlisted operations exposed to `acpctl`. Anything else over the UDS
/// returns 404 from the framework fallback, rewrapped by `ensure_envelope`.
pub fn build_local_router(state: AppState) -> Router {
    let limit = state.max_request_bytes;

    let routes = Router::new()
        .route("/v1/status", get(status_handler))
        .route("/v1/security/check", get(security_check_handler))
        .route("/v1/deps/check", post(deps_check_handler))
        .route("/v1/logs/events", get(logs_events_handler))
        .route("/v1/files", get(files_list_handler))
        .route(
            "/v1/files/content",
            get(files_content_get_handler).put(files_content_put_handler),
        )
        .route("/v1/commands", post(commands_submit_handler))
        .route("/v1/config/export", get(config_export_handler))
        .route("/v1/permissions/pending", get(permissions_pending_handler))
        .layer(RequestBodyLimitLayer::new(limit))
        .layer(axum::extract::DefaultBodyLimit::disable());

    // Layer ordering matters here. In axum, each `.layer` call wraps further
    // out: the LAST layer added sees requests first and responses last. We
    // want `tag_local` outermost so the `KeyKind::Local` extension is on the
    // request before `ensure_envelope` or `log_api_request` inspect it —
    // otherwise a 413 from `RequestBodyLimitLayer` would short-circuit before
    // the tag was set, and the resulting `security.request_oversized` event
    // would be attributed to `source = "api"` instead of `local`.
    Router::new()
        .merge(routes)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            api::log_api_request,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            api::track_active_requests,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            api::ensure_envelope,
        ))
        .layer(middleware::from_fn(tag_local))
        .with_state(state)
}

/// Stamp every UDS request with `KeyKind::Local` before any handler or
/// downstream middleware (incl. `log_api_request`) inspects extensions.
async fn tag_local(mut req: Request<Body>, next: Next) -> Response {
    req.extensions_mut().insert(KeyKind::Local);
    next.run(req).await
}

/// Unlinks the socket file on drop — but only if the inode at the path still
/// matches the one we bound. Without that check, a second daemon that
/// concurrently took over the same socket path could have its live socket
/// unlinked when the first daemon's guard runs. Inode identity is the cheap
/// invariant we own: `UnixListener::bind` created the inode at this path; a
/// subsequent bind by another process creates a *new* inode at the same
/// path. We refuse to unlink unless they match.
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
        // ENOENT here just means another process already unlinked it (e.g. a
        // manual cleanup); not worth surfacing.
        let current_inode = match current_inode(&self.path) {
            Ok(inode) => inode,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return,
            Err(err) => {
                tracing::warn!(error = %err, path = %self.path.display(), "stat acpctl socket on shutdown");
                return;
            }
        };
        if self.inode != Some(current_inode) {
            tracing::warn!(
                path = %self.path.display(),
                bound_inode = ?self.inode,
                current_inode,
                "acpctl socket inode changed since bind; refusing to unlink (another daemon may own it)",
            );
            return;
        }
        if let Err(err) = std::fs::remove_file(&self.path) {
            if err.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(error = %err, path = %self.path.display(), "failed to unlink acpctl socket");
            }
        }
    }
}

/// Bound listener with its cleanup guard. Returned from `bind_local` so the
/// caller can confirm bind success synchronously (before reporting startup),
/// then hand the pair to `serve_local` for the accept loop.
pub struct BoundLocalListener {
    listener: UnixListener,
    guard: SocketGuard,
}

/// Whether the listener may chmod (`0o700`) an already-existing socket parent
/// directory. The default path under `~/.local/share/acp-stack/` is
/// daemon-managed, so repair is safe; an operator-configured `socket_path`
/// pointing into a shared directory must not have its perms silently changed.
pub enum ParentPolicy {
    /// Daemon-managed parent: create if missing, chmod to `0o700` if existing.
    RepairOwnerOnly,
    /// Operator-configured parent: create if missing (always `0o700` for
    /// fresh dirs); existing parents are validated as owner-only but not
    /// chmodded — fail startup if they are group- or world-writable so a
    /// misconfigured `socket_path` cannot park the socket inside a shared
    /// directory where another local user could unlink or spoof it.
    ValidateOwnerOnly,
}

/// Prepare the parent directory, refuse to bind if a live daemon already owns
/// the socket, remove a stale socket inode if one is present, bind a
/// `UnixListener`, and chmod the socket file to `0o600`. Returns the bound
/// listener + a `SocketGuard` so `cli::run_serve` fails fast if the daemon
/// cannot listen locally, instead of discovering the failure only after the
/// TCP server exits.
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

    tracing::info!(path = %socket_path.display(), "acpctl UDS bound");
    Ok(BoundLocalListener { listener, guard })
}

/// Run the accept loop until shutdown. Consumes the `SocketGuard` so the
/// socket file is unlinked on exit (graceful or task abort).
pub async fn serve_local(state: AppState, bound: BoundLocalListener) -> Result<()> {
    // Take ownership of the guard so its `Drop::drop` runs when this future
    // is cancelled or completes.
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
        // Fresh creation is always 0o700 regardless of policy: nobody owns
        // the path yet, so there is no operator-managed mode to preserve.
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
                    "acpctl socket parent is a symlink: {} (refusing to follow into an unverified directory)",
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
                    "acpctl socket parent is not a directory: {}",
                    parent.display()
                ),
            ),
        });
    }
    let mode = metadata.permissions().mode();
    // Group- or world-accessible (any non-owner permission bits set) is a
    // hard reject — even read access lets other local users discover the
    // socket inode and enumerate clients.
    if mode & 0o077 != 0 {
        return Err(StackError::ServeIo {
            source: std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "acpctl socket parent {} has mode {:o}; require owner-only (0o700) for a custom socket_path",
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
                    "acpctl socket parent {} is not owned by the runtime user (uid {} != {})",
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
/// stale (unlink and continue). The probe is a single `UnixStream::connect`:
/// if it succeeds, a daemon is accepting on this socket and we must not
/// touch it; if it returns `ConnectionRefused`, the inode is orphaned and
/// safe to clean. Other inode types (regular file, directory, symlink) are
/// always rejected — a misconfigured `socket_path` must never destroy user
/// data at the configured location.
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
                        "configured acpctl socket path is occupied by a non-socket: {}",
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
                    "another acpctl listener is already accepting on {}; refusing to take over",
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
            // Re-stat before unlinking. If another startup raced us and
            // bound a fresh socket between our probe and this unlink, the
            // inode will have changed — refusing to unlink leaves their
            // socket intact and surfaces a clean bind error to us.
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
                            "acpctl socket at {} was replaced concurrently; refusing to unlink another daemon's socket",
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
        let path = default_socket_path().expect("home resolves");
        let display = path.display().to_string();
        assert!(
            display.ends_with("/.local/share/acp-stack/acpctl.sock"),
            "{display}"
        );
    }
}
