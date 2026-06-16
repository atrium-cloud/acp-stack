# Phase 3 Todo - Portable Logging And Analytics

## References

- [project-spec](../../specs/project-spec.md)
- [api](../../specs/api/api.md)
- [roadmap](../../mgmt/roadmap.md)

## External Logging Schema

- [x] Define the logical migration sequence shared by SQLite and PostgreSQL-compatible sinks.
- [x] Add schema for sessions, turns, events, commands, permissions, security events, lifecycle records, and derived metrics.
- [x] Document required indexes for local query performance and Supabase/PostgreSQL mirrors.
- [x] Add migration compatibility tests between SQLite and PostgreSQL DDL where practical.

## Supabase Logging Sink

- [x] Add `[logging.supabase]` config validation.
- [x] Resolve `api_key_ref` from the secret store only when external logging is enabled.
- [x] Batch or stream normalized events to Supabase.
- [x] Retry transient sink failures without blocking local SQLite writes.
- [x] Persist sink delivery status and failure summaries locally.
- [x] Ensure external sink payloads never include secret values.

## Metrics

- [x] Derive session duration metrics.
- [x] Derive turn counts.
- [x] Capture token usage when reported by the agent.
- [x] Capture context window usage when reported by the agent.
- [x] Derive command counts and command durations.
- [x] Derive permission response times.
- [x] Derive API connection summaries.
- [x] Derive WebSocket connection summaries.
- [x] Derive security event summaries.
- [x] Implement `GET /v1/metrics/summary`.

## Log Query UX

- [x] Add query filters for time range, source, session ID, command ID, permission ID, and event kind.
- [x] Add pagination for event, command, permission, security, and session logs.
- [x] Implement `acps logs query --since <duration>`.
- [x] Implement `acps logs query --session <session-id>`.
- [x] Implement `acps logs query --kind <kind>`.

## Local Observability

- [x] Implement keyless local `acps status`.
- [x] Implement keyless local `acps security check`.
- [x] Implement keyless local `acps deps check`.
- [x] Implement keyless local `acps logs query --since <duration>`.
- [x] Implement keyless local `acps metrics summary`.
- [x] Implement keyless local `acps sessions list`.
- [x] Implement keyless local `acps sessions status`.
- [x] Ensure daemon-backed local observability uses the internal Unix socket rather than public session/admin API keys.
- [x] Ensure local socket actions are logged with source `local`.

## Local Socket Restrictions

- [x] Prevent local socket callers from reading secret values.
- [x] Prevent local socket callers from rotating API keys.
- [x] Prevent local socket callers from importing or exporting config.
- [x] Prevent local socket callers from mutating workspace files, commands, permissions, or sessions.
- [x] Prevent local socket callers from disabling permissions, rate limits, origin checks, or security logging.

## Acceptance

- [x] SQLite remains the local source of truth when external logging is enabled.
- [x] Supabase logging can be enabled and inspected.
- [x] Derived session, turn, token, context, command, duration, permission, API, WebSocket, and security metrics are queryable.
- [x] Keyless local `acps` views cover constrained local inspection.
