# ACP Bridge

`acp-stack` is an ACP client. The configured agent is the ACP server process, launched over stdio unless an adapter provides that stdio surface.

## Initialization

When the agent starts, the bridge initializes ACP and records the advertised capabilities. Capability snapshots are exposed through the API and used to decide which session operations are available.

Initialization failure prevents the agent from becoming ready and is reported in agent status.

## Sessions

The bridge maps runtime session operations to ACP methods where supported:

- create
- list
- load
- resume
- fork
- close/delete
- prompt
- cancel
- set model or mode config options

If an agent does not advertise an optional capability, the corresponding runtime operation fails with `StackError::AgentUnsupportedCapability` (HTTP 501, `error_code = "agent.unsupported_capability"`). The bridge gates each optional ACP session method by checking the capability snapshot before dispatching:

- `session/list` requires `supports_list_sessions`
- `session/load` requires `supports_load_session`
- `session/resume` requires `supports_resume_session`
- `session/fork` requires `supports_fork_session`

Capability flags are read from the ACP `initialize` response — `loadSession` on the top-level capabilities object, and `sessionCapabilities.{list,resume,fork,close}` for the rest. Forking at a prompt breakpoint also requires explicit `sessionCapabilities.fork.messageId` support; otherwise only current-head fork is allowed. The bridge code lives in `src/runtime/agent/acp_bridge.rs`.

Sessions learned from `session/list` are persisted only when their CWD is an existing directory under `[workspace].root`. Load, resume, and fork recheck the stored CWD before passing it back to the agent.

### Session Resume Capability Matrix

`data/agents.toml` does not declare per-agent overrides for these capabilities; every value below is discovered at runtime from the agent's `initialize` reply. A value listed as "untested" has not been confirmed end-to-end against the agent in question.

| Agent      | `session/list` | `session/load` | `session/resume` | `session/fork` |
| ---------- | -------------- | -------------- | ---------------- | -------------- |
| OpenCode   | discovered     | discovered     | discovered       | discovered     |
| Cursor CLI | discovered     | discovered     | discovered       | discovered     |
| Amp Code   | discovered     | discovered     | discovered       | discovered     |
| Pi Agent   | discovered     | discovered     | discovered       | discovered     |
| Goose      | discovered     | discovered     | discovered       | discovered     |
| Codex      | discovered     | discovered     | discovered       | discovered     |

"Discovered" means the runtime trusts the value advertised by the agent's `initialize` response. When an agent reports `false` (or omits the flag), the matching `POST /v1/sessions/{id}/{load,resume,fork}` route returns HTTP 501 `agent.unsupported_capability` and the operator-facing alternative is to create a fresh session. The per-agent live behavior of these capabilities is captured in `docs/agents/{agent}.md`.

## Streaming

ACP `session/update` notifications are persisted as durable events and published to WebSocket subscribers. Prompt submission returns quickly with a prompt id; clients can follow live updates or poll durable prompt state.

## Permissions

ACP permission requests flow into the same permission system used by mediated commands. Decisions are recorded and returned to the agent through ACP.

## MCP Servers

Configured MCP servers are attached to ACP sessions when the agent and SDK support session MCP configuration. Secret refs for MCP env vars and headers are resolved at attach time and are not written to logs or API responses.
