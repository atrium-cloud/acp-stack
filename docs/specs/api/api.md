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

These map to ACP session methods where supported by the configured agent.

### Workspace API

- `GET /v1/workspace`
- `GET /v1/files?path=...`
- `GET /v1/files/content?path=...`
- `PUT /v1/files/content`
- `POST /v1/files/upload`
- `GET /v1/files/download?path=...`
- `DELETE /v1/files?path=...`

All paths are resolved under `workspace.root` unless explicitly allowed by future policy. The workspace API should reject path traversal and symlink escapes.

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

## WebSocket

WebSocket endpoint:

```text
GET /v1/ws
```

The WebSocket multiplexes runtime events. Clients subscribe to topics:

- `sessions.{id}`
- `commands.{id}`
- `permissions`
- `workspace`
- `agent`
- `status`
- `logs`

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
    "kind": "agent_message_chunk",
    "text": "Done"
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
