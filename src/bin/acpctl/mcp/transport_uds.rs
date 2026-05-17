//! Streamable-HTTP MCP transport over a Unix-domain socket. Lets MCP clients
//! that dial `unix:/path/to/socket` connect without spawning `acpctl mcp
//! serve` as a child process.
//!
//! Bind safety is delegated to `acp_stack::local_listener::bind_local`, which
//! validates / repairs the parent directory according to `ParentPolicy`,
//! refuses to overwrite a non-socket inode, probes any pre-existing socket
//! for liveness before unlinking, and returns a `SocketGuard` that cleans up
//! by inode identity. The default path under `~/.local/share/acp-stack/` is
//! daemon-managed (`RepairOwnerOnly`); a custom `--bind` is operator-managed
//! and validated strictly (`ValidateOwnerOnly`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use hyper::server::conn::http1;
use hyper_util::rt::TokioIo;
use hyper_util::service::TowerToHyperService;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::tower::{
    StreamableHttpServerConfig, StreamableHttpService,
};
use tokio::net::UnixListener;

use acp_stack::local_listener;

use super::server::AcpctlMcpServer;

const DEFAULT_BIND_RELATIVE: &str = ".local/share/acp-stack/acpctl-mcp.sock";

pub(crate) async fn serve_http_uds(
    daemon_socket: &Path,
    bind: Option<PathBuf>,
) -> Result<(), String> {
    // The default path lives inside the daemon's owned tree, so we may chmod
    // an existing parent back to 0o700. Any operator override must already
    // sit inside an owner-only directory; `ValidateOwnerOnly` rejects shared
    // parents instead of silently locking them down.
    let (bind_path, policy) = match bind {
        Some(custom) => (custom, local_listener::ParentPolicy::ValidateOwnerOnly),
        None => (
            default_bind_path()?,
            local_listener::ParentPolicy::RepairOwnerOnly,
        ),
    };

    let bound = local_listener::bind_local(&bind_path, policy)
        .await
        .map_err(|err| format!("bind {}: {err}", bind_path.display()))?;
    let (listener, guard) = bound.into_parts();

    let daemon_socket = daemon_socket.to_path_buf();
    let factory = move || -> Result<AcpctlMcpServer, std::io::Error> {
        Ok(AcpctlMcpServer::new(daemon_socket.clone()))
    };
    // rmcp's streamable HTTP server defaults to a loopback-only `Host` header
    // allowlist as DNS-rebinding protection. That protection has no meaning
    // over a Unix-domain socket (no DNS resolution can reach us), so we
    // disable the check rather than force every client to pretend its URI
    // authority is `localhost`. The UDS file mode (0600 in a 0700 dir) is
    // the real capability boundary.
    let config = StreamableHttpServerConfig::default().disable_allowed_hosts();
    let service = Arc::new(StreamableHttpService::new(
        factory,
        Arc::new(LocalSessionManager::default()),
        config,
    ));

    eprintln!(
        "acpctl mcp serve: listening on unix:{} (http-uds)",
        bind_path.display()
    );

    let result = tokio::select! {
        _ = shutdown_signal() => Ok(()),
        result = accept_loop(listener, service) => result,
    };

    // Dropping `guard` here triggers inode-identity cleanup of the socket
    // inode bound above. Doing it explicitly makes the lifetime obvious.
    drop(guard);
    result
}

/// Wait for either SIGINT (Ctrl-C) or SIGTERM (supervisor stop). The daemon
/// itself listens to both via `api::shutdown_signal`; we mirror that here so
/// `acpctl mcp serve` cleans up its socket inode whether the parent kills
/// us with TERM or the operator presses Ctrl-C.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(sig) => sig,
            Err(err) => {
                tracing::warn!(error = %err, "could not install SIGTERM handler; SIGINT only");
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

async fn accept_loop(
    listener: UnixListener,
    service: Arc<StreamableHttpService<AcpctlMcpServer, LocalSessionManager>>,
) -> Result<(), String> {
    loop {
        let (stream, _addr) = listener
            .accept()
            .await
            .map_err(|err| format!("accept on UDS: {err}"))?;
        let tower_service = (*service).clone();
        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            let hyper_service = TowerToHyperService::new(tower_service);
            let result = http1::Builder::new()
                .keep_alive(true)
                .serve_connection(io, hyper_service)
                .await;
            if let Err(err) = result {
                tracing::warn!(error = %err, "acpctl mcp http-uds connection error");
            }
        });
    }
}

fn default_bind_path() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| "$HOME is not set; pass --bind explicitly".to_owned())?;
    if home.is_empty() {
        return Err("$HOME is empty; pass --bind explicitly".to_owned());
    }
    Ok(PathBuf::from(home).join(DEFAULT_BIND_RELATIVE))
}
