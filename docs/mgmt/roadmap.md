# Roadmap

## Release Goals

The project should progress through five early versions. Each version should leave stable contracts behind rather than throwaway prototype behavior.

### 0.0.1 - Local Runtime Foundation

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

The initial installer foundation fetched the upstream ACP registry at runtime. A follow-up still inside 0.0.x closes that gap with an intentionally narrow embedded catalog at `data/registry.toml`, starting with OpenCode as the first verified headless target while preserving the model needed for future adapter-backed agents; see `docs/todos/phase_1.md` under "Agent Registry & Two-Layer Install".

### 0.0.2 - Secrets, Permissions, and MCP

Add the trust and integration layer:

- age-backed secrets
- scoped secret injection
- declared MCP servers
- dependency manifest validation and status reporting
- permission requests and decisions
- ACP permission passthrough
- command policy enforcement for daemon-mediated commands
- per-IP and per-key rate limiting
- temporary IP blocks after repeated authentication failures
- WebSocket origin checks
- CORS allowlist
- Slack, Linear, and HTTP MCP examples

### 0.0.3 - Portable Logging and Analytics

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

### 0.0.4 - Packaging and Deployment

Make the runtime straightforward to deploy:

- Docker image
- systemd installer
- reverse proxy deployment guides
- unprivileged `acp` runtime user automation
- config import/export hardening
- supported dependency installation through `deps apply`
- dependency status improvements
- installer command status and retry UX
- security self-check CLI and API
- `acpctl` permission boundaries and audit coverage

### 0.0.5 - Client and Operations Polish

Round out the standalone runtime surface:

- TypeScript client SDK
- Python client SDK
- richer CLI UX
- log query filters and pagination
- command output streaming improvements
- MCP compatibility matrix
- basic operational health checks
- security self-check history and remediation hints

## Later Scope

The following are outside the 0.0.1 to 0.0.5 scope:

- multiple active agents per runtime
- broad cross-distro package/runtime reconciliation
- complete OS-level interception of arbitrary shell activity
- built-in TLS termination, persistent IP allowlists, and advanced edge/WAF policy
- snapshots and hibernation
- hosted fleet management
- billing and tenant management

## Version-Line Success Criteria

The early version line is successful when the version 0.0.5 acceptance criteria in the project spec are met. The full checklist is maintained in [Project Spec](../specs/project-spec.md#acceptance-criteria).

Version 0.0.5 is successful when a user can:

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
