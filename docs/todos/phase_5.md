# Phase 5 Todo - Client And Operations Polish

## References

- [project-spec](../specs/project-spec.md)
- [api](../specs/api/api.md)
- [acp-bridge](../specs/acp/acp-bridge.md)
- [acpctl](../specs/acpctl/acpctl.md)
- [roadmap](../mgmt/roadmap.md)

## TypeScript SDK

- [ ] Generate or hand-write typed API client models.
- [ ] Implement bearer-key auth support.
- [ ] Implement config, agent, sessions, workspace, commands, permissions, logs, deps, status, and metrics clients.
- [ ] Implement WebSocket subscription helper.
- [ ] Add retry and timeout options.
- [ ] Add TypeScript examples for session prompt streaming and permission handling.
- [ ] Publish package metadata and build scripts.

## Python SDK

- [ ] Generate or hand-write typed API client models.
- [ ] Implement bearer-key auth support.
- [ ] Implement config, agent, sessions, workspace, commands, permissions, logs, deps, status, and metrics clients.
- [ ] Implement WebSocket subscription helper.
- [ ] Add retry and timeout options.
- [ ] Add Python examples for session prompt streaming and permission handling.
- [ ] Publish package metadata and build scripts.

## CLI UX

- [ ] Improve `acps status` with daemon, agent, workspace, dependency, and sink health.
- [ ] Add human-readable and JSON output modes for common commands.
- [ ] Add consistent `--format text|json` support.
- [ ] Add clearer command errors with remediation hints.
- [ ] Add progress output for long-running installer, dependency, import, and export operations.
- [ ] Add shell completion generation.
- [ ] Add help examples for common workflows.

## Log Query Polish

- [ ] Add stable pagination cursors.
- [ ] Add filters by event kind, source, session, command, permission, security category, and time range.
- [ ] Add sorting options where safe.
- [ ] Add `acps logs query --json`.
- [ ] Add `acps logs query --follow` where supported.
- [ ] Add tests for pagination stability during concurrent writes.

## Command Output Streaming

- [ ] Improve stdout/stderr chunk framing.
- [ ] Include stream name, sequence number, timestamp, and command ID in output chunks.
- [ ] Persist command output chunks durably.
- [ ] Support command cancellation with final status event.
- [ ] Ensure WebSocket consumers can resume querying persisted output after disconnect.

## MCP Compatibility Matrix

- [ ] Define compatibility matrix format.
- [ ] Document tested stdio MCP server behavior.
- [ ] Document tested HTTP MCP server behavior.
- [ ] Include Slack MCP compatibility notes.
- [ ] Include Linear MCP compatibility notes.
- [ ] Include generic HTTP MCP compatibility notes.
- [ ] Record known unsupported MCP features and workarounds.

## Operational Health

- [ ] Add basic liveness endpoint.
- [ ] Add readiness endpoint.
- [ ] Add health checks for agent process state.
- [ ] Add health checks for SQLite access.
- [ ] Add health checks for workspace access.
- [ ] Add health checks for external logging sink when enabled.
- [ ] Add health checks for configured MCP declarations.

## Security Self-Check History

- [ ] Persist each security self-check run.
- [ ] Track check status, severity, details, and remediation hint.
- [ ] Add API route for security check history.
- [ ] Add `acps security history`.
- [ ] Add remediation text for key, file permission, origin, CORS, proxy, dependency, and sink findings.

## Version-Line Acceptance

- [ ] A user can install `acp-stack` on a Linux instance.
- [ ] A user can run `acps init`.
- [ ] A user can configure one ACP agent that accepts direct API keys.
- [ ] A user can add secrets without writing plaintext to disk.
- [ ] A user can export and import reusable TOML config.
- [ ] A user can validate dependency and MCP declarations.
- [ ] A user can start the daemon.
- [ ] A user can create an agent session through CLI or HTTP.
- [ ] A user can send a prompt and stream updates over WebSocket.
- [ ] A user can browse, upload, download, read, and write workspace files.
- [ ] A user can run a mediated shell command.
- [ ] A user can receive and answer permission requests.
- [ ] A user can query sessions, events, commands, and permission decisions from durable logs.
- [ ] A user can enable Supabase logging and inspect derived metrics externally.
