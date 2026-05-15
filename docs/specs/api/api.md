# API Spec

All public routes are versioned under `/v1`. This document owns the HTTP and WebSocket contracts for clients. For project-level runtime behavior, see [Project Spec](../project-spec.md).

## Unified API

All routes are versioned under `/v1`.

Auth uses bearer API keys:

```http
Authorization: Bearer <key>
```

`acps init` generates both API keys on first run and stores them in the age-encrypted secret store under the names declared by `[auth].session_key_ref` and `[auth].admin_key_ref`. Keys are formatted as `acps_<43-char base64url>` (32 bytes of system CSPRNG output).

- **Session key** - general operations: agent sessions, workspace files, mediated commands, logs, and pending permission reads. This key can be regenerated using `acps auth regenerate-session-key`.
- **Admin key** - elevated operations: secrets, config import, security-sensitive status, session-key regeneration, and policy changes. This key is generated only once during init and is never regenerable in place; use `acps reset --yes` to wipe the instance and re-init if the admin key is lost or compromised.

### Response Envelope

Successful responses:

```json
{
  "ok": true,
  "data": {}
}
```

Errors:

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

### Config API

- `GET /v1/config/export` - returns canonical TOML.
- `POST /v1/config/import` - validates and applies TOML.
- `POST /v1/config/validate` - validates TOML without applying.

### Agent API

- `POST /v1/agent/install`
- `POST /v1/agent/start`
- `POST /v1/agent/stop`
- `GET /v1/agent/status`
- `GET /v1/agent/capabilities`

### Session API

- `POST /v1/sessions`
- `GET /v1/sessions`
- `GET /v1/sessions/{id}`
- `POST /v1/sessions/{id}/load`
- `POST /v1/sessions/{id}/resume`
- `POST /v1/sessions/{id}/prompt`
- `POST /v1/sessions/{id}/cancel`
- `DELETE /v1/sessions/{id}`
- `GET /v1/sessions/{id}/prompts/{prompt_id}`
- `GET /v1/sessions/{id}/events`

These map to ACP session methods where supported by the configured agent.

`POST /v1/sessions/{id}/prompt` is fire-and-forget: it enqueues the prompt on
a background task and returns `{ prompt_id, status }` immediately. Clients may
subscribe to `sessions.{id}` on `/v1/ws` for live ACP `session/update` fanout,
poll `GET /v1/sessions/{id}/prompts/{prompt_id}` for terminal prompt state
(`completed`, `errored`, or `cancelled`), and poll
`GET /v1/sessions/{id}/events?after=<event_id>&limit=<n>` for durable
session-scoped history.

### Workspace API

Session-tier. All request paths are workspace-relative (rooted at `workspace.root`, no leading `/`). The empty string and `.` mean the workspace root itself. The workspace API rejects path traversal (`..`), embedded NUL bytes, absolute paths, and symlinks that escape `workspace.root`. Path resolution canonicalizes intermediate symlinks; reads through symlinks that still resolve inside the root are allowed. Writes additionally refuse to overwrite an existing symlink at the target.

`workspace.max_file_bytes` (config) bounds reads, writes, downloads, and uploads. Files larger than that limit return `workspace.too_large` (413).

- `GET /v1/workspace` — workspace metadata.

  Response: `{ root, uploads_path, default_shell, max_file_bytes }`.

  `uploads_path` is the workspace-relative form of `workspace.uploads`, so clients can use it directly with the file routes.

- `GET /v1/files?path=...` — list directory entries.

  Response: `{ path, entries: [{ name, kind, size, modified }] }` where `kind` is `"file" | "directory" | "symlink" | "other"`. Entries are sorted directories-first, then files, then symlinks, each group ascending by name. `size` is omitted for non-files. `modified` is RFC-3339.

- `GET /v1/files/content?path=...` — read a file.

  Response: `{ path, encoding, content, size, modified }`. `encoding` is `"utf8"` when the bytes are valid UTF-8, else `"base64"`. Files over `max_file_bytes` → 413 `workspace.too_large`.

- `PUT /v1/files/content` — write a file atomically.

  Body: `{ path, encoding, content }` with `encoding` in `"utf8" | "base64"`. Decoded byte length over `max_file_bytes` → 413 `workspace.too_large`. Response: `{ path, size, modified }`. The parent directory must exist; missing parent → 404 `workspace.not_found`. The handler writes through a sibling temp file and renames.

- `POST /v1/files/upload` — multipart upload (`multipart/form-data`).

  Required fields: `path` (text, **destination relative to `workspace.uploads`** — interpreted as the final path including the filename) and `file` (binary part). The multipart filename is echoed back as `filename` but is not used for path construction. Decoded size over `max_file_bytes` → 413. Missing parent → 404. Response: `{ path, filename, size, modified }` where `path` is workspace-relative (so it can be passed back to `/v1/files*` routes directly).

- `GET /v1/files/download?path=...` — binary stream download.

  Headers: `Content-Type: application/octet-stream`, `Content-Disposition: attachment; filename="<basename>"`, `Content-Length`. Body is the raw file bytes (not enveloped). Files over `max_file_bytes` → 413 (envelope-formatted before any bytes are sent).

- `DELETE /v1/files?path=...` — remove a regular file.

  Response: `{ path, deleted: true }`. Refuses directories with 400 `workspace.path_invalid` (no recursive removal in 0.0.1). Refuses symlinks with 400 `workspace.symlink_escape`.

Every workspace mutation (`workspace.write`, `workspace.upload`, `workspace.delete`) is written to the `events` table and fanned out on the `workspace` WebSocket topic.

### Command API

- `POST /v1/commands`
- `GET /v1/commands`
- `GET /v1/commands/{id}`
- `POST /v1/commands/{id}/cancel`

Commands launched through this API are mediated by the Command Gateway. They are logged, evaluated against policy, and streamed over WebSocket.

### Permissions API

- `GET /v1/permissions/pending`
- `GET /v1/permissions/{id}`
- `POST /v1/permissions/{id}/approve`
- `POST /v1/permissions/{id}/deny`

Permission requests can originate from:

- ACP `session/request_permission`
- `acp-stack` mediated command policy
- future runtime modules

### Secrets API

Admin key required.

- `GET /v1/secrets` - lists names and metadata only.
- `POST /v1/secrets` - adds or updates a secret.
- `DELETE /v1/secrets/{name}` - removes a secret.

Secret values are never returned.

### Dependencies API

- `GET /v1/deps` - returns declared dependencies and satisfaction status.
- `POST /v1/deps/check` - re-runs validation.

0.0.2 reports missing dependencies but does not attempt broad installation by default.

### Status, Logs, and Metrics API

- `GET /v1/status`
- `GET /v1/status/agent`
- `GET /v1/status/connections`
- `GET /v1/security/check`
- `GET /v1/logs/events`
- `GET /v1/logs/commands`
- `GET /v1/logs/permissions`
- `GET /v1/logs/security`
- `GET /v1/logs/sessions`
- `GET /v1/metrics/summary`

The 0.0.1 daemon implements the status/log/metrics subset against local config, SQLite state, and the in-process agent supervisor:

- `GET /v1/status` returns schema version, latest durable event timestamp, and server version.
- `GET /v1/status/agent` and `GET /v1/agent/status` return the configured agent identity/command, optional adapter metadata, process state, pid when running, and recent `agent_lifecycle` records.
- `GET /v1/agent/capabilities` returns the latest persisted ACP `initialize` capability snapshot plus optional adapter metadata and current process state. Before the first successful start it returns `agent.not_initialized`.
- `GET /v1/status/connections` returns the current in-process active HTTP request count.
- `GET /v1/security/check` is admin-tier and returns the current security self-check envelope. In 0.0.1 it reports findings for the effective listener bind (including `acps serve --bind` overrides), wildcard CORS on public binds, proxy-header trust without a trusted proxy allowlist, empty cached API keys, and auth-failure counts in the last minute at or above the configured threshold.
- `GET /v1/logs/events` returns durable event rows and supports `limit` plus exact `level` filtering.
- `GET /v1/logs/commands`, `GET /v1/logs/sessions`, and `GET /v1/logs/security` return rows from the corresponding SQLite tables. Security logs expose auth-failure metadata only; attempted token values are never stored or returned.
- `GET /v1/logs/permissions` returns durable events whose kind starts with `permission.` or `permissions.` until the dedicated permissions schema lands.
- `GET /v1/metrics/summary` returns local row counts for events, sessions, commands, auth failures, agent lifecycle records, installer runs, and agent capability snapshots.

## WebSocket

WebSocket endpoint:

```text
GET /v1/ws
```

The WebSocket multiplexes runtime events. Clients subscribe to topics:

- `sessions.{id}` (implemented for live ACP `session/update` events)
- `commands.{id}`
- `permissions`
- `workspace` (implemented; emits `workspace.write`, `workspace.upload`, `workspace.delete`)
- `agent`
- `status`
- `logs`

The other reserved topics do not yet have live producers.

Example client message:

```json
{
  "type": "subscribe",
  "topics": ["sessions.sess_123", "permissions"]
}
```

Example server event:

```json
{
  "type": "event",
  "id": "evt_123",
  "topic": "sessions.sess_123",
  "createdAt": "2026-05-12T00:00:00Z",
  "payload": {
    "kind": "session.update",
    "data": {
      "sessionId": "sess_123",
      "update": {
        "sessionUpdate": "agent_message_chunk",
        "content": { "type": "text", "text": "Done" }
      }
    }
  }
}
```

Every WebSocket event that represents important runtime history should also be written to SQLite.

## API Security Boundaries

The API uses the same two-key model and security controls described in [security](../security.md). Session-key and admin-key authorization boundaries are part of the public API contract, and browser-facing clients must satisfy the configured CORS and WebSocket origin policy.

### HTTP Hardening

HTTP hardening is part of the runtime, not only the reverse proxy layer. The reverse proxy should still provide TLS, compression policy, and public-edge routing, but `acp-stack` should defend itself against common direct API attacks.

Required 0.0.2 controls:

- bearer token parsing that rejects duplicate or malformed `Authorization` headers
- constant-time key comparison for session and admin keys
- route-level authorization for session-key vs admin-key operations
- request body limits from config
- rate limiting keyed by client IP and API key hash
- stricter unauthenticated rate limits
- temporary IP blocks after repeated failed auth
- structured security logs for failures and blocks
- CORS allowlist for browser clients
- WebSocket origin validation
- no secret values in logs, errors, metrics, or Supabase events

When `trust_proxy_headers = false`, client IP comes from the socket address. When `trust_proxy_headers = true`, `acp-stack` may use `X-Forwarded-For` or `Forwarded` headers only from configured trusted proxy addresses.
