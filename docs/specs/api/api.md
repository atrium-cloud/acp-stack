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

- `GET /v1/config/export` (session-tier) - returns canonical TOML including `config_version = 1`.
- `POST /v1/config/import` (admin-tier) - parses TOML from the raw body, refuses any change to `[auth].session_key_ref` or `[auth].admin_key_ref`, then atomically writes the canonical form to `~/.config/acp-stack/acp-stack.toml`. Response: `{ imported: true, restart_required: true }`. The running daemon does not hot-reload — clients must restart the daemon for the new config to take effect. A `server.config_imported` event is appended to the events table.
  - Query param `dry_run=true`: validates, canonicalizes, compares auth refs against the running config, and returns metadata (`{ dry_run: true, config_version, canonical_toml_size, input_size, auth_refs_unchanged, target, target_exists }`) without writing to disk or recording an audit event.
  - Import body size is capped at 1 MiB (1,048,576 bytes). Oversized requests return 413 `import.too_large`.
- `POST /v1/config/validate` (session-tier) - parses and validates the raw body without applying.

### Agent API

- `POST /v1/agent/install`
- `POST /v1/agent/start`
- `POST /v1/agent/stop`
- `POST /v1/agent/restart`
- `GET /v1/agent/status`
- `GET /v1/agent/capabilities`

Phase 4 provider/model API:

- `GET /v1/providers` (session-tier) — returns the provider summary list described under the Providers and Models API section below.
- `GET /v1/models` (session-tier) — returns ACP-advertised model + mode values for the configured agent (spawns a provisional ACP session).
- `POST /v1/agent/restart` (admin-tier) — stops the supervised agent if running, then starts it with the current `[agent]` config. Used after `acps agent set` writes provider/model changes that require a process-level reload. Goose model changes do NOT require this; clients can switch live via ACP `session/set_config_option`. Response: `{ stopped_at, started_at, prior_exit_status?, capabilities, pid? }`. When the agent was already stopped, the call degenerates into a plain start and `prior_exit_status` is `null`.

### Session API

- `POST /v1/sessions`
- `GET /v1/sessions`
- `GET /v1/sessions/-/status`
- `GET /v1/sessions/{id}`
- `POST /v1/sessions/{id}/load`
- `POST /v1/sessions/{id}/resume`
- `POST /v1/sessions/{id}/prompt`
- `POST /v1/sessions/{id}/cancel`
- `DELETE /v1/sessions/{id}`
- `GET /v1/sessions/{id}/prompts/{prompt_id}`
- `GET /v1/sessions/{id}/events`

These map to ACP session methods where supported by the configured agent.

`GET /v1/sessions` returns the durable local session list. Before querying
SQLite, the daemon attempts ACP `session/list` when the agent is running and
advertises `sessionCapabilities.list`; each returned `SessionInfo` is upserted
into `sessions`, preserving local `active` and `closed` rows and inserting newly
discovered rows as `status = "available"`. `available` means the session is
known from ACP discovery but is not loaded/active in this runtime.
`session/list` is discovery only: callers must load or resume an available
session before prompting it.
The response includes explicit sync metadata:

```json
{
  "sessions": [],
  "agent_sync": {
    "attempted": true,
    "status": "synced|unsupported|not_running",
    "upserted": 0,
    "updated": 0
  }
}
```

`GET /v1/sessions` accepts optional `since`, `until`, `range`, and
`resolve_bounds` query parameters that filter by `sessions.updated_at`. `range`
accepts `day`, `week`, `month`, `year`, `all`, or a duration suffix such as
`30m`, `12h`, `60d`, `8w`, `6mo`, or `1y`, interpreted relative to the request
time. `since` and `until` accept either an RFC3339 timestamp or the same
duration suffixes, also interpreted relative to the request time. `limit` still
caps the newest-first result set. When `resolve_bounds=true`, omitted `since`
defaults to the first stored session update time and omitted `until` defaults to
the latest stored session update time, or to request time when the latest
session is active.

`GET /v1/sessions/-/status` returns a compact active-session view without full
session metadata or event history. Query params: `threshold` (duration suffix,
default `15m`) and `limit` (default/cap 1000). Active sessions are `recent`
when their latest derived activity is within the threshold; otherwise they are
returned as idle (`recent: false`) rather than hidden. Last activity is derived
from the newest relevant session-scoped ACP event, prompt row, or session-row
fallback. ACP events and prompt status updates report `last_activity_from: "agent"`;
prompt submissions and operator/API/local/CLI session actions report `"user"`.

```json
{
  "generated_at": "2026-05-25T00:00:00.000000000Z",
  "threshold": "15m",
  "active_count": 1,
  "truncated": false,
  "sessions": [
    {
      "id": "sess_123",
      "status": "active",
      "agent_id": "opencode",
      "cwd": "/workspace",
      "title": "work",
      "last_activity_at": "2026-05-25T00:00:00.000000000Z",
      "last_activity_from": "agent",
      "recent": true
    }
  ]
}
```

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

  Response: `{ path, deleted: true }`. Refuses directories with 400 `workspace.path_invalid` (no recursive removal in the current API). Refuses symlinks with 400 `workspace.symlink_escape`.

Every workspace mutation (`workspace.write`, `workspace.upload`, `workspace.delete`) is written to the `events` table and fanned out on the `workspace` WebSocket topic.

### Command API

- `POST /v1/commands`
- `GET /v1/commands`
- `GET /v1/commands/{id}`
- `POST /v1/commands/{id}/cancel`

Commands launched through this API are mediated by the Command Gateway. They are logged, evaluated against policy, and streamed over WebSocket.

`POST /v1/commands` request body:

```json
{
  "command": "<shell string>",
  "cwd": "<optional workspace-relative or absolute path under workspace.root>",
  "env": { "<name>": "<value>" },
  "timeout": "<optional duration like 30s, 10m>"
}
```

Response data (also returned by `GET` and `POST /cancel`):

```json
{
  "id": "cmd_…",
  "created_at": "<RFC3339>",
  "updated_at": "<RFC3339>",
  "status": "pending|running|exited|failed|canceled",
  "command": "<string>",
  "exit_status": 0,
  "started_at": "<RFC3339|null>",
  "finished_at": "<RFC3339|null>",
  "cwd": "<absolute path|null>",
  "duration_ms": 123,
  "truncated": false
}
```

Execution model (Phase 1):

- Shell-string spawn: `[workspace].default_shell -c <command>` with a fresh
  process group on Unix.
- `cwd` resolves under `workspace.root` (relative paths join the root,
  absolute paths must canonicalize inside). Anything else returns
  `command.cwd_outside_workspace`.
- `env` keys must appear on `[commands].env_allowlist` or the submission is
  rejected with `command.env_not_allowed`. Secrets are never injected
  implicitly; the gateway uses `env_clear` and then sets only the names
  passed in the request body.
- Policy:
  - `[permissions].deny` glob matches reject the submission synchronously
    with `command.denied`.
  - `[permissions].review` matches in `auto` mode proceed and emit a
    `command.review_flagged` event.
  - `[permissions].review` matches in `supervised` or `locked` mode, and
    unmatched submissions in `locked` mode, create a pending
    `permission_requests` row (`source = "command"`, `subject_id =
    command_id`), emit a `permission.created` event on the `permissions`
    WebSocket topic, and block subprocess spawn until an operator decides
    through `/v1/permissions/{id}/approve` (transitions the command to
    `running` then its exit status) or `/v1/permissions/{id}/deny` (the
    command finalizes as `failed` without ever spawning). The pending row
    expires automatically after `[permissions].request_timeout` (default
    `5m`); the row's terminal state then follows `[permissions].timeout_action`.
- Output: stdout and stderr are read in bounded chunks (up to 4 KiB per
  read). Each chunk becomes one `command.stdout` / `command.stderr` event
  and is also fanned out on `commands.{id}`; chunk boundaries are not
  guaranteed to line up with newlines. Once a run exceeds
  `[commands].max_output_bytes` further bytes are drained but not
  persisted, the row's `truncated` flag is set, and a
  `command.output_truncated` event is emitted.
- Cancel / timeout: SIGTERM is sent to the process group; if the child
  hasn't exited after `[commands].cancel_grace`, SIGKILL is sent. Timeouts
  produce `status = failed`; explicit cancels produce `status = canceled`.

### Permissions API

All four routes are session-tier (per `docs/specs/security.md` — the operator already has a session when deciding on a permission; admin keys are reserved for management/destructive actions).

- `GET /v1/permissions/pending` — list pending rows, oldest-first. Response: `{ permissions: [ { id, created_at, updated_at, status, source, requester, subject_id, detail, expires_at } ] }`.
- `GET /v1/permissions/{id}` — single row by id.
- `POST /v1/permissions/{id}/approve` — body `{ option_id?: string, reason?: string }`. `option_id` is forwarded to ACP-source requests as the chosen `PermissionOptionId`; if omitted on an ACP-source row, the first option from the original request is used.
- `POST /v1/permissions/{id}/deny` — body `{ reason?: string }`.

A pending request can resolve in four ways: `approved`, `denied`, `expired` (per-row timer from `[permissions].request_timeout`, default `5m`, action from `[permissions].timeout_action`, default `deny`), or `canceled` (the originating session was canceled, the daemon restarted with an unresolved row, or a command awaiting approval was canceled by its caller).

Permission requests can originate from:

- ACP `session/request_permission` (`subject_id` = session id)
- `acp-stack` mediated command policy under `review`/`locked` modes (`subject_id` = command id)

Decisions are recorded in `permission_decisions` with the resolved tier as `deciding_principal` ("session-key" today; "system" for timeout/restart settlements).

### Secrets API

Admin key required.

- `GET /v1/secrets` - response `{ names: [...] }`. Values are never returned.
- `POST /v1/secrets` - body `{ name, value }`. Response `{ name, action: "set" | "updated" }`. Names matching the configured `[auth].session_key_ref` or `[auth].admin_key_ref` are rejected with `secrets.reserved_for_auth` (400) — the auth refs rotate only through `acps auth regenerate-session-key` or `acps reset --yes` + re-init.
- `DELETE /v1/secrets/{name}` - response `{ name, deleted: true }`. Same auth-ref protection as POST.

Secret values are never returned through API, CLI logs, errors, metrics, or WebSocket events.

### Dependencies API

- `GET /v1/deps` (session-tier) - returns declared dependencies and satisfaction status.
- `POST /v1/deps/check` (session-tier) - re-runs validation.
- `POST /v1/deps/apply` (admin-tier) - narrow installer for command deps that declare an `[install]` block. Body `{ confirmation: false }` (or omitted body) returns a structured preview listing every candidate action; `{ confirmation: true }` runs the snippets and returns `{ applied: true, candidates: [...], report: { before, after, results } }`. Optional `feature` field narrows to one declared feature. Per-action outcomes persist as `installer_runs` rows tagged `agent_id = "deps_apply"` / `step = "deps_apply"`. The runner refuses `scope = "system"` actions when the daemon is not root.

Phase 2 reports missing dependencies but does not attempt broad installation by default. Commands are checked via PATH lookup. Packages, runtimes, and MCP cross-references are declarative-only and report `available = false` with a `<kind>-check-not-implemented` reason in this phase (MCP entries cross-reference `[[mcp.servers]]` for declaration presence).

### Providers and Models API

Session-tier.

- `GET /v1/providers` - returns the providers supported for the configured agent, sourced from the embedded provider/env mapping. Each item carries `id`, `name`, optional `agent_provider_id` (when the agent uses a different native id), optional `default_api_key_ref`, and optional `companion_env_refs` / `optional_env_refs` arrays. Pure embedded-mapping lookup — does not spawn the agent.
- `GET /v1/models` - spawns a provisional ACP session against the configured agent, reads its `session/new` advertised `config_options`, and returns `{ agent_id, models: [...], modes: [...] }`. Mirrors what `acps agent set` reads when prompting interactively, so a UI driver can render the same picker without shelling out. Empty `modes` indicates the agent does not advertise a mode option. The agent must be installed and the per-call discovery can take several seconds; `session/new` is bounded by a timeout and the provisional agent process is shut down before the endpoint returns.

### Status, Logs, and Metrics API

- `GET /v1/status`
- `GET /v1/status/agent`
- `GET /v1/status/connections`
- `GET /v1/health/live`
- `GET /v1/health/ready`
- `GET /v1/security/check`
- `GET /v1/logs/events`
- `GET /v1/logs/commands`
- `GET /v1/logs/permissions`
- `GET /v1/logs/security`
- `GET /v1/logs/sessions`
- `GET /v1/metrics/summary`

The current daemon implements the status/log/metrics subset against local config, SQLite state, and the in-process agent supervisor:

- `GET /v1/status` returns schema version, latest durable event timestamp, and server version.
- `GET /v1/status/agent` and `GET /v1/agent/status` return the configured agent identity/command, optional adapter metadata, process state, pid when running, and recent `agent_lifecycle` records.
- `GET /v1/agent/capabilities` returns the latest persisted ACP `initialize` capability snapshot plus optional adapter metadata and current process state. Before the first successful start it returns `agent.not_initialized`.
- `GET /v1/status/connections` returns the current in-process active HTTP request count.
- `GET /v1/health/live` is session-tier and answers "is the process up and the router accepting requests?" — returns `200` with `{ ok: true, data: { ok: true, server: { version } } }` once `acps serve` has bound its listener. Touches no state-store, supervisor, or workspace primitive, so it remains 200 even when subsystems are degraded. Operators that need subsystem detail should call `/v1/health/ready` instead.
- `GET /v1/health/ready` is session-tier and aggregates the signals that distinguish a healthy daemon from one that is struggling: SQLite reachability, workspace writability, agent process state, external logging sink backlog, and the most recent `acps deps apply` run. Returns `200` when every subsystem reports ok, otherwise `503 Service Unavailable` with the same body shape so callers can read `data.failing` and per-subsystem detail off a single schema regardless of status code. The envelope follows the convention from the [Response Envelope](#response-envelope) section: top-level `ok` mirrors `data.ok` (so a 503 has `{ok: false, data: {...}}`), but unlike error responses the failing case still nests the full report under `data` rather than the standard `error` block, since readiness is fundamentally a structured success-or-degraded signal rather than an exception. Body shape: `{ ok: bool, data: { ok, failing: [string], sqlite: { reachable, schema_version?, latest_event_at?, error? }, workspace: { writable, path }, agent: { configured, id, state, pid? }, sink: { enabled, open_failure_count, latest_failure_at?, latest_error?, probe_error? }, deps: { last_apply_run_id?, last_apply_at?, last_apply_status?, last_apply_exit?, cluster_has_failure, probe_error? } } }`. The `failing` list names subsystems that flipped the ok flag — currently `sqlite`, `workspace`, `sink`, and `deps`. The `agent` subsystem is reported but never flips `ok`: a stopped process can be the operator's intent (e.g. between `acps agent stop` and `acps agent start`). For rows written by schema 13+ binaries, `deps.cluster_has_failure` aggregates every per-dependency installer row with the latest non-null `apply_run_id`, so partial applies are evaluated exactly even when rows are far apart in time. Legacy rows with null `apply_run_id` retain the pre-13 15-minute timestamp-neighborhood fallback (covering the default 10-minute per-step apply timeout with slack) because old invocations cannot be reconstructed perfectly. `sink.probe_error` and `deps.probe_error` capture state-store query failures distinct from `sink.latest_error` (the Supabase upload error) so a corrupt sink or installer_runs table cannot make those subsystems look healthy. MCP reachability, orphaned-PID detection, and stalled-prompt detection are tracked separately and not part of this report yet.
- `GET /v1/security/check` is admin-tier and returns `{ ok, findings, auth_failure_count }`, where each finding is `{ code, severity, message, remediation? }`. `remediation` is an optional operator-actionable hint string (e.g. a literal `chmod 0700 -- '<path>'` command for `runtime.path_mode_loose`); the field is omitted when no remediation applies. It reports findings for the effective listener bind (including `acps serve --bind` overrides), wildcard CORS on public binds, proxy-header trust without a trusted proxy allowlist, empty or weak cached API keys, auth-failure counts in the last minute at or above the configured threshold, Supabase sink delivery failures, uninspectable runtime-managed paths, owner-only mode and ownership problems on runtime-managed paths, configured runtime-user mismatches, and workspace writability failures.

#### `GET /v1/logs/events`

Session-tier. Returns durable event rows newest-first.

Query parameters (all optional):

- `limit` (default `100`, max `1000`).
- `level` — exact match (`info`, `warn`, `error`, ...).
- `kind` — exact event kind, or a dotted prefix when the value ends with `.` (e.g. `kind=command.` matches every `command.*` kind).
- `source` — writer label: `system`, `api`, `acp`, `command`, `permission`, `cli`, `local`. Added in Phase 3 (migration 007).
- `session_id` — events scoped to the given session id (uses the `events.session_id` column populated by the ACP bridge).
- `command_id` — events whose `payload_json.command_id` matches.
- `permission_id` — events whose `payload_json.permission_id` matches. Legacy permission events with only `payload_json.id` also match when the row is permission-scoped.
- `since`, `until` — RFC3339 bounds (`since` inclusive, `until` exclusive). Strings are compared lexicographically against the stored RFC3339 timestamps.
- `after` — keyset pagination cursor; pass the `id` of the last event from the previous page.

Response:

```json
{
  "ok": true,
  "data": {
    "events": [
      { "id": "evt_…", "created_at": "…", "level": "info", "kind": "command.exited",
        "message": "", "payload_json": "{…}", "source": "command" }
    ],
    "next_cursor": "evt_…"  // present (and non-null) only when the page saturated `limit`
  }
}
```

#### `GET /v1/logs/sessions`, `GET /v1/logs/commands`

Session-tier. Same response envelope as `/v1/logs/events` (typed item shapes from the corresponding SQLite tables, plus `next_cursor`). Query params: `limit`, `since`, `until`, `after`, plus `status` on commands.

#### `GET /v1/logs/permissions`

Session-tier. Returns durable events whose kind starts with `permission.` / `permissions.`. Accepts `limit`, `kind`, `source`, `since`, `until`, `after`, `permission_id`. Body: `{ events, next_cursor }`.

#### `GET /v1/logs/security`

Session-tier. Returns `{ auth_failures, events, auth_failures_next_cursor, events_next_cursor }`. Both inner streams accept `limit`, `since`, and `until`. Page them independently with `auth_failures_after` and `events_after`; the legacy `after` parameter remains as a compatibility alias for both streams when the stream-specific cursor is omitted. `auth_failures` lists durable auth-rejection rows; `events` lists durable `security.*` rows (rate-limit hits, IP blocks, denied origins, oversized requests, etc.). Attempted token values are never stored or returned.

#### `GET /v1/metrics/summary`

Session-tier. Returns derived metrics for the requested window. When `since` is omitted the default window is the last 24 hours. Accepts `since`, `until` as either RFC3339 timestamps or duration suffixes (`30m`, `1h`, `2d`, `1w`); when a duration is supplied it is interpreted as "this much time ago".

Response:

```jsonc
{
  "ok": true,
  "data": {
    "window": { "since": "<RFC3339>", "until": "<RFC3339>" },
    "counts": { "events": int, "sessions": int, "commands": int,
                "auth_failures": int, "agent_lifecycle": int, "installer_runs": int,
                "agent_capabilities": int, "prompts": int,
                "permission_requests": int, "permission_decisions": int },
    "sessions": { "active": int, "closed": int,
                  "average_duration_ms": int|null,
                  "p50_duration_ms": int|null, "p95_duration_ms": int|null },
    "turns": { "total": int, "by_status": { ... }, "average_per_session": float|null },
    "commands": { "total": int, "by_status": { ... },
                  "average_duration_ms": int|null,
                  "p50_duration_ms": int|null, "p95_duration_ms": int|null,
                  "truncated_count": int },
    "permissions": { "total": int, "by_outcome": { ... },
                     "average_response_ms": int|null,
                     "p50_response_ms": int|null, "p95_response_ms": int|null },
    "security": { "auth_failures": int, "by_reason": { ... }, "events_by_kind": { ... } },
    "api_connections": { "request_count": int|null,
                         "by_status": { "2xx": int, ... },
                         "average_duration_ms": int|null },
    "ws_connections": { "connections_opened": int|null,
                        "connections_closed": int|null,
                        "average_duration_ms": int|null },
    "usage": { "tokens_input": int|null, "tokens_output": int|null,
               "context_window_max": int|null }
  }
}
```

`api_connections.request_count` and `ws_connections.*` are populated by the `api.request` / `ws.client_connected` / `ws.client_disconnected` audit events emitted by the daemon (source `api`). `/v1/ws` and `/v1/status*` are excluded from `api.request` to keep cardinality bounded. `usage.*` is best-effort: when the configured agent reports token/context usage on ACP `session/update`, the bridge persists a `usage.reported` event and the metrics summary aggregates it; otherwise these fields stay `null`.

## WebSocket

WebSocket endpoint:

```text
GET /v1/ws
```

The WebSocket multiplexes runtime events. Clients subscribe to topics:

- `sessions.{id}` (implemented for live ACP `session/update` events)
- `commands.{id}` (implemented; emits `command.started`, `command.stdout`, `command.stderr`, `command.exited`, `command.failed`, `command.canceled`, `command.timeout`, `command.output_truncated`, `command.review_flagged`)
- `permissions` (implemented; emits `permission.created`, `permission.approved`, `permission.denied`, `permission.canceled`, `permission.expired`)
- `workspace` (implemented; emits `workspace.write`, `workspace.upload`, `workspace.delete`)
- `agent` (implemented; emits `agent.starting`, `agent.started`, `agent.spawn_failed`, `agent.stopped`)
- `status` (implemented; emits `server.started`, `server.stopped`)
- `logs` (implemented; emits every event-table row regardless of source)

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

### WebSocket Management

The runtime maintains an in-memory registry of `/v1/ws` clients with connection id, connected time, last activity, subscribed topics, derived `sessions.{id}` subscriptions, disconnect signal, and bounded request-origin metadata. Connection metadata is persisted only through connect/disconnect lifecycle events; the live registry is process-local and clears on daemon restart.

Planned routes:

- `GET /v1/ws/connections` — session-tier sanitized view of live WebSocket clients.
- `GET /v1/ws/sessions` — unique subscribed ACP session ids with live connection counts.
- `POST /v1/ws/connections/disconnect` — admin-tier only; body `{ "connection_ids": ["ws_..."], "reason": "operator-request" }`.
- `POST /v1/ws/sessions/disconnect` — admin-tier only; body `{ "session_ids": ["sess_..."], "reason": "operator-request" }`.

Disconnecting a WebSocket client closes only that socket. It must not close the underlying ACP session, cancel prompts, or mutate durable session state. The disconnect event should record `reason = "operator_disconnect"` and include topics/session ids so operators can distinguish client management from agent/session lifecycle.

### Request-Origin Metadata

When Cloudflare edge mode is configured, request-origin enrichment flows through the same trusted-proxy validation used for client IP selection. The daemon ignores Cloudflare headers from untrusted peers. Bounded fields include `origin_kind`, `proxy_provider`, `client_ip`, `country_code`, `region_code`, `region_name`, and `cloudflare_ray_id`; public metrics should aggregate by bounded buckets rather than exposing raw headers.

## API Security Boundaries

The API uses the same two-key model and security controls described in [security](../security.md). Session-key and admin-key authorization boundaries are part of the public API contract, and browser-facing clients must satisfy the configured CORS and WebSocket origin policy.

### HTTP Hardening

HTTP hardening is part of the runtime, not only the reverse proxy layer. The reverse proxy should still provide TLS, compression policy, and public-edge routing, but `acp-stack` should defend itself against common direct API attacks.

Required Phase 2 controls:

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
