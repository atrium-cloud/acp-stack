# MCP Example

This document provides a worked example for declaring a Linear MCP server in `acp-stack`. It complements:

- [config.md](config.md) — `[[mcp.servers]]` schema and validation rules
- [security.md](security.md) — secret-reference semantics for stdio `env` and HTTP `headers`
- [acp/acp-bridge.md](acp/acp-bridge.md) — when servers attach to a session and how the bridge forwards them

The example below shows the TOML block, the `acps secrets set` call that populates the referenced secret, and the upstream prerequisite. Every `[[mcp.servers]]` `name` may also be cross-referenced from `[dependencies.mcp]` so the runtime reports declaration status via `GET /v1/deps` and `acps deps check`.

`acp-stack` is a headless Linux runtime. MCP setups that require OAuth user flows or browser-based consent are out of scope; this HTTP MCP example uses a Bearer token instead.

## Presets And Custom Servers

MCP support has two paths:

- curated presets for a narrow set of frequently used servers
- custom server declarations written directly in config

Presets are convenience templates, not an allowlist. Users can always declare their own specific stdio or HTTP MCP servers as long as the declaration is headless-compatible and uses explicit secret references.

The curated preset docs should cite their upstream sources. The `modelcontextprotocol/servers` repository is useful for reference implementations and for links to common servers, but it is not treated as a centralized `acp-stack` catalog.

Initial curated preset work stays narrow:

- Linear HTTP - documented in this file
- Exa HTTP - documented in this file
- Slack - planned example once its headless auth shape is documented
- generic HTTP - template for user-provided HTTP MCP endpoints

Additional reference servers, such as Fetch, Filesystem, Git, Memory, and Time from `modelcontextprotocol/servers`, can be evaluated later one at a time. A reference implementation is not automatically a production-ready preset.

Custom stdio example:

```toml
[[mcp.servers]]
type = "stdio"
name = "custom-tools"
command = "custom-mcp-server"
args = ["--mode", "stdio"]
env = ["CUSTOM_MCP_API_KEY"]
```

Custom HTTP example:

```toml
[[mcp.servers]]
type = "http"
name = "custom-http"
url = "https://tools.example.com/mcp"
headers = [
  { name = "Authorization", value_ref = "CUSTOM_MCP_BEARER" },
]
```

Dependency declarations may reference either preset or custom MCP names:

```toml
[dependencies]
mcp = [{ name = "custom-tools" }, { name = "custom-http" }]
```

No MCP server receives implicit access to the secret store. Stdio servers get only the env names listed on that server. HTTP servers get only the configured header values resolved from their `value_ref` names.

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

## Exa (HTTP)

Exa hosts a remote MCP server at `https://mcp.exa.ai/mcp` using streamable HTTP transport. The server reads an API key from the `x-api-key` header, which fits a headless deployment without OAuth.

```toml
[[mcp.servers]]
type = "http"
name = "exa"
url = "https://mcp.exa.ai/mcp"
headers = [
  { name = "x-api-key", value_ref = "EXA_API_KEY" },
]
```

Populate the secret:

```sh
acps secrets set EXA_API_KEY
```

The header value is interpolated verbatim, so the stored secret is the raw API key with no prefix. Keys come from the Exa dashboard. The server enables `web_search_exa` (general web search returning ready-to-use content) and `web_fetch_exa` (retrieves URLs as clean markdown) by default; opt-in tools are selected through the `?tools=` query string on the URL when needed.

The remote endpoint and authentication options above are verified against upstream as of 2026-05-17.

## How servers attach to sessions

All `[[mcp.servers]]` entries are resolved and forwarded to the configured agent at session create, load, and resume. There is no per-agent allowlist: `acp-stack` is a single-agent runtime, and every declared server is offered to that agent. Agents that do not advertise MCP capability ignore the list.

Secret values are read from the encrypted store at attach time and passed to the agent in memory. They never enter SQLite, the durable event log, API responses, or WebSocket frames. The `mcp.session_attached` event records only the server names attached to a session.

Failures during resolution (missing secret, unreadable store) surface as session-create errors rather than silent omissions; see `src/mcp.rs` for the resolver and `src/api.rs` for the call site.
