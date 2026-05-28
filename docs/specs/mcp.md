# MCP Compatibility

`acp-stack` attaches configured MCP servers to ACP sessions. MCP declarations live in config, use secret refs for credentials, and are resolved when a session is created, loaded, or resumed.

## Declaration Scope

MCP declarations are runtime-wide in the initial release. Every session receives the configured servers when the selected agent and ACP SDK support session MCP configuration.

Per-session MCP declarations are unsupported. Use separate runtime configs when sessions need different MCP server sets.

## Matrix

| Server shape | Config support | Health check | Notes |
| ------------ | -------------- | ------------ | ----- |
| Stdio MCP | supported | command is executable; secret refs exist | Environment values come from the encrypted secret store. |
| HTTP MCP | supported | declaration and secret refs only | Health checks do not call remote MCP endpoints. |
| Slack MCP | preset-compatible | stdio/HTTP shape dependent | Declare the server shape and required token refs explicitly. |
| Linear MCP | preset-compatible | HTTP secret refs | Use `https://mcp.linear.app/mcp` with an authorization header ref. |
| Generic HTTP MCP | supported | declaration and secret refs only | Any HTTPS MCP endpoint can be declared with required headers. |

## Examples

HTTP:

```toml
[[mcp.servers]]
type = "http"
name = "linear"
url = "https://mcp.linear.app/mcp"
headers = [{ name = "Authorization", value_ref = "LINEAR_API_KEY" }]
```

Stdio:

```toml
[[mcp.servers]]
type = "stdio"
name = "local-tool"
command = "local-tool-mcp"
args = ["serve"]
env = ["LOCAL_TOOL_API_KEY"]
```

Store referenced secrets separately:

```sh
acps secrets set LINEAR_API_KEY
```

## Validation And Health

Config validation rejects duplicate server names, unsupported URL schemes, empty stdio commands, empty HTTP header names, and invalid secret-ref names.

`GET /v1/health/ready` reports MCP declaration health:

- stdio servers fail readiness when the command is missing, not executable, or a referenced secret is missing
- HTTP servers fail readiness when a referenced secret is missing
- HTTP health does not perform network probes

## Unsupported Features

The initial release does not support per-session MCP declarations, runtime mutation of MCP server lists without config import/restart, live remote endpoint certification, OAuth brokering, or automatic package-manager installation for MCP servers.

Workarounds:

- use separate runtime configs for different MCP server sets
- declare required stdio binaries under `[dependencies.commands]`
- store credentials with `acps secrets set`
- inspect `acps deps check`, `acps status`, and `/v1/health/ready` when MCP attachment fails
