use super::*;

use agent_client_protocol::schema::v1::ContentBlock;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Our owned view of the `initialize` response. Mirrors the protocol shape
/// but is independent of the SDK's `AgentCapabilities` type so our
/// `GET /v1/agent/capabilities` JSON contract stays stable across SDK
/// minor-version churn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

    fn supports_mcp_capability(&self, name: &str) -> bool {
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

    pub(super) fn validate_mcp_servers(&self, servers: &[McpServer]) -> Result<()> {
        for server in servers {
            let required = match server {
                McpServer::Stdio(_) => None,
                McpServer::Http(_) => Some(("http", "mcpCapabilities.http")),
                McpServer::Sse(_) => Some(("sse", "mcpCapabilities.sse")),
                _ => {
                    return Err(StackError::AgentUnsupportedCapability {
                        name: "mcpCapabilities.unknown",
                    });
                }
            };
            if let Some((capability, error_name)) = required
                && !self.supports_mcp_capability(capability)
            {
                return Err(StackError::AgentUnsupportedCapability { name: error_name });
            }
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
