# ACP Bridge Spec

This document describes how `acp-stack` interacts with ACP-compatible agents. It does not redefine the upstream ACP protocol; it defines the runtime bridge, capability handling, event persistence, and integration boundaries around ACP.

## ACP Bridge

`acp-stack` acts as an ACP client. It launches the configured agent as a subprocess and communicates over ACP JSON-RPC via stdio.

The configured subprocess may be either:

- a native ACP agent, such as agents listed by the ACP registry as directly implementing ACP
- an ACP adapter executable, such as `codex-acp`, that speaks ACP to `acp-stack` and wraps an upstream agent that does not speak ACP directly

`acp-stack` does not implement agent-specific adapters itself. It installs and launches registry-distributed ACP agent or adapter executables, then treats the resulting process as the ACP protocol peer. The Zed ACP ecosystem page identifies which agents are native and which are available via adapters; the `agentclientprotocol/registry` repository is the install source of truth for registry entries such as `codex-acp`.

As of 2026-05-15, the Zed ACP ecosystem page marks Claude Agent and Codex CLI with `Via Adapter`, and the Pi agent page describes Pi as an ACP adapter. These are compatibility facts from the external ecosystem, not hard-coded runtime policy; `acps` should rely on the registry entry it resolves at install time.

### Initialization

On agent start:

1. Spawn the configured agent command.
2. Inject only the secrets referenced by `[agent].env`.
3. Set cwd to `agent.cwd` or `workspace.root`.
4. Send ACP `initialize` with client capabilities.
5. Record agent capabilities in SQLite.
6. Emit agent lifecycle events over WebSocket.

### Sessions

Session API calls map to ACP session methods:

- create -> `session/new`
- load -> `session/load`
- resume -> `session/resume`
- close -> `session/close`
- prompt -> `session/prompt`
- cancel -> `session/cancel`

If the agent lacks a capability, `acp-stack` returns a typed API error instead of emulating behavior poorly.

### Streaming

ACP `session/update` notifications are:

- forwarded over WebSocket
- normalized into the event model
- persisted in SQLite
- associated with session ID, agent ID, and timestamps

### MCP Servers

MCP servers declared in config are launched or referenced during session creation where the agent supports MCP.

0.0.2 supports:

- stdio MCP server declarations
- HTTP MCP server declarations
- secret interpolation for env vars and HTTP headers
- dependency status reporting for MCP commands

MCP examples should focus on external services such as Slack, Linear, GitHub, or databases. Filesystem MCP is not a primary example because the agent already has workspace access through bash and the `acp-stack` Workspace API.

## Permission Passthrough

ACP permission requests enter the same durable permission pipeline as stack-mediated commands. The runtime persists each request, publishes it over WebSocket, exposes it through the permissions API, records the decision, and then resumes, rejects, or times out the blocked ACP operation.

See [project-spec](../project-spec.md) and [api](../api/api.md#permissions-api) for the shared permission lifecycle and HTTP contract.
