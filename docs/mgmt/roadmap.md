# Roadmap

## Release Goals

The `0.0.1` release is organized into five phases. Each phase should leave stable contracts behind rather than throwaway prototype behavior.

### Phase 1 - Local Runtime Foundation

Establish the local daemon shape:

- one active configured ACP agent per runtime
- durable sessions if the agent supports them
- versioned HTTP and WebSocket API
- portable config import/export
- SQLite runtime state
- direct API-key agent compatibility
- declared agent installer commands
- workspace file operations
- daemon-mediated shell commands
- event and command logs
- request size limits
- constant-time API key validation
- authentication failure logging
- CLI commands that exercise the same core services as the API

The initial installer foundation fetched the upstream ACP registry at runtime. A follow-up still inside the `0.0.1` release closes that gap with an intentionally narrow embedded catalog at `data/agents.toml`, starting with OpenCode, Cursor CLI, Amp, and Pi as verified headless targets; see `docs/todos/phase_1.md` under "Agent Registry & Two-Layer Install".

### Phase 2 - Secrets, Permissions, and MCP

Add the trust and integration layer:

- age-backed secrets
- scoped secret injection
- declared MCP servers
- curated MCP presets plus custom MCP server declarations
- dependency manifest validation and status reporting
- permission requests and decisions
- ACP permission passthrough
- command policy enforcement for daemon-mediated commands
- per-IP and per-key rate limiting
- temporary IP blocks after repeated authentication failures
- WebSocket origin checks
- CORS allowlist
- Slack, Linear, and HTTP MCP examples

### Phase 3 - Portable Logging and Analytics

Add optional external telemetry sinks while keeping SQLite as the local source of truth and preserving a portable relational schema:

- Supabase log sink
- PostgreSQL-compatible external logging schema
- shared logical SQL migration sequence
- local agent-facing `acpctl` CLI
- optional local `acpctl mcp serve` introspection server
- session duration metrics
- turn counts
- token usage when reported by the agent
- context window usage when reported by the agent
- command counts and command durations
- permission response times
- API and WebSocket connection summaries
- security event summaries

### Phase 4 - Packaging and Deployment

Make the runtime straightforward to deploy:

- Docker image
- systemd installer
- full init orchestration with resumable workspace code/data ingestion
- provider/model resolution from `models.dev` through the unified API, followed by atomic agent config writes and agent relaunch
- reverse proxy deployment guides
- unprivileged `acp` runtime user automation
- config import/export hardening
- supported dependency installation through `deps apply`
- real-prompt init testflight with explicit provider-credit warning
- dependency status improvements
- installer command status and retry UX
- security self-check CLI and API
- `acpctl` permission boundaries and audit coverage

### Phase 5 - Client and Operations Polish

Round out the standalone runtime surface:

- TypeScript client SDK
- Python client SDK
- richer CLI UX
- init selection and retry UX for code, data, MCP, secrets, and testflight
- log query filters and pagination
- command output streaming improvements
- MCP compatibility matrix
- basic operational health checks
- security self-check history and remediation hints

## Later Scope

The following are outside the Phase 1-5 scope for `0.0.1`:

- multiple active agents per runtime
- broad cross-distro package/runtime reconciliation
- complete OS-level interception of arbitrary shell activity
- built-in TLS termination, persistent IP allowlists, and advanced edge/WAF policy
- snapshots and hibernation
- hosted fleet management
- billing and tenant management

## Initial Release Success Criteria

The `0.0.1` release is successful when the acceptance criteria in the project spec are met. The full checklist is maintained in [Project Spec](../specs/project-spec.md#acceptance-criteria).

The initial release is successful when a user can:

1. Install `acp-stack` on a Linux instance.
2. Run `acps init`.
3. Install or configure one ACP agent that accepts direct API keys without OAuth.
4. Add secrets without writing plaintext to disk.
5. Export the reusable config as TOML.
6. Import that config on another instance.
7. Validate dependency and MCP declarations.
8. Start the daemon.
9. Create an agent session through CLI or HTTP.
10. Send a prompt and stream updates over WebSocket.
11. Browse, upload, download, read, and write workspace files.
12. Run a mediated shell command.
13. Receive and answer permission requests.
14. Query sessions, events, commands, and permission decisions from durable logs.
15. Enable Supabase logging and inspect session, turn, token, context, command, duration, and permission metrics externally.
