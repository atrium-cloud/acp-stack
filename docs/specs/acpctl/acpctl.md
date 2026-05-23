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
acpctl ws connections
acpctl ws sessions
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

`acpctl` exposes read-only WebSocket reporting so agents can describe current client/session state in responses:

```sh
acpctl ws connections
acpctl ws sessions
```

The sanitized view may include connection id, age, topics, derived subscribed session ids, origin kind, coarse Cloudflare country/region metadata, and last activity. It must not expose raw IP addresses, raw `Origin`, raw `User-Agent`, Cloudflare credentials, or disconnect controls. Matching `acpctl mcp serve` tools should mirror only the two view operations.

## Per-command Permission Boundary

The table below is the source of truth for `acpctl`'s direct command permission audit. Every direct route-backed CLI subcommand maps to exactly one allowlisted UDS route, and every off-allowlist HTTP route returns 404 over the UDS (so it cannot be reached even if a future operator misconfigures the socket). `acpctl mcp serve` is the one non-direct command: it starts a tool facade, and each tool call maps to one of the same allowlisted routes documented below. Routes in the right column form the hard-blocked deny list; they are absent from `src/local_listener/router.rs::build_local_router` and remain reachable only through the public HTTP API behind admin-tier authentication.

| acpctl subcommand | UDS route | Permission tier | High-risk? | Audit source |
| ----------------- | --------- | --------------- | ---------- | ------------ |
| `acpctl status` | `GET /v1/status` | local | no | `api.request source=local` |
| `acpctl security check` | `GET /v1/security/check` | local | no | `api.request source=local` |
| `acpctl deps check` | `POST /v1/deps/check` | local | no | `api.request source=local` |
| `acpctl logs query` | `GET /v1/logs/events` | local | no | `api.request source=local` |
| `acpctl workspace list` | `GET /v1/files` | local | no | `api.request source=local` |
| `acpctl workspace read` | `GET /v1/files/content` | local | no | `api.request source=local` |
| `acpctl workspace write` | `PUT /v1/files/content` | local | no | `api.request source=local`, `workspace.write` event |
| `acpctl command run` | `POST /v1/commands` | local | mediated | `api.request source=local`, command-gateway events |
| `acpctl config export` | `GET /v1/config/export` | local | no (refs only) | `api.request source=local` |
| `acpctl permissions pending` | `GET /v1/permissions/pending` | local (read-only) | no | `api.request source=local` |
| `acpctl ws connections` | `GET /v1/ws/connections` | local (read-only) | no | `api.request source=local` |
| `acpctl ws sessions` | `GET /v1/ws/sessions` | local (read-only) | no | `api.request source=local` |

`acpctl mcp serve` exposes exactly twelve tools matching the direct allowlist: `status`, `security_check`, `deps_check`, `logs_query`, `workspace_list`, `workspace_read`, `workspace_write`, `command_run`, `config_export`, `permissions_pending`, `ws_connections`, and `ws_sessions`. The MCP dispatcher sends every tool call through the existing `acpctl.sock` UDS client, so the same router allowlist, `KeyKind::Local` tagging, and `api.request source=local` audit rows apply. Denied capabilities are not registered as MCP tools.

| Capability denied to `acpctl` | Off-allowlist route | Enforcement |
| ----------------------------- | ------------------- | ----------- |
| Read secret values | `GET /v1/secrets/{name}` | 404 (route absent) |
| Write/rotate secrets, incl. API keys | `POST /v1/secrets`, `DELETE /v1/secrets/{name}` | 404 (route absent) |
| Approve own permission requests | `POST /v1/permissions/{id}/approve` | 404 (route absent) |
| Deny permission requests | `POST /v1/permissions/{id}/deny` | 404 (route absent) |
| Install agent | `POST /v1/agent/install` | 404 (route absent) |
| Start/stop the agent process | `POST /v1/agent/start`, `POST /v1/agent/stop` | 404 (route absent) |
| Import config | `POST /v1/config/import` | 404 (route absent) |
| Disconnect WebSocket clients | `POST /v1/ws/connections/disconnect`, `POST /v1/ws/sessions/disconnect` | 404 (route absent) |
| Toggle permissions, rate limits, CORS, security logging | configuration mutation | no exposed route |

A change to either table must be paired with a matching change in `src/local_listener/router.rs::build_local_router` and the deny-list assertions in `tests/acpctl_tests.rs`. Drift between this table, the router allowlist, and the test deny-list is treated as a P0 spec violation.

## Implementation (Phase 3)

The Phase 3 implementation realizes this surface as a `tokio` Unix-domain-socket listener inside the `acps` daemon (`src/local_listener.rs`, with router/socket internals under `src/local_listener/`). When `acps serve` starts it binds the socket at `~/.local/share/acp-stack/acpctl.sock` (override with `[acpctl] socket_path` in the TOML), sets the file mode to `0600` inside a `0700` parent directory, and unlinks the socket on graceful shutdown. The listener serves an Axum router that mounts an explicit allowlist of local operations; any other route returns 404, including the public-API routes for secret values, API-key rotation, permission approve/deny, config import, agent install/start/stop, and WebSocket disconnect. A `tag_local` middleware stamps every UDS request with `KeyKind::Local` so the public router's tier gate rejects the tag if it ever leaks across listeners, and reused handlers attribute durable writes (`api.request`, `workspace.write`, etc.) to `source = "local"`. The `acpctl` binary entrypoint (`src/bin/acpctl/main.rs`) speaks HTTP/1.1 over the socket through helper modules under `src/bin/acpctl/` and forwards each subcommand to its mapped route; it sends no `Authorization` header — filesystem permissions on the socket are the access control.

The optional `acpctl mcp serve` command exposes the local introspection interface as an MCP server for agents that prefer tool calls over shell commands. The MCP server must enforce the same capability, permission, and logging rules as the CLI.

The durable audit record for every `acpctl` invocation is the per-completed-request `api.request` event written by the shared `log_api_request` middleware (`src/api/auth.rs`). Each row carries `{method, path, status, duration_ms, key_kind}` and the row's `source` column is `local` for UDS-driven calls. This is the same audit emission used by the public HTTP API; the cardinality skip that suppresses high-rate `/v1/status*` and `/v1/ws` rows is bypassed for `source = "local"` so every acpctl call is recorded regardless of route. No separate `audit` table or `acpctl.action` event kind is needed; the unified `events` log is the audit trail. The `tests/acpctl_tests.rs` integration suite asserts both halves of this contract: a positive `source = local` row per invocation, and a 404 response for every off-allowlist high-risk route (secrets read/write/delete, config import, agent install/start/stop, permissions approve/deny).

The Phase 3 implementation lives under `src/bin/acpctl/mcp/` and is built on the `rmcp` crate. It supports two transports: `--transport stdio` (default), where the agent spawns `acpctl mcp serve` as a child process and exchanges JSON-RPC over stdin/stdout, and `--transport http-uds`, which binds a streamable-HTTP MCP endpoint on a Unix-domain socket (default `~/.local/share/acp-stack/acpctl-mcp.sock`, 0600 inside a 0700 parent) for MCP clients that dial `unix:` URLs. In both modes the server registers tools mirroring the UDS allowlist — `status`, `security_check`, `deps_check`, `logs_query`, `workspace_list`, `workspace_read`, `workspace_write`, `command_run`, `config_export`, `permissions_pending`, `ws_connections`, and `ws_sessions` — and every tool call is translated to an HTTP/1.1 request against the daemon's existing local UDS (`acpctl.sock`) using the same client module as the `acpctl` CLI. Because every call rides the existing local listener, capability enforcement (filesystem perms on the parent socket), the `KeyKind::Local` request stamp, and the `source = "local"` event attribution flow through unchanged: no parallel permission or logging code is introduced for the MCP surface. The deny list — secrets, key rotation, permission approve/deny, config import, agent install/start/stop, WebSocket disconnect, security/rate-limit/origin/logging toggles — is enforced by absence: the UDS router never mounts those routes, and the MCP tool registry never names them.
