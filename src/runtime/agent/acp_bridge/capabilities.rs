use super::*;

use agent_client_protocol::schema::v1::ContentBlock;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Our owned view of the `initialize` response. Mirrors the protocol shape
/// but is independent of the SDK's `AgentCapabilities` type so our
/// `GET /v1/agent/capabilities` JSON contract stays stable across SDK
/// minor-version churn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentCapabilitiesDto {
    pub protocol_version: u16,
    /// Raw JSON object of the agent's advertised capabilities. We surface it
    /// verbatim so clients can read every field today without the daemon
    /// growing a struct for each one. Named accessors land alongside the
    /// session API.
    pub capabilities: Value,
    /// `agentInfo.name` if the agent provided it. The spec says `SHOULD`,
    /// not `MUST`, so this is best-effort.
    pub agent_name: Option<String>,
    pub agent_title: Option<String>,
    pub agent_version: Option<String>,
}

/// A configured MCP server the running agent cannot be given, plus the
/// capability it would have had to advertise. Recorded per session so the
/// degraded set is visible in durable logs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkippedMcpServer {
    pub name: String,
    pub capability: &'static str,
}

/// Resolved MCP servers split against the agent's advertised transports.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PartitionedMcpServers {
    pub accepted: Vec<McpServer>,
    pub skipped: Vec<SkippedMcpServer>,
}

/// A configured feature the runtime routes around because the agent does not
/// advertise the capability backing it. The feature stays in config; this
/// record surfaces the omission through init reports and session events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct IgnoredFeature {
    /// What kind of configured feature was ignored: `mcp.server`,
    /// `agent.mode`, or `agent.model`.
    pub feature: &'static str,
    /// The configured value: the MCP server name, or the mode/model value.
    pub target: String,
    /// The capability the agent would have had to advertise
    /// (`mcpCapabilities.*`), or — for `agent.mode`/`agent.model` — the
    /// `session/new` config option that would have had to carry the value.
    pub capability: &'static str,
    pub reason: String,
}

pub const IGNORED_FEATURE_MCP_SERVER: &str = "mcp.server";
pub const IGNORED_FEATURE_AGENT_MODE: &str = "agent.mode";
pub const IGNORED_FEATURE_AGENT_MODEL: &str = "agent.model";

impl AgentCapabilitiesDto {
    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string(self).map_err(|err| StackError::AgentInitializeFailed {
            reason: format!("failed to serialize agent capabilities: {err}"),
        })
    }

    /// Whether the agent advertised the `load_session` capability in its
    /// `initialize` response. Used to gate `POST /v1/sessions/{id}/load`.
    pub fn supports_load_session(&self) -> bool {
        self.capabilities
            .get("loadSession")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    pub fn supports_list_sessions(&self) -> bool {
        self.supports_session_capability("list")
    }

    pub fn supports_resume_session(&self) -> bool {
        self.supports_session_capability("resume")
    }

    pub fn supports_close_session(&self) -> bool {
        self.supports_session_capability("close")
    }

    pub fn supports_delete_session(&self) -> bool {
        self.supports_session_capability("delete")
    }

    pub fn supports_fork_session(&self) -> bool {
        self.supports_session_capability("fork")
    }

    pub fn supports_fork_message_id(&self) -> bool {
        let fork = self
            .capabilities
            .get("sessionCapabilities")
            .and_then(Value::as_object)
            .and_then(|caps| caps.get("fork"))
            .and_then(Value::as_object);
        fork.and_then(|fork| fork.get("_meta"))
            .and_then(Value::as_object)
            .and_then(|meta| meta.get("acpStack"))
            .and_then(Value::as_object)
            .and_then(|stack| stack.get("messageId"))
            .is_some_and(Value::is_object)
    }

    fn supports_prompt_capability(&self, name: &str) -> bool {
        self.capabilities
            .get("promptCapabilities")
            .and_then(Value::as_object)
            .and_then(|capabilities| capabilities.get(name))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    /// Whether the agent advertised any MCP capability at all. Gates both the
    /// init MCP prompts and session-time stdio attachment: an agent whose
    /// `mcpCapabilities` is absent or claims nothing gives no evidence MCP
    /// works, so it is offered no prompts and sent no servers.
    pub fn advertises_mcp_support(&self) -> bool {
        self.capabilities
            .get("mcpCapabilities")
            .and_then(Value::as_object)
            .is_some_and(|capabilities| {
                capabilities
                    .iter()
                    .any(|(_, value)| value.as_bool().unwrap_or(false))
            })
    }

    /// Whether the agent advertised a specific MCP capability (`http`/`sse`).
    pub fn supports_mcp_capability(&self, name: &str) -> bool {
        self.capabilities
            .get("mcpCapabilities")
            .and_then(Value::as_object)
            .and_then(|capabilities| capabilities.get(name))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    pub(super) fn validate_prompt(&self, prompt: &[ContentBlock]) -> Result<()> {
        for block in prompt {
            let required = match block {
                ContentBlock::Text(_) | ContentBlock::ResourceLink(_) => None,
                ContentBlock::Image(_) => Some(("image", "promptCapabilities.image")),
                ContentBlock::Audio(_) => Some(("audio", "promptCapabilities.audio")),
                ContentBlock::Resource(_) => {
                    Some(("embeddedContext", "promptCapabilities.embeddedContext"))
                }
                _ => {
                    return Err(StackError::AgentUnsupportedCapability {
                        name: "promptCapabilities.unknown",
                    });
                }
            };
            if let Some((capability, error_name)) = required
                && !self.supports_prompt_capability(capability)
            {
                return Err(StackError::AgentUnsupportedCapability { name: error_name });
            }
        }
        Ok(())
    }

    /// `Ok(None)` when the agent can accept this server, `Ok(Some(capability))`
    /// when it does not advertise the transport, and `Err` for a transport
    /// variant we do not model — we cannot reason about a shape we don't know,
    /// so that stays a hard failure.
    ///
    /// stdio has no dedicated advertisement flag in ACP, so it requires at
    /// least one advertised MCP capability: some agents reject `session/new`
    /// outright when handed servers they cannot serve, so an advertisement
    /// claiming no MCP means no MCP servers of any transport are sent.
    fn unsupported_mcp_capability(&self, server: &McpServer) -> Result<Option<&'static str>> {
        match server {
            McpServer::Stdio(_) => {
                Ok((!self.advertises_mcp_support()).then_some("mcpCapabilities"))
            }
            McpServer::Http(_) => {
                Ok((!self.supports_mcp_capability("http")).then_some("mcpCapabilities.http"))
            }
            McpServer::Sse(_) => {
                Ok((!self.supports_mcp_capability("sse")).then_some("mcpCapabilities.sse"))
            }
            _ => Err(StackError::AgentUnsupportedCapability {
                name: "mcpCapabilities.unknown",
            }),
        }
    }

    /// Split the resolved MCP servers into the ones this agent can accept and
    /// the ones whose transport it does not advertise.
    ///
    /// One undeliverable integration must not make sessions uncreatable: an
    /// operator declaring an HTTP MCP server against an adapter that only
    /// speaks stdio still gets a working session, minus that server. Callers
    /// are responsible for recording the skipped set.
    pub fn partition_mcp_servers(&self, servers: Vec<McpServer>) -> Result<PartitionedMcpServers> {
        let mut partitioned = PartitionedMcpServers::default();
        for server in servers {
            match self.unsupported_mcp_capability(&server)? {
                Some(capability) => partitioned.skipped.push(SkippedMcpServer {
                    name: crate::runtime::agent::mcp::server_name(&server).to_owned(),
                    capability,
                }),
                None => partitioned.accepted.push(server),
            }
        }
        Ok(partitioned)
    }

    /// Config-level MCP assessment for init reporting. Wraps
    /// `partition_mcp_servers` so init reports and session-time partitioning
    /// cannot drift apart.
    pub fn ignored_mcp_features(&self, servers: Vec<McpServer>) -> Result<Vec<IgnoredFeature>> {
        let partitioned = self.partition_mcp_servers(servers)?;
        Ok(partitioned
            .skipped
            .into_iter()
            .map(|skipped| IgnoredFeature {
                feature: IGNORED_FEATURE_MCP_SERVER,
                target: skipped.name,
                capability: skipped.capability,
                reason: "agent does not advertise this MCP transport".to_owned(),
            })
            .collect())
    }

    /// Invariant guard on the bridge's own request paths. Transport support is
    /// decided upstream by `partition_mcp_servers`; what must never reach the
    /// wire is a variant we cannot classify at all.
    pub(super) fn reject_unmodeled_mcp_servers(&self, servers: &[McpServer]) -> Result<()> {
        for server in servers {
            self.unsupported_mcp_capability(server)?;
        }
        Ok(())
    }

    fn supports_session_capability(&self, name: &str) -> bool {
        self.capabilities
            .get("sessionCapabilities")
            .and_then(Value::as_object)
            .and_then(|caps| caps.get(name))
            .is_some_and(Value::is_object)
    }

    pub(super) fn from_initialize_response(response: &InitializeResponse) -> Result<Self> {
        // The SDK's `AgentCapabilities` is a typed struct that may rename
        // fields between minor versions; serialize through serde_json to keep
        // our durable storage and API contract independent of that surface.
        let raw_caps = serde_json::to_value(&response.agent_capabilities).map_err(|err| {
            StackError::AgentInitializeFailed {
                reason: format!("failed to serialize agent capabilities: {err}"),
            }
        })?;
        let protocol_version = response.protocol_version.as_u16();
        let (agent_name, agent_title, agent_version) = match serde_json::to_value(response) {
            Ok(Value::Object(map)) => {
                let info = map.get("agentInfo").cloned().unwrap_or(Value::Null);
                (
                    info.get("name").and_then(Value::as_str).map(str::to_owned),
                    info.get("title").and_then(Value::as_str).map(str::to_owned),
                    info.get("version")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                )
            }
            _ => (None, None, None),
        };
        Ok(Self {
            protocol_version,
            capabilities: raw_caps,
            agent_name,
            agent_title,
            agent_version,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use agent_client_protocol::schema::v1::{McpServerHttp, McpServerStdio};
    use serde_json::json;

    fn capabilities_with(mcp: Value) -> AgentCapabilitiesDto {
        AgentCapabilitiesDto {
            protocol_version: 1,
            capabilities: json!({ "mcpCapabilities": mcp }),
            agent_name: None,
            agent_title: None,
            agent_version: None,
        }
    }

    fn http_server(name: &str) -> McpServer {
        McpServer::Http(McpServerHttp::new(name, "https://example.invalid/mcp"))
    }

    fn stdio_server(name: &str) -> McpServer {
        McpServer::Stdio(McpServerStdio::new(
            name,
            std::path::PathBuf::from("/bin/sh"),
        ))
    }

    #[test]
    fn partition_keeps_servers_the_agent_advertises() {
        let capabilities = capabilities_with(json!({ "http": true }));
        let partitioned = capabilities
            .partition_mcp_servers(vec![stdio_server("local"), http_server("linear")])
            .expect("partition");

        assert_eq!(partitioned.accepted.len(), 2);
        assert!(partitioned.skipped.is_empty());
    }

    #[test]
    fn partition_drops_every_server_when_no_mcp_capability_is_claimed() {
        let capabilities = capabilities_with(json!({ "http": false }));
        let partitioned = capabilities
            .partition_mcp_servers(vec![stdio_server("local"), http_server("linear")])
            .expect("partition");

        assert!(partitioned.accepted.is_empty());
        assert_eq!(
            partitioned.skipped,
            vec![
                SkippedMcpServer {
                    name: "local".to_owned(),
                    capability: "mcpCapabilities",
                },
                SkippedMcpServer {
                    name: "linear".to_owned(),
                    capability: "mcpCapabilities.http",
                }
            ]
        );
    }

    #[test]
    fn partition_keeps_stdio_when_any_mcp_capability_is_claimed() {
        // sse:true is evidence the agent engages with MCP, so the stdio
        // baseline rides along even though http stays skipped.
        let capabilities = capabilities_with(json!({ "sse": true }));
        let partitioned = capabilities
            .partition_mcp_servers(vec![stdio_server("local"), http_server("linear")])
            .expect("partition");

        assert_eq!(partitioned.accepted.len(), 1);
        assert_eq!(
            crate::runtime::agent::mcp::server_name(&partitioned.accepted[0]),
            "local"
        );
        assert_eq!(partitioned.skipped.len(), 1);
    }

    #[test]
    fn unadvertised_transport_no_longer_fails_the_whole_session() {
        // Regression: a `[[mcp.servers]] type = "http"` declaration against an
        // adapter that only speaks stdio used to make session create impossible.
        let capabilities = capabilities_with(json!({}));
        let partitioned = capabilities
            .partition_mcp_servers(vec![http_server("linear")])
            .expect("partition must not error on an unadvertised transport");

        assert!(partitioned.accepted.is_empty());
        assert_eq!(partitioned.skipped.len(), 1);
    }

    #[test]
    fn ignored_mcp_features_reports_all_servers_when_no_mcp_is_claimed() {
        let capabilities = capabilities_with(json!({ "http": false }));
        let ignored = capabilities
            .ignored_mcp_features(vec![stdio_server("local"), http_server("linear")])
            .expect("assess");

        assert_eq!(ignored.len(), 2);
        assert_eq!(ignored[0].feature, IGNORED_FEATURE_MCP_SERVER);
        assert_eq!(ignored[0].target, "local");
        assert_eq!(ignored[0].capability, "mcpCapabilities");
        assert_eq!(ignored[1].target, "linear");
        assert_eq!(ignored[1].capability, "mcpCapabilities.http");
    }

    #[test]
    fn ignored_mcp_features_is_empty_when_transports_are_advertised() {
        let capabilities = capabilities_with(json!({ "http": true }));
        let ignored = capabilities
            .ignored_mcp_features(vec![stdio_server("local"), http_server("linear")])
            .expect("assess");
        assert!(ignored.is_empty());
    }

    #[test]
    fn advertises_mcp_support_requires_a_true_capability() {
        assert!(capabilities_with(json!({ "http": true })).advertises_mcp_support());
        assert!(!capabilities_with(json!({ "http": false })).advertises_mcp_support());
        assert!(!capabilities_with(json!({})).advertises_mcp_support());
        let no_mcp_key = AgentCapabilitiesDto {
            protocol_version: 1,
            capabilities: json!({}),
            agent_name: None,
            agent_title: None,
            agent_version: None,
        };
        assert!(!no_mcp_key.advertises_mcp_support());
    }

    #[test]
    fn modeled_transports_pass_the_bridge_invariant_guard() {
        let capabilities = capabilities_with(json!({ "http": false }));
        capabilities
            .reject_unmodeled_mcp_servers(&[stdio_server("local"), http_server("linear")])
            .expect("both variants are modeled");
    }
}
