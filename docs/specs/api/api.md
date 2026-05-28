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
| Local       | `acpctl` over the local Unix socket only                                        |

`acps init` creates the session and admin keys on first run. The session key can be rotated. The admin key is regenerated only by resetting and reinitializing the instance.

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

## Config And Secrets

| Route                       | Tier    | Contract                                                     |
| --------------------------- | ------- | ------------------------------------------------------------ |
| `GET /v1/config/export`     | session | returns canonical TOML with secret refs only                 |
| `POST /v1/config/validate`  | session | validates raw TOML without writing                           |
| `POST /v1/config/import`    | admin   | validates and writes canonical TOML; supports `dry_run=true` |
| `GET /v1/secrets`           | admin   | lists secret names only                                      |
| `POST /v1/secrets`          | admin   | stores or replaces a secret value                            |
| `DELETE /v1/secrets/{name}` | admin   | deletes a secret                                             |

Secret values are never returned by the API.

## Agent And Providers

| Route                        | Tier    | Contract                                                      |
| ---------------------------- | ------- | ------------------------------------------------------------- |
| `POST /v1/agent/install`     | admin   | installs the configured supported agent                       |
| `POST /v1/agent/start`       | admin   | starts the supervised agent process                           |
| `POST /v1/agent/stop`        | admin   | stops the supervised agent process                            |
| `POST /v1/agent/restart`     | admin   | restarts the supervised agent process                         |
| `POST /v1/agent/switch`      | admin   | switches harness, installs it, and returns model choices      |
| `GET /v1/agent/status`       | session | returns configured identity and process state                 |
| `GET /v1/agent/capabilities` | session | returns the latest ACP capability snapshot when available     |
| `GET /v1/providers`          | session | lists provider ids available for the configured agent         |
| `GET /v1/models`             | session | lists ACP-advertised model and mode choices when discoverable |

Agent start/restart uses the current `[agent]` config and injected secret refs. Provider/model changes that require process reload are applied after restart.

`POST /v1/agent/switch` accepts `{ "agent": "<id>", "provider": "<optional-provider-id>", "api_key_ref": "<optional-ref>", "drop": false }`. The route validates provider compatibility, copies compatible provider secret refs when the target expects a different default ref, installs the target harness, provisions agent-owned config without a model, discovers ACP-advertised model values when the target supports model selection, writes canonical config, restarts the supervised agent only if it was already running, and optionally removes source agent-owned config. Source cleanup failures are reported as `cleanup_errors` without rolling back a successful switch. `drop` does not delete secrets, installed harnesses/adapters, or sessions.

## Sessions

| Route                                       | Tier    | Contract                                                       |
| ------------------------------------------- | ------- | -------------------------------------------------------------- |
| `POST /v1/sessions`                         | session | creates a new ACP session                                      |
| `GET /v1/sessions`                          | session | lists durable sessions, optionally after ACP session-list sync |
| `GET /v1/sessions/-/status`                 | session | returns compact active-session status                          |
| `GET /v1/sessions/{id}`                     | session | returns one session                                            |
| `POST /v1/sessions/{id}/load`               | session | loads an existing agent session                                |
| `POST /v1/sessions/{id}/resume`             | session | resumes a session                                              |
| `POST /v1/sessions/{id}/prompt`             | session | enqueues a prompt and returns a prompt id                      |
| `POST /v1/sessions/{id}/cancel`             | session | cancels an in-flight prompt                                    |
| `DELETE /v1/sessions/{id}`                  | session | closes or deletes a session when supported                     |
| `GET /v1/sessions/{id}/prompts/{prompt_id}` | session | returns prompt status                                          |
| `GET /v1/sessions/{id}/events`              | session | returns durable session events                                 |
| `GET /v1/sessions/{id}/snapshot`            | session | returns session row, in-flight prompts, and recent events      |

`POST /v1/sessions/{id}/prompt` is asynchronous. Clients can poll the prompt status endpoint or subscribe to `sessions.{id}` over WebSocket.

Prompt status values are `pending`, `running`, `completed`, `errored`, `cancelled`, and `stalled`. `stalled` is a terminal status reached only when the stale-prompt sweeper observes no ACP `session/update` activity for longer than `[prompts].stale_threshold`. From the client's perspective, a `stalled` prompt is final: it will not transition back to `running`, and recovery means submitting a new prompt. See `docs/specs/runtime.md` for the sweeper contract.

`GET /v1/sessions/{id}/snapshot` is the reconnect-bootstrap helper. The response carries:

- `session` — full session row (id, status, agent id, cwd, title, metadata).
- `in_flight_prompts` — prompts currently in `pending` or `running`. Empty when the session is idle. Each entry is the same shape returned by `GET /v1/sessions/{id}/prompts/{prompt_id}`.
- `last_event_id` — the id of the newest persisted session event, or `null` when the session has no events. Acts as a tail cursor for forward catch-up: callers fetch events newer than the snapshot via `GET /v1/sessions/{id}/events?after=last_event_id`, which paginates forward on `(created_at, id)` ascending.
- `recent_events` — the latest session events, newest-first, capped at 50. The cap is enforced by `SNAPSHOT_RECENT_EVENTS_LIMIT` in `src/api/routes/sessions.rs` and is sized to cover one prompt-turn's worth of updates without bloating the response.

The intended reconnect flow is: `GET snapshot` once to recover state, subscribe to `sessions.{id}` over WebSocket, then use `GET events?after=last_event_id` to catch up on any events that landed between the snapshot read and the WebSocket subscribe. For deeper history (older than the 50-event snapshot window), additional pagination is not currently exposed; older events are reachable only through the durable logs endpoints.

Session list filters accept `limit`, time bounds, and range values. Duration suffixes such as `30m`, `12h`, `60d`, `8w`, `6mo`, and `1y` are interpreted relative to request time.

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

Command status values are `pending`, `running`, `exited`, `failed`, and `canceled`. Command records include `last_output_event_id`, `last_output_at`, `last_output_seq`, `output_bytes`, and `last_progress_at` for reconnect and liveness checks.

Output is persisted up to the configured byte cap and streamed on the command WebSocket topic while the command runs. `GET /v1/commands/{id}/output` accepts `limit`, `after`, and `order=asc|desc` and returns `{ chunks, next_cursor }`. Each chunk is shaped as `{ event_id, created_at, command_id, stream, seq, data }`.

The reconnect flow is: read `GET /v1/commands/{id}`, subscribe to `commands.{id}`, then query `/output?order=asc&after=<last-seen-event-id>` to catch chunks missed between the HTTP read and WebSocket subscribe.

## Permissions

| Route                               | Tier    | Contract                                   |
| ----------------------------------- | ------- | ------------------------------------------ |
| `GET /v1/permissions/pending`       | session | lists pending requests                     |
| `POST /v1/permissions/{id}/approve` | session | approves a request                         |
| `POST /v1/permissions/{id}/deny`    | session | denies a request                           |
| `POST /v1/permissions/{id}/cancel`  | session | cancels a request owned by the caller flow |

Permission requests are created by ACP permission callbacks and by mediated commands when policy requires review.

## Dependencies

| Route                 | Tier    | Contract                           |
| --------------------- | ------- | ---------------------------------- |
| `GET /v1/deps`        | session | reports declared dependency status |
| `POST /v1/deps/check` | session | re-checks dependency status        |
| `POST /v1/deps/apply` | admin   | runs declared install actions      |

The runtime never invents package-manager commands. Only install actions declared in config can be applied.

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
| `GET /v1/logs/security`      | admin   | returns security events; `order=` applies to both result streams |
| `GET /v1/logs/sessions`      | session | returns session-scoped history; supports `order=`             |
| `GET /v1/metrics/summary`    | session | returns aggregate metrics for a time window                   |

Log query filters include `limit`, `level`, `kind`, `source`, `session_id`, `command_id`, `permission_id`, `category`, `since`, `until`, `after`, and `order`. `order` accepts `asc` or `desc` (default `desc`). On `/v1/logs/security`, `order` applies to both `auth_failures` and `events`; `category` accepts the security-category labels documented in `docs/specs/state-logging.md` and constrains only the `events` stream.

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
