# acpctl Spec

`acpctl` is the local, agent-facing control interface for runtime inspection and constrained local operations. It shares core services with the daemon and HTTP API instead of creating a separate behavior path.

## Local Agent CLI

`acpctl` is the local, agent-facing control CLI installed on the instance. It exposes a constrained subset of runtime operations for agents and local shell users without requiring the public HTTP session/admin API keys.

Example commands:

```sh
acpctl status
acpctl security check
acpctl deps check
acpctl logs query --since 1h
acpctl workspace list .
acpctl workspace read README.md
acpctl command run "rg TODO ."
acpctl config export
acpctl permissions pending
acpctl mcp serve
```

`acpctl` should call the same core service layer as the daemon and HTTP API. It is not a separate implementation path.

## Local Introspection Interface

Phase 3 adds `acpctl`, a local interface intended for agents running inside the instance.

The local interface exists so an agent can inspect and operate on the runtime safely:

- check runtime status
- inspect dependency status
- query recent logs
- run the security self-check
- list/read/write workspace files
- run mediated shell commands
- export config with secret references only
- inspect pending permission requests
- expose the same surface as a local MCP server through `acpctl mcp serve`

Security boundary:

- `acpctl` runs as the unprivileged runtime user.
- `acpctl` uses a local capability mechanism, such as a Unix socket or owner-only local token, not the public session/admin API keys.
- all `acpctl` actions go through the same permission pipeline as HTTP-triggered actions.
- all `acpctl` actions are logged with source `local`.
- agents cannot read secret values through `acpctl`.
- agents cannot rotate API keys through `acpctl`.
- agents cannot disable permissions, rate limits, CORS/origin checks, or security logging.
- agents cannot approve their own high-risk command requests unless policy explicitly allows it.
- agents cannot disconnect WebSocket clients; disconnect authority is admin-only through `acps`.
- config export through `acpctl` follows normal export rules and includes secret references only.

Phase 5 should expose read-only WebSocket reporting through `acpctl` so agents can describe current client/session state in responses:

```sh
acpctl connections ws list
acpctl connections ws sessions
```

The sanitized view may include connection id, age, topics, derived subscribed session ids, origin kind, coarse Cloudflare country/region metadata, and last activity. It must not expose raw IP addresses, raw `Origin`, raw `User-Agent`, Cloudflare credentials, or disconnect controls. Matching `acpctl mcp serve` tools should mirror only the two view operations.

## Implementation (Phase 3)

The Phase 3 implementation realizes this surface as a `tokio` Unix-domain-socket listener inside the `acps` daemon (`src/local_listener.rs`, with router/socket internals under `src/local_listener/`). When `acps serve` starts it binds the socket at `~/.local/share/acp-stack/acpctl.sock` (override with `[acpctl] socket_path` in the TOML), sets the file mode to `0600` inside a `0700` parent directory, and unlinks the socket on graceful shutdown. The listener serves an Axum router that mounts an explicit allowlist of the ten operations above; any other route returns 404, including the public-API routes for secret values, API-key rotation, permission approve/deny, config import, and agent install/start/stop. A `tag_local` middleware stamps every UDS request with `KeyKind::Local` so the public router's tier gate rejects the tag if it ever leaks across listeners, and reused handlers attribute durable writes (`api.request`, `workspace.write`, etc.) to `source = "local"`. The `acpctl` binary entrypoint (`src/bin/acpctl/main.rs`) speaks HTTP/1.1 over the socket through helper modules under `src/bin/acpctl/` and forwards each subcommand to its mapped route; it sends no `Authorization` header — filesystem permissions on the socket are the access control.

The optional `acpctl mcp serve` command exposes the local introspection interface as an MCP server for agents that prefer tool calls over shell commands. The MCP server must enforce the same capability, permission, and logging rules as the CLI.

The durable audit record for every `acpctl` invocation is the per-completed-request `api.request` event written by the shared `log_api_request` middleware (`src/api/auth.rs`). Each row carries `{method, path, status, duration_ms, key_kind}` and the row's `source` column is `local` for UDS-driven calls. This is the same audit emission used by the public HTTP API; the cardinality skip that suppresses high-rate `/v1/status*` and `/v1/ws` rows is bypassed for `source = "local"` so every acpctl call is recorded regardless of route. No separate `audit` table or `acpctl.action` event kind is needed; the unified `events` log is the audit trail. The `tests/acpctl_tests.rs` integration suite asserts both halves of this contract: a positive `source = local` row per invocation, and a 404 response for every off-allowlist high-risk route (secrets read/write/delete, config import, agent install/start/stop, permissions approve/deny).

The Phase 3 implementation lives under `src/bin/acpctl/mcp/` and is built on the `rmcp` crate. It supports two transports: `--transport stdio` (default), where the agent spawns `acpctl mcp serve` as a child process and exchanges JSON-RPC over stdin/stdout, and `--transport http-uds`, which binds a streamable-HTTP MCP endpoint on a Unix-domain socket (default `~/.local/share/acp-stack/acpctl-mcp.sock`, 0600 inside a 0700 parent) for MCP clients that dial `unix:` URLs. In both modes the server registers exactly ten tools mirroring the UDS allowlist — `status`, `security_check`, `deps_check`, `logs_query`, `workspace_list`, `workspace_read`, `workspace_write`, `command_run`, `config_export`, `permissions_pending` — and every tool call is translated to an HTTP/1.1 request against the daemon's existing local UDS (`acpctl.sock`) using the same client module as the `acpctl` CLI. Because every call rides the existing local listener, capability enforcement (filesystem perms on the parent socket), the `KeyKind::Local` request stamp, and the `source = "local"` event attribution flow through unchanged: no parallel permission or logging code is introduced for the MCP surface. The deny list — secrets, key rotation, permission approve/deny, config import, agent install/start/stop, security/rate-limit/origin/logging toggles — is enforced by absence: the UDS router never mounts those routes, and the MCP tool registry never names them.
