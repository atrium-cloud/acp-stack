# ACP Bridge Spec

This document describes how `acp-stack` interacts with ACP-compatible agents. It does not redefine the upstream ACP protocol; it defines the runtime bridge, capability handling, event persistence, and integration boundaries around ACP.

## ACP Bridge

`acp-stack` acts as an ACP client. It launches the configured agent as a subprocess and communicates over ACP JSON-RPC via stdio.

The configured subprocess may be either:

- a native ACP agent, such as agents listed by the ACP registry as directly implementing ACP
- an ACP adapter executable, such as `codex-acp`, that speaks ACP to `acp-stack` and wraps an upstream agent that does not speak ACP directly

`acp-stack` does not implement agent-specific adapters itself. It installs and launches catalog-listed ACP agent or adapter executables, then treats the resulting process as the ACP protocol peer. The install source of truth is the embedded `data/agents.toml` (see [runtime](../runtime.md#agent-installation)). The current embedded catalog is intentionally narrow while the headless deployment pipeline is proven: OpenCode and Cursor CLI are verified native targets, and Amp plus Pi are verified adapter-backed targets through `amp-acp` and `pi-acp`. The registry model separately represents adapters and their upstream harnesses for future entries.

Upstream `agentclientprotocol/registry` is a reference used by the dev-only sync tool but is no longer fetched at runtime.

### Initialization

On agent start:

1. Spawn the configured agent command.
2. Set reserved runtime context (`PATH`, `HOME`) and inject only the secrets
   referenced by `[agent].env`.
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

Phase 2 supports:

- stdio MCP server declarations
- HTTP MCP server declarations
- secret interpolation for env vars and HTTP headers
- dependency status reporting for MCP commands

The initial MCP example focuses on Linear as an external service. Filesystem MCP is not a primary example because the agent already has workspace access through bash and the `acp-stack` Workspace API. The worked Linear example lives in [../mcp.md](../mcp.md).

## Permission Passthrough

ACP `session/request_permission` requests enter the same durable permission pipeline as stack-mediated commands. The bridge registers an `on_receive_request` handler that:

- builds a `NewPermission { source: Acp, requester: format!("session:{session_id}"), subject_id: Some(session_id), detail: serialized RequestPermissionRequest }` and submits it to the runtime's `PermissionService`,
- awaits the resulting `oneshot::Receiver<PermissionOutcome>` until an operator decides (or the per-request timer fires),
- translates the outcome back to an ACP `RequestPermissionOutcome`:
  - `Approved { option_id }` → `Selected { option_id }`. When the operator omits `option_id` on the approve body, the first option from the original request is used.
  - `Denied` / `Canceled` / `Expired` (with `timeout_action = "deny"`) → `Cancelled`.
  - `Expired` with `timeout_action = "approve"` is auto-approved with no option_id, which the bridge handles by selecting the first option from the original request.

Cancellation paths:

- If the originating session is closed or canceled while a request is still pending, the runtime cancels every pending ACP-source permission row whose `subject_id` matches the session id; the awaited outcome is `Canceled`, the bridge replies `Cancelled`, and the agent settles its prompt turn.
- If the daemon restarts with rows still pending, startup reconciliation marks every ACP-source pending row `canceled` (with a `system` / `daemon-restart` decision). The agent will have already abandoned its turn; the durable audit trail reflects that.

See [project-spec](../project-spec.md) and [api](../api/api.md#permissions-api) for the shared permission lifecycle and HTTP contract.
