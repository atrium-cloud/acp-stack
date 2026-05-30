# Phase 3 Todo - Portable Logging And Analytics

## References

- [project-spec](../specs/project-spec.md)
- [api](../specs/api/api.md)
- [acpctl](../specs/acpctl/acpctl.md)
- [roadmap](../mgmt/roadmap.md)

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

## Local Agent CLI

- [x] Implement `acpctl status`.
- [x] Implement `acpctl security check`.
- [x] Implement `acpctl deps check`.
- [x] Implement `acpctl logs query --since <duration>`.
- [x] Implement `acpctl workspace list <path>`.
- [x] Implement `acpctl workspace read <path>`.
- [x] Implement `acpctl workspace write <path>`.
- [x] Implement `acpctl command run <command>`.
- [x] Implement `acpctl config export`.
- [x] Implement `acpctl permissions pending`.
- [x] Ensure `acpctl` uses a local capability mechanism rather than public session/admin API keys.
- [x] Ensure `acpctl` actions are logged with source `local`.

## Local MCP Introspection

- [x] Implement `acpctl mcp serve`.
- [x] Expose status, dependency, log, workspace, command, config export, and pending-permission tools.
- [x] Enforce the same permission and logging rules as the `acpctl` CLI.
- [x] Prevent agents from reading secret values through the local MCP interface.
- [x] Prevent agents from rotating API keys through the local MCP interface.
- [x] Prevent agents from disabling permissions, rate limits, origin checks, or security logging.

## Acceptance

- [x] SQLite remains the local source of truth when external logging is enabled.
- [x] Supabase logging can be enabled and inspected.
- [x] Derived session, turn, token, context, command, duration, permission, API, WebSocket, and security metrics are queryable.
- [x] Agents can use `acpctl` for constrained local inspection.
- [x] `acpctl mcp serve` exposes the same constrained local interface through MCP.
