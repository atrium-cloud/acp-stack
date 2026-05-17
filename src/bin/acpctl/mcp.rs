//! `acpctl mcp serve` — exposes the local introspection interface as an MCP
//! server. The MCP server is a thin tool dispatcher: every tool call is
//! translated into an HTTP/1.1 request against the daemon's local UDS
//! (`acpctl.sock`), so capability enforcement (filesystem perms on the parent
//! socket) and `source = "local"` event attribution come from the existing
//! middleware stack without any duplication.
//!
//! Two transports are supported:
//! - `stdio` (default): the agent spawns `acpctl mcp serve` as a child process
//!   and exchanges JSON-RPC over stdin/stdout. This is the standard MCP local
//!   transport.
//! - `http-uds`: binds a streamable-HTTP MCP endpoint on a Unix-domain socket
//!   so MCP clients that dial UDS URLs (e.g. via
//!   `rmcp::transport::streamable_http_client::unix_socket`) can connect
//!   without spawning a child process.

mod dispatcher;
mod server;
mod tools;
mod transport_uds;

use std::path::Path;

use crate::cli_defs::{McpServeArgs, McpTransport};

pub(crate) async fn run_serve(daemon_socket: &Path, args: McpServeArgs) -> Result<(), String> {
    match args.transport {
        McpTransport::Stdio => server::serve_stdio(daemon_socket).await,
        McpTransport::HttpUds => transport_uds::serve_http_uds(daemon_socket, args.bind).await,
    }
}
