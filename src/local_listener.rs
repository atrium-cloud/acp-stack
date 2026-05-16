//! Unix-domain-socket listener that serves the `acpctl` local agent CLI.
//!
//! The local listener exposes an explicit allowlist of operations from the API
//! router on a separate Unix-domain socket. Access control is filesystem based:
//! the socket file is mode `0600` inside an owner-only parent directory.

mod router;
mod socket;

pub use router::build_local_router;
pub use socket::{
    BoundLocalListener, ParentPolicy, SocketGuard, bind_local, default_socket_path, serve_local,
};
