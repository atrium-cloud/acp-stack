# API Spec

All public HTTP routes are versioned under `/v1`. Clients authenticate with a bearer API key:

```http
Authorization: Bearer <key>
```

## Auth Tiers

| Tier        | Used for                                                                        |
| ----------- | ------------------------------------------------------------------------------- |
| Session key | sessions, workspace files, mediated commands, logs, status, pending permissions |
| Admin key   | secrets, config import, agent process control, security-sensitive operations    |
| Local       | internal Unix socket used by keyless local `acps` routes                        |

`acps init` creates the session and admin keys on first run, prints the plaintext once, and stores only local verifier rows. The session key can be rotated by an admin-authenticated daemon call. The admin key is regenerated only by resetting and reinitializing the instance.

Public HTTP tiering is strict. `[local].session_auth = "keyless"` only affects same-user Unix-socket access and never makes admin keys valid for public session routes.

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

Binary downloads and WebSocket frames are not wrapped in this envelope.

## Bootstrap Init API

`acps init serve` exposes only the bootstrap init routes below. Normal session/admin `/v1` routes are not mounted in this mode. Calls use exactly one `Authorization: Bearer <bootstrap-token>` header; the token comes from process input, not config or state.

| Route                                      | Contract |
| ------------------------------------------ | -------- |
| `POST /v1/init/sessions`                  | starts one active init session and accepts optional initial agent/provider/model/workspace args (including `sandbox` mode for `[workspace.sandbox]`) |
| `GET /v1/init/sessions/{id}`              | returns non-secret status, pending input, recent progress, and `completed_awaiting_ack` when a result exists |
| `GET /v1/init/sessions/{id}/events?after_seq=N` | replays non-secret progress and input lifecycle events |
| `GET /v1/init/sessions/{id}/ws`           | upgrades to the hosted init WebSocket |

`POST /v1/init/sessions` returns `{ "session_id": "...", "status": "running" }` in the standard success envelope. It returns `409 init.session_active` while another session is running or awaiting result acknowledgement.

Status and event replay never include plaintext session/admin keys or secret input values. Pending input includes `request_id`, `style`, `prompt`, `required`, optional `default`, and visible option labels/hints. Client `input` frames must include the active `request_id`; stale input is rejected.

WebSocket server frames are `hello`, `progress`, `input_required`, `input_accepted`, `result`, and `error`. Client frames are `input`, `cancel`, `replay_result`, and `ack_result`. The final `result` frame carries the platform handoff payload, including freshly generated keys when present.

After `result`, the session remains `completed_awaiting_ack`. If the WebSocket drops before acknowledgement, the backend reconnects and sends `replay_result`; the server does not replay keys through status or generic events. `ack_result` is terminal: the server clears the in-memory handoff payload, closes the session, and exits successfully.

## Config And Secrets

| Route                                | Tier    | Contract                                                     |
| ------------------------------------ | ------- | ------------------------------------------------------------ |
| `GET /v1/config/export`              | session | returns current canonical TOML with secret refs only         |
| `POST /v1/config/validate`           | session | validates raw TOML without writing                           |
| `POST /v1/config/import`             | admin   | validates and writes canonical TOML; supports `dry_run=true` |
| `GET /v1/secrets`                    | admin   | lists secret names only                                      |
| `POST /v1/secrets`                   | admin   | stores or replaces a secret value                            |
| `DELETE /v1/secrets/{name}`          | admin   | deletes a secret                                             |
| `POST /v1/auth/session-key/regenerate` | admin | replaces the session verifier and returns the new plaintext key once |
| `PUT /v1/auth/local-session-access`  | admin   | sets `[local].session_auth` and applies it to the running daemon |

Secret values are never returned by the API. Auth keys are not secret-store entries.

## Agent And Providers

| Route                        | Tier    | Contract                                                      |
| ---------------------------- | ------- | ------------------------------------------------------------- |
| `POST /v1/agent/install`     | admin   | installs the configured supported agent                       |
| `POST /v1/agent/start`       | admin   | starts the supervised agent process                           |
| `POST /v1/agent/stop`        | admin   | stops the supervised agent process                            |
| `POST /v1/agent/restart`     | admin   | restarts the supervised agent process                         |
| `GET /v1/agent/restart-blockers` | admin | returns active-session blockers for guarded restart        |
| `POST /v1/agent/switch`      | admin   | switches harness, installs it, and returns model choices      |
| `GET /v1/agent/status`       | session | returns configured identity and process state                 |
| `GET /v1/agent/capabilities` | session | returns the latest ACP capability snapshot when available     |
| `GET /v1/providers`          | session | lists provider ids available for the configured agent         |
| `GET /v1/models`             | session | lists ACP-advertised model and mode choices when discoverable |

Agent start/restart uses the current `[agent]` config and injected secret refs. Provider/model changes that require process reload are applied after restart. `POST /v1/agent/restart?require_idle=true` returns blockers instead of restarting when active sessions have in-flight prompts or pending ACP permission requests. `POST /v1/agent/restart?auto=true` queues a restart that runs once the same blockers clear. `GET /v1/agent/restart-blockers` returns `{ "target_id": "...", "blockers": [...] }`; blocker rows include `session_id`, `target_id`, `state`, and either prompt fields (`prompt_id`, `prompt_status`, `prompt_stop_reason`) or `permission_id`. State values are `prompt_sent`, `working`, `permission_required`, and defensive `blocked`.

`POST /v1/agent/switch` accepts `{ "agent": "<id>", "provider": "<optional-provider-id>", "api_key_ref": "<optional-ref>", "drop": false }`. The route validates provider compatibility, copies compatible provider secret refs when the target expects a different default ref, installs the target harness, provisions agent-owned config without a model, discovers ACP-advertised model values when the target supports model selection, writes canonical config, restarts the supervised agent only if it was already running, and optionally removes source agent-owned config. Source cleanup failures are reported as `cleanup_errors` without rolling back a successful switch. `drop` does not delete secrets, installed harnesses/adapters, or sessions.

## Array

| Route                                            | Tier    | Contract                                                            |
| ------------------------------------------------ | ------- | ------------------------------------------------------------------- |
| `GET /v1/array/status`                           | session | enabled flag, primary target, local-delegation readiness, per-target id/agent/name/state/pid |
| `GET /v1/array/targets/{target_id}/capabilities` | session | latest ACP capability snapshot for one target                       |
| `POST /v1/array/targets/{target_id}/install`     | admin   | installs one target's harness                                       |
| `POST /v1/array/targets/{target_id}/start`       | admin   | starts one target's process                                         |
| `POST /v1/array/targets/{target_id}/stop`        | admin   | stops one target's process                                          |
| `POST /v1/array/targets/{target_id}/restart`     | admin   | restarts one target's process                                       |

The `/v1/agent/*` routes operate on the Array `primary_target`. Session routes accept `?target=<id>` (alias `target`) to address a specific target; an unknown `target_id` returns `400 request.invalid_param`. With Array off, only the primary target is addressable for driving session ops and start/restart of a non-primary target is rejected with `400`, but terminal ops (`close`, `cancel`) can still wind down a session on any stored target. See [../array.md](../array.md) for the full Array model.

## Sessions

| Route                                       | Tier    | Contract                                                       |
| ------------------------------------------- | ------- | -------------------------------------------------------------- |
| `POST /v1/sessions`                         | session | creates a new ACP session                                      |
| `GET /v1/sessions`                          | session | lists durable sessions, optionally after ACP session-list sync |
| `GET /v1/sessions/-/status`                 | session | returns compact windowed session turn status                    |
| `GET /v1/sessions/{id}`                     | session | returns one session                                            |
| `POST /v1/sessions/{id}/load`               | session | loads an existing agent session                                |
| `POST /v1/sessions/{id}/resume`             | session | resumes a session                                              |
| `POST /v1/sessions/{id}/fork`               | session | forks a session through ACP                                    |
| `POST /v1/sessions/{id}/prompt`             | session | enqueues a prompt and returns a prompt id                      |
| `POST /v1/sessions/{id}/cancel`             | session | cancels an in-flight prompt                                    |
| `DELETE /v1/sessions/{id}`                  | session | closes the agent-side session and preserves local history      |
| `GET /v1/sessions/{id}/prompts/{prompt_id}` | session | returns prompt status                                          |
| `GET /v1/sessions/{id}/events`              | session | returns durable session events                                 |
| `GET /v1/sessions/{id}/changes`             | session | returns the process-local ACP file-diff snapshot               |
| `GET /v1/sessions/{id}/snapshot`            | session | returns session row, in-flight prompts, and recent events      |

`POST /v1/sessions/{id}/prompt` is asynchronous. Clients can poll the prompt status endpoint or subscribe to `sessions.{id}` over WebSocket.

Before a prompt row is created, media-bearing prompts are checked against the selected target model's known input modalities from `models.dev`. Confidently unsupported image, audio, or video input returns HTTP 400 `prompt.unsupported_modality`; unknown models, unavailable catalog data, PDFs, and generic files are allowed through.

Session create, load, resume, and fork accept an optional `cwd`. Session `cwd` values must be existing directories that canonicalize under `[workspace].root`; stored CWD defaults are rechecked before reuse. Explicit load/resume CWDs are stored after the agent accepts the call. Closed sessions cannot be loaded, resumed, forked, or prompted.

Session close is history-preserving: the runtime calls ACP `session/close` when supported, marks the local row `closed`, and keeps durable events/query history. Permanent deletion is deferred until product semantics are defined.

`GET /v1/sessions/{id}/changes` reduces explicit ACP `type: "diff"` tool-call content into the latest tool-call snapshot. A missing `oldText` is returned as `null` and represents a created file. Tool-call-update content replaces the prior collection when present and otherwise leaves it intact. The snapshot is bounded, process-local, and identified by `generation` plus `revision`; `truncated: true` means whole tool calls were omitted by a capacity limit. It is not rebuilt from SQLite after restart. Raw `session.update` event persistence and WebSocket delivery are unchanged.

`POST /v1/sessions/{id}/fork` also accepts optional `{ "message_id": "<prompt message id>" }`. `message_id` requires an acknowledged ACP prompt message id from the parent session; unsupported fork capabilities return HTTP 501 `agent.unsupported_capability`.

Prompt status values are `pending`, `running`, `completed`, `errored`, `cancelled`, and `stalled`. `stalled` is a terminal status reached only when the stale-prompt sweeper observes no ACP `session/update` activity for longer than `[prompts].stale_threshold`. From the client's perspective, a `stalled` prompt is final: it will not transition back to `running`, and recovery means submitting a new prompt. See `docs/specs/runtime.md` for the sweeper contract.

`GET /v1/sessions/{id}/snapshot` is the reconnect-bootstrap helper. The response carries:

- `session` — full session row (id, status, agent id, cwd, title, metadata).
- `in_flight_prompts` — prompts currently in `pending` or `running`. Empty when the session is idle. Each entry is the same shape returned by `GET /v1/sessions/{id}/prompts/{prompt_id}`.
- `last_event_id` — the id of the newest persisted session event, or `null` when the session has no events. Acts as a tail cursor for forward catch-up: callers fetch events newer than the snapshot via `GET /v1/sessions/{id}/events?after=last_event_id`, which paginates forward on `(created_at, id)` ascending.
- `recent_events` — the latest session events, newest-first, capped at 50. The cap is enforced by `SNAPSHOT_RECENT_EVENTS_LIMIT` in `src/api/routes/sessions.rs` and is sized to cover one prompt-turn's worth of updates without bloating the response.

The intended reconnect flow is: `GET snapshot` once to recover state, subscribe to `sessions.{id}` over WebSocket, then use `GET events?after=last_event_id` to catch up on any events that landed between the snapshot read and the WebSocket subscribe. For deeper history (older than the 50-event snapshot window), additional pagination is not currently exposed; older events are reachable only through the durable logs endpoints.

Session status defaults to a rolling `8h` activity window and accepts `window=<duration>` from `1m` through `999h`. Each row includes a derived `state`: `idle`, `prompt_sent`, `working`, `permission_required`, `done`, `stopped`, `error`, `cancelled`, `available`, or `closed`. `done` means the latest prompt completed with `stop_reason = "end_turn"`.

Session list filters accept `limit`, time bounds, and range values. Duration suffixes such as `30m`, `12h`, `60d`, `8w`, `6mo`, and `1y` are interpreted relative to request time.

The local Unix-socket router always exposes selected low-risk daemon-backed routes without bearer auth for local `acps` commands, including `GET /v1/sessions`, `GET /v1/sessions/-/status`, metrics summary, WebSocket summaries, and the security diagnostic. Session-tier HTTP routes are also mounted on the local socket but return 404 unless `[local].session_auth = "keyless"` is active. Admin-tier routes, auth rotation, config import, secret mutation, dependency apply, WebSocket disconnects, and WebSocket upgrades are not registered on the local socket.

### Prompt-Path Error Codes

Terminal prompt failures surface through the prompt row's `error_code` and through the matching session-scoped event:

| `error_code`           | HTTP status | Description                                                                            |
| ---------------------- | ----------- | -------------------------------------------------------------------------------------- |
| `agent.inference_5xx`  | 502         | Upstream inference endpoint returned 5xx (or the 529-overloaded variant)               |
| `agent.inference_4xx`  | 424         | Upstream inference endpoint returned 4xx (rate limit, malformed request)               |
| `agent.request_failed` | 502         | Agent rejected the ACP request for a non-inference reason                              |
| `prompt.stalled`       | n/a         | Sweeper-written code on rows it flipped to `stalled`; not surfaced as an HTTP response |

The `agent.inference_*` codes carry a sanitized public message of the form `"inference endpoint returned <status_code> (<reason_category>)"`, where `reason_category` is drawn from a fixed static enum. No URLs, request/response bodies, headers, or secret material reach the API response or the persisted prompt row; see `docs/specs/state-logging.md` for the full taxonomy and event shapes.

## Metrics Summary

`GET /v1/metrics/summary` includes `prompt_failures` so operators can separate upstream inference outages from local runtime failures. The object contains `total`, explicit counters for each `failure_class` (`inference_5xx`, `inference_4xx`, `agent_request`, `vm`, `sqlite`, `daemon`, `agent_process`, `stalled`), `by_class`, and inference event breakdowns by HTTP status code and reason category.

The `api_connections` metrics block includes `request_count`, `average_duration_ms`, existing `by_status` response buckets, and count maps by method, route template, key kind, event source, origin kind, country code, and region code. Missing country or region metadata is grouped under `unknown`.

## Workspace Files

Workspace routes are session-tier. Paths are workspace-relative. The runtime rejects absolute paths, NUL bytes, `..` traversal, symlink escapes, writes through existing symlink targets, and files above `workspace.max_file_bytes`.

| Route                             | Contract                                   |
| --------------------------------- | ------------------------------------------ |
| `GET /v1/workspace`               | returns workspace metadata                 |
| `GET /v1/files?path=...`          | lists directory entries                    |
| `GET /v1/files/content?path=...`  | reads a file as UTF-8 or base64            |
| `PUT /v1/files/content`           | writes a file atomically                   |
| `POST /v1/files/upload`           | uploads one file below `workspace.uploads` |
| `GET /v1/files/download?path=...` | streams raw file bytes                     |
| `DELETE /v1/files?path=...`       | deletes one regular file                   |

`workspace.max_file_bytes` caps reads, writes, uploads, and downloads. Oversized files return `413 workspace.too_large`.

## Commands

Commands are session-tier and mediated by policy.

| Route                           | Contract                         |
| ------------------------------- | -------------------------------- |
| `POST /v1/commands`             | starts or queues a shell command |
| `GET /v1/commands`              | lists command records            |
| `GET /v1/commands/{id}`         | returns one command              |
| `GET /v1/commands/{id}/output`  | returns persisted output chunks  |
| `POST /v1/commands/{id}/cancel` | cancels a running command        |

Request body:

```json
{
  "command": "rg TODO .",
  "cwd": ".",
  "env": { "NAME": "value" },
  "timeout": "10m"
}
```

Command status values are `pending`, `running`, `exited`, `failed`, and `canceled`. Command records include `last_output_event_id`, `last_output_at`, `last_output_seq`, `output_bytes`, and `last_progress_at` for reconnect and liveness checks, plus `origin` (`operator` for gateway submissions, `acp` for agent-created client terminals) and `session_id` (set on `acp`-origin rows).

Output is persisted up to the configured byte cap and streamed on the command WebSocket topic while the command runs. `GET /v1/commands/{id}/output` accepts `limit`, `after`, and `order=asc|desc` and returns `{ chunks, next_cursor }`. Each chunk is shaped as `{ event_id, created_at, command_id, stream, seq, data }`.

The reconnect flow is: read `GET /v1/commands/{id}`, subscribe to `commands.{id}`, then query `/output?order=asc&after=<last-seen-event-id>` to catch chunks missed between the HTTP read and WebSocket subscribe.

## Permissions

| Route                               | Tier    | Contract                                   |
| ----------------------------------- | ------- | ------------------------------------------ |
| `GET /v1/permissions/pending`       | session | lists pending requests                     |
| `GET /v1/permissions/{id}`          | session | returns a single permission request        |
| `POST /v1/permissions/{id}/approve` | session | approves a request                         |
| `POST /v1/permissions/{id}/deny`    | session | denies a request                           |

Cancellation is not an HTTP operation: pending requests are cancelled internally when their owning flow ends (session close, mediated-command cancel).

Permission requests are created by ACP permission callbacks and by mediated commands when policy requires review. Composed mediated commands using shell control operators, command substitution, or process substitution require review before execution, including in `permissions.mode = "auto"`. Policy matching considers shell-word-normalized command words, so constructed spellings such as quoted or escaped command names can be denied or routed to review.

## Dependencies

| Route                 | Tier    | Contract                           |
| --------------------- | ------- | ---------------------------------- |
| `GET /v1/deps`        | session | reports declared dependency status |
| `POST /v1/deps/check` | session | re-checks dependency status        |
| `POST /v1/deps/apply` | admin   | runs declared install actions      |

The runtime never invents package-manager commands. Only install actions declared in config can be applied. Apply responses include `apply_run_id` for correlating dependency audit rows.

## Status, Logs, Metrics, And Security

| Route                        | Tier    | Contract                                         |
| ---------------------------- | ------- | ------------------------------------------------ |
| `GET /v1/status`             | session | returns local status summary                     |
| `GET /v1/status/agent`       | session | alias for agent status                           |
| `GET /v1/status/connections` | session | returns active HTTP request count                |
| `GET /v1/health/live`        | session | process liveness                                 |
| `GET /v1/health/ready`       | session | subsystem readiness summary; `503` when degraded |
| `GET /v1/security/check`     | admin   | runs the self-check, persists the run, returns findings       |
| `GET /v1/security/history`   | admin   | lists persisted self-check runs newest-first                  |
| `GET /v1/security/history/{run_id}` | admin | returns a single self-check run with findings          |
| `GET /v1/logs/events`        | session | returns durable event rows; supports `category=` and `order=` |
| `GET /v1/logs/commands`      | session | returns command history; supports `order=`                    |
| `GET /v1/logs/permissions`   | session | returns permission history; supports `order=`                 |
| `GET /v1/logs/security`      | session | returns security events; `order=` applies to both result streams |
| `GET /v1/logs/sessions`      | session | returns session-scoped history; supports `order=`             |
| `GET /v1/metrics/summary`    | session | returns aggregate metrics for a time window                   |

Log query filters include `limit`, `level`, `kind`, `source`, `session_id`, `command_id`, `permission_id`, `category`, `since`, `until`, `after`, and `order`. `order` accepts `asc` or `desc` (default `desc`). On `/v1/logs/security`, `order` applies to both `auth_failures` and `events`; `category` accepts the security-category labels documented in `docs/specs/state-logging.md` and constrains only the `events` stream.

Readiness includes an `mcp` object for configured MCP declarations:

```json
{
  "configured_count": 1,
  "failing_count": 0,
  "servers": [
    {
      "name": "linear",
      "kind": "http",
      "ok": true
    }
  ]
}
```

Stdio server rows may include `command_path`. Failing rows may include `missing_secret_refs` and `reason`. HTTP MCP readiness validates declaration shape and secret refs only; it does not call the remote MCP endpoint.

Readiness also reports orphaned agent process groups under `agent.orphaned_process_count` and `agent.orphaned_process_pids`. Any live process group from an older `agent.started` lifecycle row, excluding the currently supervised PID, degrades readiness with `agent` in `failing`.

## WebSocket

`GET /v1/ws` upgrades to a WebSocket connection. Clients authenticate with the session key and subscribe to topics such as:

- `logs`
- `workspace`
- `permissions`
- `commands.{id}`
- `sessions.{id}`
- `agent.lifecycle`

WebSocket management routes:

| Route                                | Tier    | Contract                                     |
| ------------------------------------ | ------- | -------------------------------------------- |
| `GET /v1/ws/connections`             | session | lists active connections without raw secrets |
| `GET /v1/ws/sessions`                | session | lists session-topic subscriptions            |
| `POST /v1/ws/connections/disconnect` | admin   | disconnects one connection                   |
| `POST /v1/ws/sessions/disconnect`    | admin   | disconnects subscribers to a session         |

## HTTP Hardening

The API enforces bearer auth, request-size limits, origin checks, rate limits, auth-failure blocking, and bounded proxy-header trust. Disallowed browser origins return `403 auth.origin_not_allowed`. Oversized JSON requests return `413 request.too_large`.
