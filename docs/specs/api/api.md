# API Overview

acp-stack exposes a versioned HTTP API under `/v1`, plus WebSocket channels and a keyless local Unix socket. This document explains the contracts behind the routes: auth tiers, the response envelope, the error-code model, and lifecycle semantics. Method-by-method details live in the [Endpoint reference](endpoints.md).

Clients authenticate with a bearer API key:

```http
Authorization: Bearer <key>
```

## Auth Tiers

Three key tiers separate everyday traffic from instance control:

| Tier        | Used for                                                                        |
| ----------- | ------------------------------------------------------------------------------- |
| Session key | sessions, workspace files, mediated commands, logs, status, pending permissions |
| Admin key   | secrets, config import, agent process control, security-sensitive operations    |
| Local       | internal Unix socket used by keyless local `acps` routes                        |

### Key Lifecycle

- `acps init` creates the session and admin keys on first run, prints the plaintext once, and stores only local verifier rows.
- The session key can be rotated by an admin-authenticated daemon call.
- The admin key is regenerated only by resetting and reinitializing the instance.

Public HTTP tiering is strict. `[local].session_auth = "keyless"` only affects same-user Unix-socket access. Public session routes accept session keys only.

## Response Envelope

JSON success responses:

```json
{ "ok": true, "data": {} }
```

JSON errors:

```json
{
  "ok": false,
  "error": {
    "code": "config.invalid",
    "message": "workspace.root must be absolute",
    "details": {}
  }
}
```

### Envelope Exceptions

- Binary downloads stream raw bytes.
- WebSocket frames carry their own shapes.
- `GET /v1/health/ready` builds an envelope-shaped body itself (not a typed `ApiSuccess`, so it is not schema-covered). `ok` mirrors readiness: a `503` carries `ok: false` alongside `data` and no `error` object.

## Error Codes

Errors carry a machine-readable `code`, a human-readable `message`, and structured `details`. Codes are dotted identifiers such as `config.invalid`, `request.invalid_param`, and `agent.inference_5xx`.

### Sanitization

Public error text is sanitized:

- Cross-field validation errors name the offending field but never echo its value.
- Secret positions carrying pasted-credential shapes are rejected without echoing the value.
- The `agent.inference_*` prompt codes use the fixed message form `"inference endpoint returned <status_code> (<reason_category>)"`. No URLs, request/response bodies, headers, or secret material reach the API response or the persisted prompt row.

Route-specific codes are listed per route in the [Endpoint reference](endpoints.md), including the prompt-path error table.

## Machine-Readable Schema

A JSON Schema (draft 2020-12) of the typed contract is generated from the Rust wire types and committed at `docs/specs/api/acps-schema.json`. A `docs/specs/api/acps-schema.meta.json` sidecar carries the schema version and per-namespace definition counts.

Consumers reference it two ways:

- The raw committed file at a git ref: `https://raw.githubusercontent.com/atrium-cloud/acp-stack/<ref>/docs/specs/api/acps-schema.json`. Pin `main` for the current contract, or a tag for a reproducible one.
- The release asset `https://github.com/atrium-cloud/acp-stack/releases/latest/download/acps-schema.json`, which resolves to the newest stable release. Nightlies are prereleases, so `latest` skips them.

The document splits `$defs` into three namespaces because serde's `#[serde(default)]`/`skip_serializing_if` fields belong to `required` under one direction but not the other:

- `request` — what a client sends (deserialize contract).
- `response` — what the server emits (serialize contract).
- `config` — the `acps-config.toml` shape.

A type used on both sides appears once per namespace.

Cross-field rules (mutually-required/exclusive fields, exactly-one-of headers, blank-as-absent) live in code; JSON Schema carries few of them. They are documented in these specs and in the field descriptions of the highest-traffic request bodies rather than duplicated as schema logic.

The few that map cleanly to a structural keyword are emitted (e.g. the config file's at-least-one-of `agent`/`array` sections as an `anyOf`), with the finer per-field checks still in the loader.

Not covered by the schema, by design:

- WebSocket frames (`/v1/ws` `LiveEvent` and the init streaming frames — hand-built and byte-pinned).
- The envelope-bypassing binary download handler and the hand-built `health/ready` handler.
- The untyped `config` import response.

Coverage of the typed handler surface is verified in CI (`cargo run --features dev-tools --bin generate-api-schema -- --coverage`). Regenerate after any DTO change with `cargo run --features dev-tools --bin generate-api-schema`.

## Bootstrap Init API

`acps init serve` is a bootstrap server for hosted instance setup. It mounts only the init routes; normal session/admin `/v1` routes are absent. Calls authenticate with a single bootstrap token that comes from process input, not config or state.

### Session Model

- One active init session at a time. A second create returns `409 init.session_active`.
- Clients declare setup intent up front in the create body (agent, provider, workspace, environment, update policies). The interactive wizard is never streamed; only prompts the declaration did not settle stream back.
- Secret values never travel in the request body. Missing refs are collected over the prompt stream, and never appear in status or event replay.
- The create body may also declare `extensions` (for example the managed-state namespace that credential deposits target, or a network-provider egress declaration). Declarations stage into a freshly-created starter config before any tracked step runs, and a network-provider declaration routes every sandboxed init phase through the egress provider from the start. A companion `sandbox_mask_paths` array unions absolute paths into the starter config's sandbox mask set, so the provider's egress dirs stay unreadable to the sandboxed agent from the first spawn.
- Progress is structural: the server emits `signal` frames (step started/finished, category applicable/settled/failed), and the client folds them into a category view. `hello` and the status body carry the full `signals` replay, so a late or reconnecting client folds the same input as a full-stream client.
- Prompt answers arrive over the WebSocket `input` frame or its REST twin `POST /v1/init/sessions/{id}/input`, interchangeably. The bootstrap server also mounts the session-tier `GET /v1/models` (with `?target_id=` target selection), so a backend renders model/mode/effort pickers while init runs.

### Session Lifecycle

- After `result`, the session waits in `completed_awaiting_ack`. `ack_result` is terminal: the server clears the in-memory handoff payload, closes the session, and exits successfully.
- A failure after key handover still delivers a `result` frame through the same ack path.
- A failure before key handover parks the session in `errored` and keeps the server up, so the backend learns the typed failure instead of a dead port. `ack_error` releases the server, which exits non-zero. A 2-minute grace (reason `error_ack_timeout`) bounds the wait.
- Abandoned sessions self-terminate: after `--idle-timeout` (default `15m`) with no connected WebSocket and no API activity, or once `--max-lifetime` elapses, the session is cancelled and the process exits non-zero. A WebSocket disconnect restarts the idle clock.

The full frame vocabulary, signal fold rules, request body fields, and timeout reasons are in the [Endpoint reference](endpoints.md#bootstrap-init). See [init.md](../init.md) for the init flow itself.

## Agent And Providers

These routes control the supervised agent process and its provider configuration. The design points:

- Process control (install, start, stop, restart, switch) is admin-tier; status and discovery reads are session-tier.
- Responses sanitize provider state. Provider records contain only provider id, selected alias, and emitted env names. When provider resolution fails, status still returns with a remote-safe `provider_error` message, so monitoring stays reachable in the broken state.
- Guarded restart: `require_idle=true` returns active-session blockers instead of restarting; `auto=true` queues the restart until they clear.
- Agent switch is journaled at `agent-switch.json` beside the canonical config, so retries converge instead of failing as "already configured". Same-target retries resume or no-op depending on the journaled phase; a fingerprint mismatch or a different target fails rather than compounding the in-flight switch.
- Skills routes act on the active agent and distinguish managed skills (installed by acp-stack, with a marker) from hand-placed ones, which `remove` refuses to delete.
- The managed update routes mirror the auto-update timer. A running agent is never touched; the route reports `skipped: true` so callers can retry safely.

Route shapes and error codes are in the [Endpoint reference](endpoints.md#agent-and-providers).

## Sessions, Prompts, And Reconnect

Lifecycle semantics that span routes:

- A session's durable `status` is `active`, `available`, or `closed`. The windowed status route derives a separate per-row `state` (`idle`, `working`, `done`, and so on) from recent activity.
- Prompt statuses are `pending`, `running`, `completed`, `errored`, `cancelled`, and `stalled`. `stalled` is terminal: the stale-prompt sweeper writes it after `[prompts].stale_threshold` with no ACP activity, and the prompt never returns to `running`. Recovery means submitting a new prompt.
- Session close preserves history; only `POST /v1/sessions/{id}/delete` hard-deletes, and only when the agent advertises the capability.
- Declared config the agent does not advertise (mode, model, effort, config options) never fails session creation. The session proceeds on agent defaults and the response reports the omission in an `ignored` array.

Reconnect patterns follow one shape: read once, subscribe, then catch up from a cursor.

- Sessions: `GET /v1/sessions/{id}/snapshot`, subscribe to `sessions.{id}`, then `GET events?after=last_event_id`.
- Commands: read the command record, subscribe to `commands.{id}`, then page `/output?order=asc&after=<last-seen-event-id>`.

The `changes` file-diff snapshot is bounded and process-local. It lives only for the process lifetime; durable event history is the restart-safe record.

## HTTP Hardening

The API enforces:

- bearer auth
- request-size limits
- origin checks — disallowed browser origins return `403 auth.origin_not_allowed`
- rate limits
- auth-failure blocking
- bounded proxy-header trust

Oversized JSON requests return `413 request.too_large`.
