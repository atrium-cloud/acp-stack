# State And Logging

SQLite is the local source of truth for runtime history. External logging is optional and should be fed by the same normalized event stream.

## SQLite State

SQLite stores:

- schema migrations
- sessions
- session events
- agent capabilities
- command runs
- command output metadata
- permission requests
- permission decisions
- lifecycle events
- dependency check results
- WebSocket connection metadata where useful
- usage metrics derived from local events

The initial state implementation creates the local SQLite database at:

```text
~/.local/share/acp-stack/state.sqlite
```

It creates baseline tables for schema migrations, events, sessions, commands, agent lifecycle records, auth failures, and installer runs. The first user-facing durable records are local events written by `acps init`, `acps status`, and CLI error handling.

The `auth_failures` table records every rejected API key authentication. The attempted key value is never stored; rows carry the structural failure context only. Columns:

| column         | type | notes                                                                              |
| -------------- | ---- | ---------------------------------------------------------------------------------- |
| `id`           | TEXT | primary key; format `af_<nanos>_<seq>_<pid>`, sorts chronologically                 |
| `created_at`   | TEXT | RFC3339 UTC with 9-digit subseconds                                                |
| `key_kind`     | TEXT | `session`, `admin`, or `unknown`                                                   |
| `reason`       | TEXT | `missing`, `invalid`, `wrong_kind`, or `malformed_header`                          |
| `client_ip`    | TEXT | nullable; populated by the HTTP layer when available                               |
| `route`        | TEXT | nullable; the rejected route path                                                  |
| `payload_json` | TEXT | structured JSON payload, validated by SQLite's `json_valid` check constraint        |

`payload_json` is the extension point: future fields (rate-limit context, header parse details, etc.) land there without a column migration.

The migration runner follows the documented `migrations/` layout: `manifest.toml` lists each migration's `id`, `name`, and `sqlite_file`. The runner embeds the SQLite migration files in the binary via `include_str!` and applies any manifest entry whose version is not already recorded in `schema_migrations`. PostgreSQL dialect files (`{id:03}_{name}.postgres.sql`) arrive with the Supabase sink in Phase 3.

`acps init` creates the local config file when absent; `acps config import` writes one too, refusing to overwrite an existing config unless `--force` is passed. Both write atomically at owner-only permissions (Unix `O_CREAT | O_EXCL` with mode `0o600`). `acps status` requires an existing config and repairs its permissions on each run. All three of `init`, `status`, and `logs query` create or migrate the local SQLite file atomically at owner-only permissions when it is missing.

Initial event records use:

- stable string IDs
- RFC3339 UTC timestamps
- `level`
- `kind`
- `message`
- validated JSON payload text

`acps init` sets the `acp-stack` config and state directories to owner-only directory permissions on Unix systems, and sets the config and SQLite files to owner-only file permissions.

Older binaries must reject state databases that contain a schema migration version newer than the binary supports.

SQLite does not store:

- portable config
- plaintext secret values
- age private key
- workspace file contents except bounded event/output records

State should be queryable by the CLI and logs API.

## Portable SQL Schema

SQLite and PostgreSQL/Supabase should share the same logical schema and migration sequence. Dialect-specific SQL files are allowed, but migration IDs, table names, column names, event type names, and JSON payload contracts must remain stable so local data can be replayed, exported, mirrored, or migrated without transformation.

Migration layout:

```text
migrations/
  manifest.toml
  001_init.sqlite.sql
  002_auth_failures_schema.sqlite.sql
  003_agent_capabilities.sqlite.sql
  004_sessions.sqlite.sql
  005_commands_schema.sqlite.sql
  006_permissions.sqlite.sql
  007_events_source.sqlite.sql
  008_sink_outbox.sqlite.sql
  009_installer_runs_step.sqlite.sql
  010_installer_runs_version.sqlite.sql
  011_installer_runs_log_dir.sqlite.sql
  012_init_runs.sqlite.sql
```

Migration 006 introduces the `permission_requests` and `permission_decisions` tables that back the permissions module. Migration 007 adds a `source` column to `events` (default `system`) so log queries can filter by writer origin (`api`, `acp`, `command`, `permission`, `cli`, `local`). The `local` source is produced by the `acpctl` UDS listener (see `acpctl.md`): every call through the Unix-domain socket lands an `api.request` row with `source = "local"` and `key_kind = "local"` in the payload, and workspace mutations triggered through that path inherit the same source. Migrations 009–011 evolve `installer_runs` with per-step labels, the resolved installed version, and the on-disk log directory. Migration 012 introduces the `init_runs` and `init_steps` tables that back the `acps init` orchestrator: one `init_runs` row per invocation, plus `init_steps` rows for phases that execute or resume (`secrets_init`, `agent_install`, `provider_configure`, `workspace_materialize`, `agent_headless_config`, `edge_artifacts`, `init_complete`, `testflight`), each carrying its `status`, `started_at`, `finished_at`, optional `log_dir`, and a typed `(error_kind, error_detail)` tuple on failure. Resume semantics consult per-phase verifiers to replay any prior `succeeded` row as `skipped` when the postcondition still holds. Postgres equivalents are emitted on the same shared migration ids when external logging is enabled.

Rules:

- migration IDs and names are shared across dialects
- table names and column names are shared across dialects
- IDs are stable strings generated by `acp-stack`
- timestamps use one consistent representation across stores, preferably RFC3339 UTC text or epoch milliseconds
- event payloads use the same JSON shape everywhere
- SQLite stores JSON payloads as validated JSON text
- PostgreSQL/Supabase stores JSON payloads as `jsonb`
- Supabase-specific features must not be required by the core schema
- external sinks use upsert-by-ID so replay from SQLite is idempotent

## Local Logs

Local logs include:

- session lifecycle events
- prompt turns
- ACP session updates
- command runs
- command output metadata
- permission requests and decisions
- security events: auth failures, rate-limit hits, temporary IP blocks, denied origins, oversized requests
- agent lifecycle events
- dependency check results
- API and WebSocket connection summaries

## Usage Metrics

`acp-stack` should derive metrics from runtime events where possible:

- session duration
- prompt/turn count
- command count
- command duration
- command exit status
- permission response time
- connected client count
- token usage when reported by the agent
- context window usage when reported by the agent

Token and context usage are best-effort fields. If the configured agent does not expose them through ACP updates or prompt responses, the fields remain null rather than estimated.

### Implementation (Phase 3)

Derivation is done in SQLite at query time and exposed through `GET /v1/metrics/summary` over a `[since, until)` window. The window defaults to the last 24 hours; callers can pass `since` / `until` as RFC3339 timestamps or duration suffixes (`30m`, `1h`, `2d`, `1w`).

The summary covers:

- `counts` per logged table.
- `sessions.active` / `closed` plus average / p50 / p95 wall-clock duration of closed rows.
- `turns.total`, `turns.by_status`, and `turns.average_per_session`.
- `commands.total`, `commands.by_status`, average / p50 / p95 `duration_ms`, `truncated_count`.
- `permissions.total`, `permissions.by_outcome`, average / p50 / p95 response_ms (decision timestamp minus request timestamp).
- `security.auth_failures`, `security.by_reason`, `security.events_by_kind`.
- `api_connections.request_count` plus `2xx`/`4xx`/`5xx` buckets and average duration, derived from `api.request` audit events. The middleware skips `/v1/ws` and `/v1/status*` to keep cardinality bounded.
- `ws_connections.connections_opened` / `connections_closed` / `average_duration_ms`, derived from `ws.client_connected` and `ws.client_disconnected` events emitted by the WS handler.
- `usage.tokens_input` / `usage.tokens_output` / `usage.context_window_max`, aggregated from `usage.reported` events the ACP bridge persists whenever an inbound `session/update` carries a recognized usage shape (top-level `usage`, `update.usage`, `prompt_response.usage`, or `meta.usage`). When the agent never reports these fields, every usage value stays `null`.

Percentiles are computed in pure Rust from the windowed result set — SQLite has no `percentile_cont`. Windows up to ~tens of thousands of rows are comfortable; larger windows should add reservoir sampling rather than column materialization.

### Edge And Connection Metrics

Request and security events include bounded origin metadata. The same origin payload is available on `api.request`, `auth_failures`, rate-limit/IP-block events, denied HTTP/WebSocket origins, oversized requests, and WebSocket lifecycle events. Cloudflare metadata is trusted only when the socket peer passes `[security.http].trusted_proxies`; direct or untrusted requests are recorded as direct/unknown origins rather than silently dropped.

Planned summary additions:

- request counts by method, matched route, source, key kind, status bucket, origin kind, country code, and region code.
- response duration average / p50 / p95 for `api.request`.
- security-event counts by origin kind and country/region bucket.
- WebSocket live/session dimensions derived from connect/disconnect lifecycle rows and the process-local live registry.

The public metrics shape should remain additive: existing keys in `/v1/metrics/summary` must stay stable, while new origin and connection breakdowns are added as new fields.

## Supabase Sink

Phase 3 adds an optional Supabase sink for remote analytics and hosted dashboards. Supabase is treated as PostgreSQL plus useful hosted tooling, not as a separate data model.

Config:

```toml
[logging.supabase]
enabled = false
url = "https://example.supabase.co"
service_role_key_ref = "SUPABASE_SERVICE_ROLE_KEY"
schema = "acp_stack"
```

Behavior:

- disabled by default
- uses a secret reference for the Supabase service role key
- batches inserts from normalized local events
- uses the shared PostgreSQL migration sequence
- never exports plaintext secrets
- tolerates network failure without blocking agent/session execution
- records delivery failures locally for later inspection
- supports idempotent replay from SQLite for missed uploads

Initial Supabase tables:

- `sessions`
- `session_turns`
- `commands`
- `permissions`
- `agent_events`
- `security_events`
- `connection_events`
- `usage_metrics`

In the shipped Postgres dialect the raw tables (`sessions`, `commands`, `prompts`, `permission_requests`, `permission_decisions`, `events`, `auth_failures`, `agent_lifecycle`) are mirrored with the redacted outbound shapes below, and the eight analytics names above are exposed as `CREATE OR REPLACE VIEW`s authored alongside migration 007. The sink uploads the raw tables only; PostgREST honors the analytics views for read traffic.

## Sink delivery state

External delivery state lives in two local tables introduced in migration 008:

- `sink_outbox(id, source_table, source_id, created_at, status, attempts, next_attempt_at, last_error, last_attempt_at)` — one row per outbound source row. `id` is `"{source_table}:{source_id}"`. Status is `pending`, `sending`, `sent`, or `failed`. Updates to a source row UPSERT the outbox row back to `pending`; the worker re-uploads and Supabase's PostgREST `Prefer: resolution=merge-duplicates` collapses duplicates server-side.
- `sink_failures_summary(window_started_at, failure_count, last_error, last_observed_at)` — rolled forward every 60 seconds while the worker is failing. The `security check` self-check reads `latest_failure_summary` and surfaces a `logging.supabase.delivery_failing` finding whenever `sink_outbox` has rows with `attempts > 0`.

Per-table redaction enforces the spec's "no plaintext secrets in external sinks" rule before the worker POSTs. JSON allowlists keep only top-level scalar values; nested arrays or objects under allowlisted keys are dropped.

| Source table | Outbound shape |
| --- | --- |
| `events.payload_json` / `events.message` | payload keeps allowlisted scalar keys only (`session_id`, `kind`, `duration_ms`, `status`, `exit_code`, `input_tokens`, `output_tokens`, `context_window_max`, `agent_id`, `command_id`, `request_id`, `bind`, `client_label`, `reason_code`); other keys and nested values are dropped; message text replaced with `"[redacted; N bytes]"` |
| `sessions.metadata_json` / session free text | metadata keeps scalar `agent_id`; `title` is nulled; `cwd` is replaced with `""` to satisfy the Postgres mirror's non-null column |
| `prompts.prompt_json` / prompt error scalars | prompt JSON replaced wholesale with `{ "redacted": true, "byte_len": N }`; `stop_reason` and `error_message` replaced with `"[redacted; N bytes]"` |
| `commands.command` | replaced with `"[redacted; N bytes]"` (inline credentials are common in shell invocations) |
| `commands.cwd` | nulled (may reveal `/var/secrets/...`-style paths) |
| `commands.env_json` | replaced with `{ "env_var_count": N }` |
| `permission_requests.detail_json` | same scalar allowlist as events |
| `auth_failures.payload_json` / request path | payload dropped to `{}`; known structural `reason` codes (`missing`, `invalid`, `wrong_kind`, `malformed_header`) kept; unknown reasons replaced with `"[redacted; N bytes]"`; `route` nulled |
| `agent_lifecycle.payload_json` | scalar allowlist of `{ bind, agent_id, exit_code, duration_ms, capabilities_hash }`; free-form `message` replaced with `"[redacted; N bytes]"` |
| `permission_decisions` | scalar columns only; free-form `reason` replaced with `"[redacted; N bytes]"` |

Unknown source tables are rejected with `StackError::SupabaseSinkUnknownTable` so accidentally extending the outbox to a new table fails closed rather than leaking columns.
