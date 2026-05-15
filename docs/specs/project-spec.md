# Project Spec

`acp-stack` is a standalone, self-hostable Linux runtime for ACP-compatible agents.

It turns a Linux machine into a reusable agent workspace: one config describes the workspace, agent command, MCP servers, dependencies, secrets, permission policy, API settings, and logging behavior. The runtime exposes that environment through a versioned HTTP and WebSocket API that clients can use.

ACP defines how clients and agents communicate. `acp-stack` supplies the surrounding runtime layer while keeping ACP as the protocol boundary.

## Detailed Specs

- [api](api/api.md) defines the HTTP and WebSocket client contract.
- [acp-bridge](acp/acp-bridge.md) defines how `acp-stack` launches and communicates with ACP-compatible agents.
- [acpctl](acpctl/acpctl.md) defines the local agent-facing control interface.
- [config](config.md) defines portable TOML config, default paths, import, and export behavior.
- [cli](cli.md) defines the `acps` command surface.
- [runtime](runtime.md) defines process supervision, agent installation, workspace behavior, dependency checks, and self-hosting flow.
- [security](security.md) defines API keys, secrets, permissions, HTTP hardening, self-checks, and runtime-user boundaries.
- [state-logging](state-logging.md) defines SQLite state, portable SQL schema, local logs, metrics, and Supabase logging.
- [messaging-clients](messaging-clients.md) defines future chat-platform integrations that use the HTTP and WebSocket API.
- [architecture](../mgmt/architecture.md) describes the runtime modules and system boundaries.
- [tech-stack](../mgmt/tech-stack.md) records implementation technology choices.
- [roadmap](../mgmt/roadmap.md) tracks the early release line and acceptance criteria.

## Core Shape

- single Rust binary
- one active configured ACP agent per runtime
- versioned HTTP and WebSocket API under `/v1`
- portable TOML config with secret references, not secret values
- SQLite local state for sessions, events, commands, permissions, lifecycle records, and metrics
- age-compatible encrypted secret store
- daemon-mediated workspace file operations and shell commands
- ACP permission passthrough into the same durable permission pipeline as stack-mediated commands
- optional MCP server declarations passed to agents where supported
- optional external PostgreSQL/Supabase logging after local SQLite logging is established

## Runtime Boundaries

`acp-stack` owns the runtime environment around a configured ACP agent process. That process may be a native ACP agent or a registry-distributed adapter such as `codex-acp` that wraps an upstream agent. `acp-stack` does not redefine ACP, implement a new agent protocol, or replace the agent itself.

The daemon, agent, MCP servers, and mediated commands run as an unprivileged runtime user by default, normally `acp`. The workspace is ordinary filesystem storage owned by the deployment environment: container volume, VM disk, bare-metal disk, network storage, or hosted workspace volume.

The 0.0.x line is scoped to headless agents or adapters published through the ACP registry and compatible with direct API keys through environment variables or config files. Agents that require browser OAuth or interactive account login are unsupported in the initial line.

## Config And State

The config describes what the runtime should be. SQLite records what happened. The secret store contains secret values.

- `acp-stack.toml` is portable desired state.
- `state.sqlite` is instance-local runtime history.
- `secrets.age` and `age.key` are instance-local secret storage.

Default paths:

```text
~/.config/acp-stack/acp-stack.toml
~/.local/share/acp-stack/state.sqlite
~/.local/share/acp-stack/secrets.age
~/.config/acp-stack/age.key
```

## Release Line

### 0.0.1 - Local Runtime Foundation

Establish the daemon shape: config import/export, SQLite state, one configured agent, direct API-key compatibility, HTTP/WebSocket API, workspace file operations, mediated shell commands, logs, basic auth hardening, and CLI commands that exercise the same core services as the API.

### 0.0.2 - Secrets, Permissions, And MCP

Add age-backed secrets, scoped secret injection, declared MCP servers, dependency status reporting, permission requests and decisions, ACP permission passthrough, command policy enforcement, rate limiting, temporary auth-failure blocks, WebSocket origin checks, CORS allowlist, and common MCP examples.

### 0.0.3 - Portable Logging And Analytics

Add optional PostgreSQL/Supabase-compatible external logging, shared logical migrations, local `acpctl`, optional `acpctl mcp serve`, and derived operational metrics such as session duration, turn counts, command counts, permission response times, and reported token/context usage.

### 0.0.4 - Packaging And Deployment

Add Docker packaging, systemd installation, reverse proxy deployment guides, unprivileged runtime-user automation, config import/export hardening, supported dependency installation flows, security self-checks, and stronger `acpctl` permission/audit coverage.

### 0.0.5 - Client And Operations Polish

Add TypeScript and Python client SDKs, richer CLI UX, log query filters and pagination, command output streaming improvements, an MCP compatibility matrix, operational health checks, and security self-check history with remediation hints.

## Out Of Scope For 0.0.x

- multiple active agents per runtime
- broad cross-distro package/runtime reconciliation
- complete OS-level interception of arbitrary shell activity
- built-in TLS termination, persistent IP allowlists, and advanced edge/WAF policy
- snapshots and hibernation
- hosted fleet management
- billing and tenant management

## Acceptance Criteria

The early release line is successful when a user can install `acp-stack` on a Linux instance, initialize it, configure one direct-key ACP agent, add secrets without plaintext storage, export/import reusable config, validate dependencies and MCP declarations, start the daemon, create and prompt sessions, stream updates, operate on workspace files, run mediated commands, answer permission requests, and query durable logs and derived metrics.
