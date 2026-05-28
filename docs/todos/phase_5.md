# Phase 5 Todo - Client And Operations Polish

## References

- [project-spec](../specs/project-spec.md)
- [api](../specs/api/api.md)
- [acp-bridge](../specs/acp/acp-bridge.md)
- [acpctl](../specs/acpctl/acpctl.md)
- [roadmap](../mgmt/roadmap.md)

## CLI UX

- [x] Add `acps init --edge cloudflare --exposure tunnel` as the recommended public deployment profile.
- [x] Add generated Cloudflare Tunnel artifact output for `cloudflared` config and systemd/Docker snippets.
- [ ] Add optional managed Cloudflare provisioning mode using secret refs for Cloudflare API credentials.
- [x] Improve `acps status` with daemon, agent, workspace, dependency, and sink health.
- [ ] Add human-readable and JSON output modes for common commands.
- [ ] Add consistent `--format text|json` support.
- [ ] Add clearer command errors with remediation hints.
- [ ] Add progress output for long-running installer, dependency, import, and export operations.
- [ ] Add richer init selection UX for code sources, data sources, MCP presets, custom MCP declarations, and required secrets.
- [x] Add init-time Agent Skills source and skill selection backed by the trusted skills catalog.
- [ ] Add provider/model selection UX on top of the Phase 4 `models.dev` resolver, including clear filtering by available secret refs.
- [x] Add `acps agent switch` UX with target install preview, compatible provider reuse, secret-ref migration, and model follow-up gating.
- [ ] Add retry/reporting polish for failed init, dependency, ingestion, and testflight steps.
- [ ] Add shell completion generation.
- [ ] Add help examples for common workflows.

## Log Query Polish

- [x] Add stable pagination cursors.
- [x] Add filters by event kind, source, session, command, permission, security category, and time range.
- [x] Add sorting options where safe.
- [x] Add `acps logs query --json`.
- [x] Add `acps logs query --follow` where supported.
- [x] Add tests for pagination stability during concurrent writes.

## Command Output Streaming

- [x] Improve stdout/stderr chunk framing.
- [x] Include stream name, sequence number, timestamp, and command ID in output chunks.
- [x] Persist command output chunks durably.
- [x] Support command cancellation with final status event.
- [x] Ensure WebSocket consumers can resume querying persisted output after disconnect.
- [x] Emit periodic running/progress events for long-running commands even when stdout/stderr are quiet.
- [x] Persist enough command-progress state for clients to distinguish quiet work from a stalled runtime.

## Session Progress And Reconnect

- [x] Persist prompt lifecycle states with explicit `pending|running|completed|errored|cancelled|stalled` or equivalent states.
- [x] Add stale-prompt detection for sessions that stop emitting ACP updates before terminal state.
- [x] Sync ACP `session/list` discovery into durable session history and expose discovered sessions as `available`.
- [x] Ensure clients can reconnect and recover current prompt/session state from HTTP without relying on live WebSocket history.
- [x] Document behavior for ACP `session/resume`, `session/load`, and unsupported resume paths per supported agent.
- [x] Classify prompt failures caused by model/inference endpoint HTTP 5xx responses separately from VM, daemon, and agent runtime failures.
- [x] Persist inference endpoint HTTP failure details as sanitized prompt error codes and session-scoped events without storing provider URLs, headers, bodies, or secrets.

## MCP Compatibility Matrix

- [x] Define compatibility matrix format.
- [x] Include curated MCP preset compatibility notes.
- [x] Document tested stdio MCP server behavior.
- [x] Document tested HTTP MCP server behavior.
- [x] Include Slack MCP compatibility notes.
- [x] Include Linear MCP compatibility notes.
- [x] Include generic HTTP MCP compatibility notes.
- [x] Document whether MCP declarations are runtime-wide or session-scoped in the initial release.
- [x] Record per-session MCP as future/unsupported unless added deliberately.
- [x] Record known unsupported MCP features and workarounds.
- [x] Add a centralized agent skills catalog after identifying trusted official sources.

## Operational Health

- [x] Add basic liveness endpoint.
- [x] Add readiness endpoint.
- [x] Add Cloudflare Tunnel posture checks when `[edge.cloudflare]` is configured.
- [x] Add health checks for agent process state.
- [x] Add health checks for SQLite access.
- [x] Add health checks for workspace access.
- [x] Add health checks for external logging sink when enabled.
- [x] Add health checks for configured MCP declarations.
- [x] Add health checks for orphaned agent/adapter processes.
- [x] Add health checks for prompts stuck without progress beyond a configured threshold.

## Observability

- [x] Expose metrics that let operators distinguish inference endpoint 5xx failures from local VM, SQLite, daemon, and agent-process health issues. Aggregation lives in the `events` table: `prompt.inference_failed` (sanitized payload) and `prompt.errored` are the metric source per the Phase 5 deferred-counter decision.
- [x] Include inference endpoint failure counters in the future observability dashboard data model. Closed via the `prompt.inference_failed` event kind with its structured `{ prompt_id, status_code, reason_category }` payload; future dashboards count from there.
- [x] Enrich `api.request`, auth-failure, rate-limit, denied-origin, oversized-request, and WebSocket lifecycle events with bounded request-origin metadata.
- [x] Trust Cloudflare request metadata only after normal trusted-proxy validation (`CF-Connecting-IP`, `CF-IPCountry`, `CF-Ray`, and optional visitor-location headers).
- [x] Add request/response counts by method, route, status bucket, key kind, source, origin kind, country, and region.
- [x] Add live WebSocket connection registry with connection IDs, topics, derived `sessions.{id}` subscriptions, origin metadata, and disconnect reason tracking.
- [x] Add admin `acps` commands to list WebSocket connections, list unique subscribed session IDs, disconnect selected connections, and disconnect all connections for selected session IDs.
- [x] Add read-only `acpctl` and `acpctl mcp serve` surfaces for sanitized WebSocket connection/session reporting.

## Security Self-Check History

- [x] Persist each security self-check run.
- [x] Track check status, severity, details, and remediation hint.
- [x] Add API route for security check history.
- [x] Add `acps security history`.
- [x] Add remediation text for key, file permission, origin, CORS, proxy, and sink findings (every currently-emitted finding code carries a non-empty operator hint; category mapping recorded in `docs/specs/security.md`).
- [x] Add a dependency self-check finding with remediation.
- [x] Add Cloudflare posture findings for public binds in tunnel mode, missing local trusted proxies, unsafe origins, missing `cloudflared`, absent Cloudflare headers after edge traffic, and direct non-Cloudflare public requests.

## Initial Release Acceptance

- [ ] A user can install `acp-stack` on a Linux instance.
- [ ] A user can run `acps init`.
- [ ] A user can configure one ACP agent that accepts direct API keys.
- [ ] A user can add secrets without writing plaintext to disk.
- [ ] A user can export and import reusable TOML config.
- [x] A user can validate dependency and MCP declarations.
- [ ] A user can start the daemon.
- [ ] A user can create an agent session through CLI or HTTP.
- [ ] A user can send a prompt and stream updates over WebSocket.
- [ ] A user can browse, upload, download, read, and write workspace files.
- [ ] A user can run a mediated shell command.
- [ ] A user can receive and answer permission requests.
- [ ] A user can query sessions, events, commands, and permission decisions from durable logs.
- [ ] A user can enable Supabase logging and inspect derived metrics externally.
