//! Internal Unix-domain-socket listener serving an allowlist of keyless local
//! `acps` routes. Access control is filesystem based: the socket is mode `0600`
//! inside an owner-only parent directory.

mod router;
mod socket;

pub use router::build_local_router;
pub use socket::{BoundLocalListener, ParentPolicy, bind_local, default_socket_path, serve_local};
