# Security

`acp-stack` treats local instance integrity as part of the product contract. The runtime fails fast on unsafe config and keeps secret values out of config, responses, and logs.

## API Keys

Two API keys are generated on first init:

| Key     | Scope                                                                   |
| ------- | ----------------------------------------------------------------------- |
| Session | normal sessions, workspace, commands, logs, status, pending permissions |
| Admin   | secrets, config import, agent process control, and sensitive operations |

The session key can be regenerated. The admin key is generated once and is replaced only by resetting and reinitializing the instance.

## Key Tiering

Tiering is strict and non-superset: the admin key is rejected on session-tier routes with `401 auth.wrong_kind`, and the session key is rejected on admin-tier routes with the same code. The admin key is not a superset of the session key.

Session-tier routes cover everything that is not management or destructive: status reads, config export, log queries, workspace operations, command runs, session and prompt lifecycle, and permission approve/deny. Session operations stay session-tier even when they write rows.

Both keys are presented as `Authorization: Bearer <key>` and compared against stored values in constant time.

## Secret Store

Secret values are stored in the encrypted local secret store. Config files carry secret reference names only.

Rules:

- API responses never return secret values.
- Config export returns refs only.
- Agent and MCP secrets are injected only where explicitly referenced.
- Secret-ref fields reject likely pasted secret values.

## HTTP Hardening

The public API enforces:

- bearer authentication
- auth tier checks
- CORS and WebSocket Origin allowlists
- request body limits
- per-key and per-IP rate limits
- temporary blocking after repeated auth failures
- bounded trusted-proxy handling
- security event logging

`trust_proxy_headers = true` accepts forwarded client metadata only from exact IPs listed in `trusted_proxies`. Do not trust broad public ranges.

## Auth Failures And Rate Limits

Every rejected authentication is recorded in the `auth_failures` table with a structural `reason` (`missing`, `malformed_header`, `invalid`, `wrong_kind`). The attempted token value is never stored. After `auth_failures_per_minute` rejections in a 60-second window, the client IP is blocked for `auth_block_duration`.

Hardening errors use stable codes in the standard error envelope:

| Status | Code                       | Trigger                                          |
| ------ | -------------------------- | ------------------------------------------------ |
| `401`  | `auth.wrong_kind`          | key valid but used on the wrong tier             |
| `429`  | `auth.rate_limited`        | per-IP, per-key, or unauthenticated bucket empty |
| `403`  | `auth.origin_not_allowed`  | CORS / WebSocket Origin not in allowlist         |
| `413`  | `request.too_large`        | request body exceeds the configured cap          |

## Permissions

Permission policy applies to ACP permission requests and mediated shell commands.

| Policy input      | Behavior                                   |
| ----------------- | ------------------------------------------ |
| `deny` match      | reject immediately                         |
| `review` match    | create a permission request or audit event |
| `auto` mode       | allow unmatched requests                   |
| `supervised` mode | require approval for unmatched risky work  |
| `locked` mode     | require approval for unmatched commands    |

Pending requests expire according to config. Approval and denial decisions are durable events.

## Workspace Boundary

Workspace paths are resolved under `[workspace].root`. The runtime rejects absolute paths from API callers, `..` traversal, embedded NUL bytes, symlink escapes, writes through existing symlink targets, and files above `workspace.max_file_bytes`. Oversized reads/writes/uploads/downloads return `413 workspace.too_large`.

## Local Interface

`acpctl` uses a local Unix socket protected by filesystem permissions. It does not use the public session or admin API keys. The local surface is allowlisted and cannot read secret values, rotate keys, import config, approve its own high-risk requests, or control public WebSocket disconnections.

## Deployment Posture

Production deployments should:

- run as an unprivileged runtime user
- keep config and state directories owner-only
- bind the daemon to loopback unless a trusted platform requires otherwise
- terminate TLS at a reverse proxy or Cloudflare Tunnel
- keep runtime auth and origin checks enabled behind the edge

## Security Self-Check

`GET /v1/security/check` and `acps security check` report findings for common misconfiguration: unsafe binds, wildcard browser origins, weak or missing cached keys, excessive auth failures, loose file modes, ownership mismatches, unwritable workspaces, and external logging delivery failures.

Findings include severity, code, message, and remediation when an operator action is available.
