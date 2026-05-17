//! `rmcp::ServerHandler` for the local introspection surface. Implements
//! `list_tools`, `get_tool`, and `call_tool`; every tool call is delegated to
//! the dispatcher, which translates it into a UDS HTTP request against the
//! daemon's local listener.

use std::path::{Path, PathBuf};

use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ErrorData as McpError, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerInfo,
};
use rmcp::service::{RequestContext, RoleServer, ServiceExt};
use rmcp::transport::stdio;
use serde_json::{Map, Value};

use super::dispatcher::{self, DispatchResult};
use super::tools;

#[derive(Clone)]
pub(crate) struct AcpctlMcpServer {
    daemon_socket: PathBuf,
}

impl AcpctlMcpServer {
    pub(crate) fn new(daemon_socket: PathBuf) -> Self {
        Self { daemon_socket }
    }

    fn daemon_socket(&self) -> &Path {
        &self.daemon_socket
    }
}

impl ServerHandler for AcpctlMcpServer {
    fn get_info(&self) -> ServerInfo {
        let capabilities = ServerCapabilities::builder().enable_tools().build();
        ServerInfo::new(capabilities).with_instructions(
            "Constrained local introspection for the acp-stack runtime. Every tool call \
             rides the same UDS allowlist as `acpctl`, so no secret values, API keys, \
             permission approvals, or security toggles are reachable through this server.",
        )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(tools::all()))
    }

    fn get_tool(&self, name: &str) -> Option<rmcp::model::Tool> {
        tools::build(name)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let name = request.name.as_ref();
        let args_value = match request.arguments {
            Some(map) => Value::Object(map),
            None => Value::Object(Map::new()),
        };
        let call = match dispatcher::build_call(name, &args_value) {
            Ok(call) => call,
            Err(err) => return Err(McpError::invalid_params(err, None)),
        };
        match dispatcher::execute(self.daemon_socket(), call).await {
            Ok(DispatchResult::Ok(value)) => Ok(CallToolResult::structured(value)),
            Ok(DispatchResult::Err { status, message }) => {
                let payload = serde_json::json!({
                    "status": status,
                    "message": message,
                });
                Ok(CallToolResult::structured_error(payload))
            }
            Err(err) => Err(McpError::internal_error(
                format!("dispatcher transport error: {err}"),
                None,
            )),
        }
    }
}

/// Run the MCP server over stdio. Returns when the client disconnects, the
/// transport closes, or the process is signalled.
pub(crate) async fn serve_stdio(daemon_socket: &Path) -> Result<(), String> {
    let service = AcpctlMcpServer::new(daemon_socket.to_path_buf())
        .serve(stdio())
        .await
        .map_err(|err| format!("mcp stdio init failed: {err}"))?;
    service
        .waiting()
        .await
        .map_err(|err| format!("mcp stdio loop joined with error: {err}"))?;
    Ok(())
}
