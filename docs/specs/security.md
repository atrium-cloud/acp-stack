# Security

`acp-stack` treats local instance integrity as part of the product contract. The runtime fails fast on unsafe config and keeps secret values out of config, responses, and logs.

## API Keys

Two API keys are generated on first init:

| Key     | Scope                                                                   |
| ------- | ----------------------------------------------------------------------- |
| Session | public session-tier API calls, including session lifecycle and prompts   |
| Admin   | secrets, config import, agent process control, and sensitive operations |

The session key can be regenerated. The admin key is generated once and is replaced only by resetting and reinitializing the instance.

Plaintext auth keys are printed only at init or session-key regeneration time. Local state stores non-recoverable verifier rows for the session and admin keys; config and `secrets.age` do not store auth keys.

## Key Tiering

Tiering is strict and non-superset: the admin key is rejected on session-tier routes with `401 auth.wrong_kind`, and the session key is rejected on admin-tier routes with the same code. The admin key is not a superset of the session key.

Session-tier routes cover public API operations that are not management or destructive: config export, workspace operations, command runs, session and prompt lifecycle, and permission approve/deny. Session operations stay session-tier even when they write rows.

`[local].session_auth = "keyless"` is a local Unix-socket exception, not a public API tier change. When enabled, same-user local callers can reach session-tier HTTP routes without bearer auth through `acps`; public HTTP routes still require the session key and still reject the admin key.

Both keys are presented as `Authorization: Bearer <key>` and validated against stored verifiers in constant time.

## Secret Store

Secret values are stored in the encrypted local secret store. Config files carry secret reference names only.

Rules:

- API responses never return secret values.
- Config export returns refs only.
- Agent and MCP secrets are injected only where explicitly referenced.
- Secret-ref fields reject likely pasted secret values.
- Auth keys are not secret-store entries.

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

Shell command policy matches both raw and shell-word-normalized command forms. Constructed command words require review when no deny or review pattern matches.

Pending requests expire according to config. Approval and denial decisions are durable events.

## Workspace Boundary

Workspace paths are resolved under `[workspace].root`. The runtime rejects absolute paths from API callers, `..` traversal, embedded NUL bytes, symlink escapes, writes through existing symlink targets, and files above `workspace.max_file_bytes`. Oversized reads/writes/uploads/downloads return `413 workspace.too_large`.

## Local Interface

Keyless local `acps` views use a local Unix socket protected by filesystem permissions. Low-risk observability routes are always available without public session or admin API keys. When `[local].session_auth = "keyless"` is enabled by an admin, same-user local callers can also reach session-tier HTTP routes without bearer auth. Admin-tier operations remain unavailable on the local socket: callers cannot read secret values, rotate keys, import config, apply dependencies, or control public WebSocket disconnections locally.

## Deployment Posture

Production deployments should:

- run as an unprivileged runtime user
- configure `[workspace].runtime_user` to a local user that resolves on the host
- keep config and state directories owner-only
- bind the daemon to loopback unless a trusted platform requires otherwise
- terminate TLS at a reverse proxy or Cloudflare Tunnel
- keep runtime auth and origin checks enabled behind the edge

## Security Self-Check

The admin-tier public `GET /v1/security/check` route and keyless local `acps security check` diagnostic report findings for common misconfiguration: unsafe binds, wildcard browser origins, excessive auth failures, loose file modes, ownership mismatches, unwritable workspaces, unavailable required dependencies, and external logging delivery failures.

Findings include severity (`warning` or `critical`), code, message, an optional structured `details` payload for findings with machine-readable context, and remediation when an operator action is available.

### History

Every self-check invocation through `GET /v1/security/check` is persisted into the `security_runs` and `security_findings` tables in the local state database. The check response includes the generated `run_id` so operators can correlate the live response with the durable row. Runs are kept indefinitely; pruning is left to future operations work.

- `GET /v1/security/history?limit=N&after=<run-id>` (admin tier) returns recent runs newest-first with aggregate counts and a `next_cursor` for keyset pagination. `limit` defaults to 20 and is capped at 500.
- `GET /v1/security/history/{run_id}` (admin tier) returns a single run with its findings in emit order, replaying exactly what `acps security check` produced.
- `acps security history [--limit N] [--after <id>] [--json]` prints the operator table or raw JSON.
- `acps security show <run-id> [--json]` prints the run summary plus its findings.

Aggregate run status is `succeeded` when no critical findings were emitted and `failed` otherwise; the orthogonal `ok` boolean is true only when neither warnings nor critical findings were emitted.

### Finding categories and remediation coverage

Every emitted finding carries a non-empty remediation. The category-to-code map for the operator-facing self-check is:

- key: `auth.failure_threshold`
- file permission: `runtime.path_ownership`, `runtime.path_mode_loose`, `runtime.path_uninspectable`, `runtime.workspace_not_writable`
- origin and CORS: `http.wildcard_origin_public_bind`, `edge.cloudflare.unsafe_origins`
- proxy: `http.trust_proxy_without_trusted_proxies`, `edge.cloudflare.missing_local_trusted_proxies`
- sink: `logging.supabase.delivery_failing`
- deps: `deps.required_unavailable`
- runtime user: `runtime.user_mismatch`
- bind: `api.public_bind`, `edge.cloudflare.public_bind_tunnel`, `edge.cloudflare.cloudflared_missing`, `edge.cloudflare.headers_missing`, `edge.cloudflare.direct_public_requests`

`deps.required_unavailable` is emitted when required dependency declarations are unavailable. Details include a bounded list of dependency names, kinds, features, and reasons; the complete report remains available from `acps deps check`.
