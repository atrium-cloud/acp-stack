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

The current Rust crate exposes a library behind the `acps` binary. The implemented foundation includes focused `cli`, `config`, `error`, `state`, `tracing_init`, `auth`, `secrets`, `envelope`, `fs_util`, `api`, and `supervisor` modules; ACP bridge, workspace, and command gateway modules will be added with their first real behavior rather than as empty placeholders. `api` owns the axum HTTP layer (router, auth middleware, response envelope wiring) and `supervisor` records the daemon's lifecycle transitions into `agent_lifecycle`.

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
