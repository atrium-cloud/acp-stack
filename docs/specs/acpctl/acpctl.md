# acpctl

`acpctl` is the constrained local interface for agents and local shell users inside an `acp-stack` instance. It uses the daemon's local Unix socket instead of the public HTTP API keys.

## Commands

```sh
acpctl status
acpctl security check
acpctl deps check
acpctl logs query --since 1h
acpctl workspace list .
acpctl workspace read README.md
acpctl workspace write path/to/file
acpctl command run "rg TODO ."
acpctl command list
acpctl command get <id>
acpctl command output <id>
acpctl command cancel <id>
acpctl config export
acpctl permissions pending
acpctl ws connections
acpctl ws sessions
acpctl mcp serve
```

## Security Boundary

`acpctl` runs as the local runtime user and is protected by filesystem permissions on the local socket. It is intentionally narrower than the public HTTP API.

Allowed capabilities:

- status and security self-check
- dependency checks
- log queries
- workspace list/read/write
- mediated shell commands
- current config export with secret refs only
- pending permission inspection
- read-only WebSocket connection/session views
- local MCP facade for the same allowlisted operations

Denied capabilities:

- read secret values
- write or rotate secrets
- import config
- install, start, stop, or restart agents
- approve or deny permission requests
- disconnect WebSocket clients
- disable permissions, rate limits, origin checks, or security logging

Every local operation is logged with `source = "local"`. Workspace and command operations still pass through the same policy and audit paths as public API requests. `workspace write` does not create parent directories.

## Allowlisted Routes

Each `acpctl` subcommand maps to exactly one UDS route. Off-allowlist routes return 404 over the UDS — they remain reachable only through the public HTTP API behind admin-tier authentication.

| acpctl subcommand     | UDS route                     | High-risk?      |
| --------------------- | ----------------------------- | --------------- |
| `status`              | `GET /v1/status`              | no              |
| `security check`      | `GET /v1/security/check`      | no              |
| `deps check`          | `POST /v1/deps/check`         | no              |
| `logs query`          | `GET /v1/logs/events`         | no              |
| `workspace list`      | `GET /v1/files`               | no              |
| `workspace read`      | `GET /v1/files/content`       | no              |
| `workspace write`     | `PUT /v1/files/content`       | no              |
| `command run`         | `POST /v1/commands`           | mediated        |
| `command list`        | `GET /v1/commands`            | no              |
| `command get`         | `GET /v1/commands/{id}`       | no              |
| `command output`      | `GET /v1/commands/{id}/output` | no             |
| `command cancel`      | `POST /v1/commands/{id}/cancel` | mediated      |
| `config export`       | `GET /v1/config/export`       | no (refs only)  |
| `permissions pending` | `GET /v1/permissions/pending` | no (read-only)  |
| `ws connections`      | `GET /v1/ws/connections`      | no (read-only)  |
| `ws sessions`         | `GET /v1/ws/sessions`         | no (read-only)  |

Hard-blocked routes — absent from the UDS router and not registered by `acpctl mcp serve`:

| Capability                                | Off-allowlist route                                                    |
| ----------------------------------------- | ---------------------------------------------------------------------- |
| Read secret values                        | `GET /v1/secrets/{name}`                                               |
| Write or rotate secrets                   | `POST /v1/secrets`, `DELETE /v1/secrets/{name}`                        |
| Approve/deny permission requests          | `POST /v1/permissions/{id}/approve`, `.../deny`                        |
| Install / start / stop agent              | `POST /v1/agent/install`, `.../start`, `.../stop`                      |
| Import config                             | `POST /v1/config/import`                                               |
| Disconnect WebSocket clients              | `POST /v1/ws/connections/disconnect`, `.../sessions/disconnect`        |

## MCP Facade

`acpctl mcp serve` exposes the local interface as MCP tools for agents that prefer tool calls over shell commands. The MCP surface mirrors the `acpctl` allowlist, including command run/list/get/output/cancel, and does not register denied capabilities.
