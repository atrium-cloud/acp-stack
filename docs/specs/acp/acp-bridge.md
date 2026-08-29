# ACP Bridge

`acp-stack` is an ACP client. The configured agent is the ACP server process, launched over stdio unless an adapter provides that stdio surface.

## Initialization

When the agent starts, the bridge initializes ACP v1 with `clientInfo.name = "acp-stack"` and the running package version, then records the advertised capabilities. The agent must return protocol version 1; any other version closes the provisional connection before the agent becomes ready. Capability snapshots are exposed through the API and decide which session operations are available.

The advertisement is also captured session-free. The init capability probe spawns the agent, completes only the `initialize` handshake, persists the capability snapshot to state, and terminates the process.

Initialization failure prevents the agent from becoming ready and is reported in agent status.

For Kimi Code, the bridge derives Kimi's process-only model API key, selected model, and lane endpoint from the active lane's encrypted key ref. The derivation happens before launching `kimi acp`.

The lane endpoint is a subscription coding endpoint, a Moonshot platform endpoint, or a custom provider's base URL. A legacy config that omits `[agent.provider]` uses the mainland subscription endpoint. Canonical config keeps only the encrypted key ref; the derived values live in the launched process alone.

For Pi Agent, the bridge sets the process-only `PI_ACP_PI_BIN` to the managed `pi` path before launching `pi-acp`. The adapter bundle carries no Pi, so this names the harness it drives. Declaring that variable in `[agent].env` is rejected.

### Client capabilities

The initialize request advertises the client capabilities `acp-stack` implements. Each flag is advertised only when its agent-to-client handlers exist, so the wire contract claims only support the runtime serves.

| Capability              | Advertised | Notes                                                                                                   |
| ----------------------- | ---------- | ------------------------------------------------------------------------------------------------------- |
| `fs.readTextFile`       | yes        | Workspace-contained disk read with optional 1-based `line`/`limit`                                      |
| `fs.writeTextFile`      | yes        | Workspace-contained atomic write-through plus a durable `fs.write` audit event                          |
| `terminal`              | yes        | All five `terminal/*` methods, backed by the terminal registry below                                    |
| `session.configOptions` | yes        | Without the `boolean` sub-capability: `session/set_config_option` currently sends value-id options only |
| `auth.terminal`         | no         | Interactive login flows are excluded by the headless scope                                              |
| `elicitation`           | no         | Unstable upstream                                                                                       |
| `mcp/*` over ACP        | no         | Unstable upstream                                                                                       |

## Client terminals

`terminal/create` executes directly — there is no permission-service gate on terminal spawns. The VM is the security boundary; agents send `session/request_permission` separately when their own policy requires review. Every created terminal is recorded in the durable command log as an `acp`-origin `commands` row tied to the local session:

- Output streams into `command.stdout`/`command.stderr` events, which also fan out live on the `commands.{id}` WebSocket topic (same payload shape as gateway commands).
- The row is finalized with the exit status.
- Agent shell activity is therefore visible in `acps logs`, command history, and live subscriptions alongside operator-submitted commands.

### Terminal Ownership

- A single owning task per terminal holds the child process and pumps output chunks while selecting over natural exit and a kill channel.
- The per-bridge registry is keyed by (session id, terminal id) and holds only shared endpoints: output buffer, exit watch, kill sender.
- `terminal/output`, `terminal/wait_for_exit`, and `terminal/kill` never contend for the process; concurrent waiters all resolve from one exit publication.
- After the child exits, the owner drains the remaining pipe output before finalizing the command row and publishing the exit status. The drain is bounded by the same post-wait budget as the command gateway, so a detached descendant holding the pipes open cannot wedge the task.
- A `terminal/wait_for_exit` response therefore guarantees the output visible through `terminal/output` and the command log is complete.
- `terminal/kill` keeps output readable until `terminal/release`, which drops all terminal state; later calls on the id return resource-not-found.
- Kill-intent exits — `terminal/kill`, release of a running terminal, or the shutdown drain — finalize the command row as `cancelled` with no exit status, mirroring operator cancel in the command gateway. Natural signal deaths (OOM kill, segfault) finalize as `failed`.

### Output Limits

Output honors `outputByteLimit` in the spec's direction: truncation drops the oldest bytes and retains the newest, cut at a UTF-8 character boundary.

- The in-memory buffer is trimmed to the limit as chunks arrive, not at read time, so a chatty command the agent never polls cannot grow daemon memory.
- When the agent omits `outputByteLimit`, a 1 MiB default cap applies.
- Agent-supplied limits are clamped to a 10 MiB ceiling, so a huge requested limit cannot re-open unbounded buffering.
- The full untrimmed stream still flows to the durable command log.

### Working Directory, Environment, And Shutdown

- A `terminal/create` that omits `cwd` defaults to the session's recorded cwd, falling back to the workspace root when no session state is attached.
- Every cwd — defaulted or explicit — must resolve inside the workspace.
- Terminal children run under the same sandbox profile as the supervised agent.
- They receive a clean session environment: managed `PATH` and `HOME` plus the env vars from `terminal/create` — never the `[agent].env` provider secrets injected into the agent process.
- Bridge shutdown (including the crash-monitor path) kills and releases every live terminal and closes the registry.
- A `terminal/create` racing shutdown is refused and its child killed, so nothing escapes the teardown.
- Terminal children have their own process groups, so the agent-process-group kill alone would orphan them.

## Client filesystem

`fs/read_text_file` and `fs/write_text_file` operate on paths confined to the session workspace:

- Absolute paths from the agent must resolve inside `[workspace].root` through the same canonicalization and symlink refusal as the workspace API.
- Reads honor the optional 1-based `line` offset and `limit` line count and are capped at 10 MiB.
- Writes are atomic write-throughs and record a durable `fs.write` event with source `acp`.
- Headless, there are no editor buffers — disk is the truth on both methods.

## Sessions

The bridge maps runtime session operations to ACP methods where supported:

- create
- list
- load
- resume
- fork
- close
- delete
- prompt
- cancel
- set model or mode config options

If an agent's advertisement omits an optional capability, the corresponding runtime operation fails with `StackError::AgentUnsupportedCapability` (HTTP 501, `error_code = "agent.unsupported_capability"`). The bridge gates each optional ACP session method by checking the capability snapshot before dispatching:

- `session/list` requires `supports_list_sessions`
- `session/load` requires `supports_load_session`
- `session/resume` requires `supports_resume_session`
- `session/fork` requires `supports_fork_session`
- `session/delete` requires `supports_delete_session`

Capability flags are read from the ACP `initialize` response: `loadSession` on the top-level capabilities object, and `sessionCapabilities.{list,resume,fork,close,delete}` for the rest. Further gates:

- Image, audio, and embedded-resource prompt blocks require the matching `promptCapabilities` flag.
- HTTP and SSE MCP declarations require `mcpCapabilities.http` and `mcpCapabilities.sse`; stdio has no dedicated flag and requires at least one advertised MCP capability.
- A declaration the advertisement does not cover is dropped from the session rather than failing it (see [MCP Servers](#mcp-servers)); an MCP transport variant the runtime does not model is still a hard failure.
- Forking at a prompt breakpoint also requires explicit `_meta.acpStack.messageId` support under `sessionCapabilities.fork`; otherwise only current-head fork is allowed.
- Unsupported combinations fail locally before a request is dispatched.

The bridge code lives in `src/runtime/agent/acp_bridge.rs`.

### Prompt Message IDs (local extension)

ACP v1 assigns message ids on agent-emitted update chunks but has no client-proposed prompt message id. Prompt breakpoints for `session/fork` therefore remain a local extension, tracked in `docs/todos/v0.1.0/phase_5.md` until upstream exposes an equivalent `session/fork` shape. The wire shape rides ACP's `_meta` extensibility point:

- `session/prompt` requests carry `_meta.acpStack.messageId` with a runtime-generated id.
- An agent that recorded the id acknowledges it by echoing the same `_meta.acpStack.messageId` shape on the `session/prompt` response. Only acknowledged ids are accepted as fork breakpoints.
- `session/fork` requests carry the breakpoint as `_meta.acpStack.messageId`.

Before ACP 1.0 this extension used the SDK's unstable top-level `messageId`/`userMessageId` prompt fields. Agents still speaking that pre-1.0 shape receive no acknowledgment, so only current-head fork remains available to them.

Sessions learned from `session/list` are persisted only when their CWD is an existing directory under `[workspace].root`. Load, resume, and fork recheck the stored CWD before passing it back to the agent. Explicit load/resume CWDs update local session state after the agent accepts the call.

ACP session lifecycle calls pass CWDs as paths because ACP has no directory-handle transport; the runtime revalidates those paths immediately before each call.

`session/close` is surfaced as history-preserving close in `acp-stack`; it keeps local session history intact. `session/delete` (`POST /v1/sessions/{id}/delete`) is the destructive counterpart:

- The agent removes the session from its own history.
- The runtime then hard-deletes the local session row together with its prompts and per-session events.
- Permission log rows stay in the durable security log, and the external log mirror is upsert-only, so mirrored rows are not retracted.
- Repeat deletes and unknown ids succeed silently, matching ACP's idempotency requirement.

### Session Resume Capability Matrix

`data/agents.toml` declares no per-agent overrides for `session/list`, `session/load`, `session/resume`, or `session/fork`, so every supported agent reports the same status: discovered at runtime from the agent's `initialize` reply.

"Discovered" means the runtime trusts the value advertised by the agent's `initialize` response. When an agent reports `false` (or omits the flag), the matching `POST /v1/sessions/{id}/{load,resume,fork}` route returns HTTP 501 `agent.unsupported_capability`, and the operator-facing alternative is to create a fresh session. The per-agent live behavior of these capabilities is captured in `docs/agents/{agent}.md`.

## Streaming

ACP `session/update` notifications are persisted as durable events and published to WebSocket subscribers. Explicit `type: "diff"` tool-call content is also reduced into the bounded process-local snapshot returned by `GET /v1/sessions/{id}/changes`; no diff is inferred from tool kind, locations, filesystem calls, or Git. Prompt submission returns quickly with a prompt id; clients can follow live updates or poll durable prompt state.

Two derived events are lifted out of the verbatim `session.update` stream when the payload shape is recognized:

- `usage.reported`: standard ACP context-window/cost snapshots plus recognized legacy token usage.
- `tool.execute`: a `tool_call`/`tool_call_update` block whose kind is `execute` — the shell runs an agent performs through its own built-in tools rather than client terminals. The command line is extracted from `rawInput.command` when present.

Other projections off the same stream:

- Standard `session_info_update` notifications patch the local session title and preserve agent timestamps and metadata in the session record.
- `available_commands_update` notifications replace the session's stored slash-command list (latest-wins, including an empty list) as a compact bounded projection of `name`, `description`, and the unstructured input hint. Per-command `_meta` is dropped, no derived event is emitted, and the verbatim `session.update` row remains the source of truth.
- The stored list reflects the last advertisement only — it may be stale until the agent re-advertises. Agents also accept commands they never advertised, so the runtime never validates prompt text against it; `POST /v1/sessions/{id}/commands` reports an advisory `advertised` flag instead.
- `tool.execute` fires on every update that states the execute kind. ACP only requires `kind` on the initial `tool_call`, so completion transitions typically remain visible only in the verbatim rows.

## Permissions

ACP permission requests flow into the same permission system used by mediated commands. The durable wait runs outside the ACP dispatch loop, so other updates and requests continue while a decision is pending. Protocol cancellation atomically cancels a still-pending permission and returns the standard request-cancelled error; an operator or timeout decision that wins the race is returned normally.

## MCP Servers

Configured MCP servers are attached to ACP sessions when the agent and SDK support session MCP configuration. Secret refs for MCP env vars and headers are resolved at attach time; the resolved values stay out of logs and API responses.

Servers whose transport the running agent's advertisement omits are skipped for that session — create, load, resume, and fork proceed with the remaining servers. Each skip is recorded as a session-scoped `mcp.session_skipped` event at level `warn`. The event carries the server name and the capability the agent would have had to advertise.

Session create applies the same routing in a fixed order: `agent.mode`, then the configured model, then `agent.effort`, and finally every `[agent.config_options]` entry. `agent.effort` maps to the agent's `thought_level` config option and applies after the model, since adapters advertise effort levels per model. `agent.mode` resolves against `config_options` first; an agent that advertises modes only in the `session/new` `modes` field has the value applied through `session/set_mode` instead. That native set applies once at create and is not carried in the config-option snapshot below.

Boolean-kind options get native boolean payloads; selects get value ids. When the agent's `session/new` config options lack the configured value, the set is skipped. The session proceeds on the agent's default, and a `session.capability_ignored` event (level `warn`) records the omission. A failure from setting an option the agent did advertise remains a hard error.

The bridge advertises the `session.configOptions.boolean` client capability at initialize, so agents may shape options as native boolean toggles instead of two-value select fallbacks. The typed mode/model/effort lanes match select-kind options only, so a boolean option carrying a typed category leaves its select twin untouched.

The runtime keeps a per-session config-option snapshot, owned by `src/runtime/agent/config_options.rs`:

- Seeded from `session/new` at create.
- Replaced from each `session/set_config_option` response; an empty response list is ignored, since lax adapters carry the refresh only in notifications.
- Replaced from `config_option_update` notifications, which the session sink projects into `sessions.metadata_json` beside the available-commands snapshot.

The snapshot backs `GET/POST /v1/sessions/{id}/config-options`; the verbatim `session.update` events remain the source of truth.
