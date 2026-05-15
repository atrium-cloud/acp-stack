# Architecture

This document captures the management-level architecture for `acp-stack`. For concrete API routes and runtime behavior, see [api](../specs/api/api.md) and [project-spec](../specs/project-spec.md).

## Overview

`acp-stack` is a single Rust binary with a modular internal architecture:

```text
+---------------------------------------------+
|                 Unified API                 |
|             HTTP + WebSocket v1             |
+---------------------------------------------+
| Auth | Config | Status | Logs | Permissions |
+-----------------------+---------------------+
| Workspace + Commands  | Agent Sessions      |
| Files | Uploads | Sh  | ACP Bridge          |
+-----------------------+---------------------+
| Runtime Supervisor | MCP Launcher | Secrets |
+---------------------------------------------+
| SQLite State | Config TOML | Age Secret Key |
+---------------------------------------------+
                    |
                    v
             Linux Environment
        Docker / VM / Bare Metal / Hosted
```

### Core Modules

- `Config` - reads, validates, imports, exports, and applies `acp-stack.toml`.
- `API` - axum HTTP routes and WebSocket event streaming.
- `Auth` - two-tier API key validation and request authorization.
- `State` - SQLite migrations and repositories for sessions, events, commands, permissions, and lifecycle records.
- `ACP Bridge` - launches the configured agent and speaks ACP JSON-RPC over stdio.
- `Runtime Supervisor` - owns process lifecycle for the active agent and MCP server processes.
- `Workspace` - bounded file operations, uploads/downloads, and workspace path policy.
- `Command Gateway` - launches shell commands through `acp-stack`, evaluates policy, records output, and creates permission requests when needed.
- `Secrets` - age key management, encrypted secret store, secret references, and scoped env injection.
- `Dependencies` - validates declared tools/runtimes/packages and reports missing items.
- `Permissions` - durable request/decision lifecycle for ACP permission requests and stack-mediated commands.
- `Events` - normalizes WebSocket messages and durable event records.

The current Rust crate exposes a library behind the `acps` binary. The implemented foundation includes focused `cli`, `commands`, `config`, `deps`, `error`, `events`, `http_hardening`, `mcp`, `permissions`, `state`, `time_util`, `tracing_init`, `auth`, `secrets`, `envelope`, `fs_util`, `api`, `supervisor`, `acp_bridge`, `agent_installer`, and `workspace` modules. `time_util` parses operator-facing duration suffixes (`30m` / `1h` / `2d` / `1w`) used by `acps logs query` and the metrics summary endpoints; the rest of the runtime continues to use chrono's RFC3339 helpers directly. `permissions` owns the durable permission lifecycle (request/decide/cancel/expire) and the in-process waiter map that resolves blocked operations once a decision lands; `deps` reports declared dependency status without installing; `mcp` resolves `[mcp.servers]` entries against the secret store and hands the SDK `McpServer` list to the bridge at session create/load/resume time; `http_hardening` owns `client_ip` selection under trusted proxies, the CORS layer construction, the WebSocket Origin allowlist check, and the in-process auth-failure IP blocker that short-circuits brute-force attempts before bearer comparison. `commands` owns the Command Gateway: it evaluates `[permissions]` glob policy, spawns shell children through `[workspace].default_shell -c`, streams stdout/stderr through `EventHub` to `commands.{id}` subscribers, persists bounded output chunks to the `events` table, and handles cancel/timeout via process-group signals. `api` owns the axum HTTP/WebSocket layer (router, auth middleware, response envelope wiring, `/v1/ws` subscription handling), `events` owns the in-process broadcast hub and stable live event envelope, `supervisor` records the daemon's lifecycle transitions and owns the spawned ACP agent's lifecycle (`AgentSupervisor`), `acp_bridge` wraps the `agent-client-protocol` SDK to spawn and initialize the configured agent, `agent_installer` runs the operator-declared install recipe, and `workspace` provides the workspace-path resolver and the list/read/write/upload/delete primitives behind `/v1/workspace` and `/v1/files*`.

`AgentSupervisor` also owns the in-flight prompt registry (`HashMap<PromptId, PromptHandle>`). Each `POST /v1/sessions/{id}/prompt` enqueues a fire-and-forget background task that drives ACP `session/prompt` to completion and writes a terminal row into the `prompts` table; `session/cancel` fires the per-prompt `CancellationToken`. `acp_bridge` retains a cloneable `ConnectionTo<Agent>` handle once `initialize` completes so session dispatchers can call `session/new`, `session/load`, `session/resume`, `session/close`, `session/prompt`, and `session/cancel` without holding the supervisor's state lock across the agent's response. Incoming `session/update` notifications are persisted into `events` keyed by `session_id` via a `SessionEventSink` trait, then published live through the `events` broadcast hub to `/v1/ws` subscribers on `sessions.{session_id}`. SQLite remains the durable history source; WebSocket fanout is live only, with current producers for sessions, commands, workspace mutations, agent lifecycle, runtime status, and generic logs.

### Config vs State

The config describes what the runtime should be.

SQLite records what happened.

The age secret store contains secret values.

These three layers must remain separate:

- `acp-stack.toml` - portable desired environment
- `state.sqlite` - instance-local sessions, events, command runs, permission decisions, and lifecycle data
- `secrets.age` plus `age.key` - instance-local secret values and decrypt key

## Runtime Boundaries

- The runtime is a single Rust binary that supervises one configured ACP agent per runtime.
- The daemon, agent, MCP servers, and mediated commands run as the unprivileged runtime user by default.
- Config describes desired state, SQLite records runtime history, and the age-backed store holds secret values.
- External telemetry sinks consume the same normalized event stream as local SQLite logging.
