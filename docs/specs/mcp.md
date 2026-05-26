# MCP Example

`acp-stack` can attach declared MCP servers to ACP sessions. MCP declarations live in config and use secret refs for credentials.

## HTTP Server Example

```toml
[[mcp.servers]]
type = "http"
name = "linear"
url = "https://mcp.linear.app/mcp"
headers = [{ name = "Authorization", value_ref = "LINEAR_API_KEY" }]
```

Store the secret separately:

```sh
acps secrets set LINEAR_API_KEY
```

## Stdio Server Example

```toml
[[mcp.servers]]
type = "stdio"
name = "local-tool"
command = "local-tool-mcp"
args = ["serve"]
env = ["LOCAL_TOOL_API_KEY"]
```

Stdio env refs are resolved from the encrypted secret store when a session is created, loaded, or resumed.

## Presets And Custom Servers

MCP presets are ordinary config declarations with known names and default shapes. Custom servers are supported when the operator provides the command, URL, args, env refs, and headers required by the server.

Secret values are never embedded in config export, API responses, or durable events.
