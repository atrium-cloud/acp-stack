# MCP Example

This document provides a worked example for declaring a Linear MCP server in `acp-stack`. It complements:

- [config.md](config.md) — `[[mcp.servers]]` schema and validation rules
- [security.md](security.md) — secret-reference semantics for stdio `env` and HTTP `headers`
- [acp/acp-bridge.md](acp/acp-bridge.md) — when servers attach to a session and how the bridge forwards them

The example below shows the TOML block, the `acps secrets set` call that populates the referenced secret, and the upstream prerequisite. Every `[[mcp.servers]]` `name` may also be cross-referenced from `[dependencies.mcp]` so the runtime reports declaration status via `GET /v1/deps` and `acps deps check`.

`acp-stack` is a headless Linux runtime. MCP setups that require OAuth user flows or browser-based consent are out of scope; this HTTP MCP example uses a Bearer token instead.

## Linear (HTTP)

Linear hosts a remote MCP server at `https://mcp.linear.app/mcp` using streamable HTTP transport. The server accepts a Bearer token directly in the `Authorization` header, which fits a headless deployment.

```toml
[[mcp.servers]]
type = "http"
name = "linear"
url = "https://mcp.linear.app/mcp"
headers = [
  { name = "Authorization", value_ref = "LINEAR_API_KEY" },
]
```

Populate the secret:

```sh
acps secrets set LINEAR_API_KEY
```

The header value is interpolated verbatim, so the stored secret must include the `Bearer ` prefix — for example `Bearer lin_api_...`. The token itself can be either a Linear personal API key (long-lived, scoped to one user) or an OAuth2 `client_credentials` app-actor token (valid for 30 days, scoped to all public teams in the workspace). The interactive OAuth user flow is intentionally not used here because it requires a browser callback that the runtime cannot satisfy.

The remote endpoint and authentication options above are verified against upstream as of 2026-05-15.

## How servers attach to sessions

All `[[mcp.servers]]` entries are resolved and forwarded to the configured agent at session create, load, and resume. There is no per-agent allowlist: `acp-stack` is a single-agent runtime, and every declared server is offered to that agent. Agents that do not advertise MCP capability ignore the list.

Secret values are read from the encrypted store at attach time and passed to the agent in memory. They never enter SQLite, the durable event log, API responses, or WebSocket frames. The `mcp.session_attached` event records only the server names attached to a session.

Failures during resolution (missing secret, unreadable store) surface as session-create errors rather than silent omissions; see `src/mcp.rs` for the resolver and `src/api.rs` for the call site.
