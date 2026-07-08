# ACP Bridge

`acp-stack` is an ACP client. The configured agent is the ACP server process, launched over stdio unless an adapter provides that stdio surface.

## Initialization

When the agent starts, the bridge initializes ACP and records the advertised capabilities. Capability snapshots are exposed through the API and used to decide which session operations are available.

Initialization failure prevents the agent from becoming ready and is reported in agent status.

### Client capabilities

The initialize request advertises the client capabilities `acp-stack` implements. Each flag is advertised only when its agent-to-client handlers exist, so the wire contract never claims support the runtime cannot serve.

| Capability | Advertised | Notes |
| --- | --- | --- |
| `fs.readTextFile` | yes | Workspace-contained disk read with optional 1-based `line`/`limit` |
| `fs.writeTextFile` | yes | Workspace-contained atomic write-through plus a durable `fs.write` audit event |
| `terminal` | yes | All five `terminal/*` methods, backed by the terminal registry below |
| `session.configOptions` | yes | Without the `boolean` sub-capability: `session/set_config_option` currently sends value-id options only |
| `auth.terminal` | no | Interactive login flows are excluded by the headless scope |
| `elicitation` | no | Unstable upstream |
| `mcp/*` over ACP | no | Unstable upstream |

## Client terminals

`terminal/create` executes directly — there is no permission-service gate on terminal spawns. The VM is the security boundary; agents send `session/request_permission` separately when their own policy requires review. Every created terminal is recorded in the durable command log as an `acp`-origin `commands` row tied to the local session, its output streams into `command.stdout`/`command.stderr` events that also fan out live on the `commands.{id}` WebSocket topic (same payload shape as gateway commands), and the row is finalized with the exit status, so agent shell activity is visible in `acps logs`, command history, and live subscriptions alongside operator-submitted commands.

Each terminal is driven by a single owning task that holds the child process and pumps output chunks while selecting over natural exit and a kill channel; the per-bridge registry keyed by (session id, terminal id) holds only shared endpoints (output buffer, exit watch, kill sender). `terminal/output`, `terminal/wait_for_exit`, and `terminal/kill` therefore never contend for the process, and concurrent waiters all resolve from one exit publication. After the child exits, the owner drains the remaining pipe output (bounded by the same post-wait budget as the command gateway, so a detached descendant holding the pipes open cannot wedge the task) before finalizing the command row and publishing the exit status — a `terminal/wait_for_exit` response therefore guarantees the output visible through `terminal/output` and the command log is complete. `terminal/kill` keeps output readable until `terminal/release`, which drops all terminal state; later calls on the id return resource-not-found. Kill-intent exits — `terminal/kill`, release of a running terminal, or the shutdown drain — finalize the command row as `canceled` with no exit status, mirroring operator cancel in the command gateway; natural signal deaths (OOM kill, segfault) finalize as `failed`.

Output honors `outputByteLimit` in the spec's direction: truncation drops the oldest bytes and retains the newest, cut at a UTF-8 character boundary. The in-memory buffer is trimmed to the limit as chunks arrive — not at read time — so a chatty command the agent never polls cannot grow daemon memory; when the agent omits `outputByteLimit`, a 1 MiB default cap applies, and agent-supplied limits are clamped to a 10 MiB ceiling so a huge requested limit cannot re-open unbounded buffering. The full untrimmed stream still flows to the durable command log.

A `terminal/create` that omits `cwd` defaults to the session's recorded cwd (falling back to the workspace root when no session state is attached), and every cwd — defaulted or explicit — must resolve inside the workspace. Terminal children run under the same sandbox profile as the supervised agent and receive a clean session environment: managed `PATH` and `HOME` plus the env vars from `terminal/create` — never the `[agent].env` provider secrets injected into the agent process. Bridge shutdown (including the crash-monitor path) kills and releases every live terminal and closes the registry: a `terminal/create` racing shutdown is refused and its child killed, so nothing escapes the teardown. Terminal children have their own process groups, so the agent-process-group kill alone would orphan them.

## Client filesystem

`fs/read_text_file` and `fs/write_text_file` operate on paths confined to the session workspace: absolute paths from the agent must resolve inside `[workspace].root` through the same canonicalization and symlink refusal as the workspace API. Reads honor the optional 1-based `line` offset and `limit` line count and are capped at 10 MiB. Writes are atomic write-throughs and record a durable `fs.write` event with source `acp`. Headless, there are no editor buffers — disk is the truth on both methods.

## Sessions

The bridge maps runtime session operations to ACP methods where supported:

- create
- list
- load
- resume
- fork
- close
- prompt
- cancel
- set model or mode config options

If an agent does not advertise an optional capability, the corresponding runtime operation fails with `StackError::AgentUnsupportedCapability` (HTTP 501, `error_code = "agent.unsupported_capability"`). The bridge gates each optional ACP session method by checking the capability snapshot before dispatching:

- `session/list` requires `supports_list_sessions`
- `session/load` requires `supports_load_session`
- `session/resume` requires `supports_resume_session`
- `session/fork` requires `supports_fork_session`

Capability flags are read from the ACP `initialize` response — `loadSession` on the top-level capabilities object, and `sessionCapabilities.{list,resume,fork,close}` for the rest. Forking at a prompt breakpoint also requires explicit `sessionCapabilities.fork.messageId` support, advertised either as a `messageId` object on the fork capability or under its `_meta` (`_meta.acpStack.messageId` or `_meta.messageId`); otherwise only current-head fork is allowed. The bridge code lives in `src/runtime/agent/acp_bridge.rs`.

### Prompt Message IDs (local extension)

ACP v1 assigns message ids on agent-emitted update chunks but has no client-proposed prompt message id, so prompt breakpoints for `session/fork` remain a local extension (tracked in `docs/todos/v0.1.0/phase_5.md` until upstream exposes an equivalent `session/fork` shape). The wire shape rides ACP's `_meta` extensibility point:

- `session/prompt` requests carry `_meta.acpStack.messageId` with a runtime-generated id.
- An agent that recorded the id acknowledges it by echoing the same `_meta.acpStack.messageId` shape on the `session/prompt` response. Only acknowledged ids are accepted as fork breakpoints.
- `session/fork` requests carry the breakpoint as a top-level `messageId` param.

Before ACP 1.0 this extension used the SDK's unstable top-level `messageId`/`userMessageId` prompt fields; agents still speaking that pre-1.0 shape are not acknowledged and therefore cannot be forked at a breakpoint.

Sessions learned from `session/list` are persisted only when their CWD is an existing directory under `[workspace].root`. Load, resume, and fork recheck the stored CWD before passing it back to the agent. Explicit load/resume CWDs update local session state after the agent accepts the call.

ACP session lifecycle calls pass CWDs as paths because ACP has no directory-handle transport; the runtime revalidates those paths immediately before each call.

`session/close` is surfaced as history-preserving close in `acp-stack`; it does not permanently delete local session history.

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

Two derived events are lifted out of the verbatim `session.update` stream when the payload shape is recognized: `usage.reported` (normalized token/context usage) and `tool.execute` (a `tool_call`/`tool_call_update` block whose kind is `execute` — the shell runs an agent performs through its own built-in tools rather than client terminals, with the command line extracted from `rawInput.command` when present). `tool.execute` fires on every update that states the execute kind; ACP only requires `kind` on the initial `tool_call`, so completion transitions typically remain visible only in the verbatim rows.

## Permissions

ACP permission requests flow into the same permission system used by mediated commands. Decisions are recorded and returned to the agent through ACP.

## MCP Servers

Configured MCP servers are attached to ACP sessions when the agent and SDK support session MCP configuration. Secret refs for MCP env vars and headers are resolved at attach time and are not written to logs or API responses.
