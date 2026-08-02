# MCP Compatibility

`acp-stack` attaches configured MCP servers to ACP sessions. MCP declarations live in config, use secret refs for credentials, and are resolved when a session is created, loaded, resumed, or forked. Portable stdio command names are resolved to absolute executable paths before they cross the ACP boundary.

## Declaration Scope

MCP declarations are runtime-wide in the initial release. Every session receives the configured servers when the selected agent and ACP SDK support session MCP configuration. Agent switching preserves MCP declarations because they are attached through ACP sessions, not stored in agent-owned harness config.

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

HTTP with a templated header value (exactly one of `value_ref` or `value` per header; templates interpolate `${SECRET_REF}` and must contain at least one ref — see the secret-reference-template rules in [config.md](config.md)):

```toml
[[mcp.servers]]
type = "http"
name = "parallel"
url = "https://api.parallel.example/mcp"
headers = [{ name = "Authorization", value = "Bearer ${PARALLEL_API_KEY}" }]
```

Stdio (bare `NAME` entries export the whole secret under that name; `VAR=template` entries compose the value):

```toml
[[mcp.servers]]
type = "stdio"
name = "local-tool"
command = "local-tool-mcp"
args = ["serve"]
env = ["LOCAL_TOOL_API_KEY", "DATABASE_URL=postgres://user:${DB_PASS}@host/db"]
```

Store referenced secrets separately:

```sh
acps secrets set LINEAR_API_KEY
```

## Validation And Health

Config validation rejects duplicate server names, empty stdio commands, empty HTTP header names, invalid secret-ref names, malformed templates (including pure literals), headers setting both or neither of `value_ref`/`value`, and duplicate env var names within one server's `env` list. HTTP server URLs must be https, or http toward a loopback host (`127.0.0.1`, `::1`, `localhost`) — a local relay never leaves the host; URLs carrying userinfo credentials are rejected.

At daemon startup a server declaration that fails these per-server rules is skipped with a startup warning rather than failing the boot — the daemon degrades instead of bricking on one bad declaration. Declaration and config-write paths (init flags, API config writes, candidate-config validation) still reject. Cross-source rules (a whole-value ref duplicated against `agent.env`, Supabase, or workspace sources) are config-level conflicts and still fail startup. Reloads of a hand-edited config while the daemon runs drop bad declarations quietly — the warning fires at startup (or after a restart), not per reload — so a broken declaration introduced at runtime surfaces through the strict config endpoints (`/v1/config/export`, `/v1/config/validate`) until then.

Native-config import validates secret references without requiring stdio executables to be installed yet. Session attachment resolves each stdio command and fails with `config.invalid` before dispatch when it is missing or not executable. HTTP and SSE servers are attached only when the agent advertises `mcpCapabilities.http` / `mcpCapabilities.sse`; when it does not, that server is skipped for the session (recorded as an `mcp.session_skipped` session event) and the session is still created with the remaining servers.

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
