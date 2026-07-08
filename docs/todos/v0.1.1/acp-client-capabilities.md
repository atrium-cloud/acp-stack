# ACP client capabilities

`acp-stack` currently sends `InitializeRequest::new(ProtocolVersion::V1)` with default (all-false) `ClientCapabilities` and registers only two agent→client handlers: `session/request_permission` and the `session/update` notification drain (`src/runtime/agent/acp_bridge.rs`). Agents therefore fall back to their built-in shell/fs tools, which bypass our permission review and command log. Zed (the reference client) advertises `fs.readTextFile`, `fs.writeTextFile`, `terminal`, and `session.configOptions.boolean`, and handles every stable agent→client method (`zed/crates/agent_servers/src/acp.rs`, `client_capabilities_for_agent`). This milestone brings `acp-stack` to the equivalent stable feature set.

Out of scope, deliberately: `auth.terminal` / `_meta.terminal-auth` (interactive login flows; excluded by the headless scope), `elicitation` (unstable, Zed gates it behind a beta flag), and `mcp/*` over ACP (unstable; Zed does not implement it either).

## P1 — advertise `session.configOptions`

We already consume session config options (model selection via `session/set_config_option`) but never advertise support, so a spec-strict agent that gates `configOptions` on client capabilities would hide its model selector from us and `acps agent set` would silently degrade.

- [x] Build the initialize request with `ClientCapabilities::new().session(ClientSessionCapabilities::new().config_options(SessionConfigOptionsCapabilities::new()))` in `AcpBridge::spawn`.
- [x] Decide whether to advertise `boolean: {}` (Zed does): not advertised — `set_session_config_option` only sends `SessionConfigValueId`, so advertising boolean would invite values we drop. Revisit when the set-config path handles `SessionConfigOptionValue::Boolean`.
- [x] Placebo agent: gate `configOptions` advertisement on the client capability so the bridge tests exercise the strict-agent path (`--require-client-config-options`).
- [x] Re-run the real-agent tests to confirm no agent regresses on the new initialize shape. 2026-07-07: OpenCode and Pi pass initialize + session/new + model advertisement with the full capability set (`--test-threads=1`; parallel runs starve the 15s initialize timeout). Amp (binary not installed) and Cursor (no `CURSOR_API_KEY`) not run locally.

## P2 — `terminal: true` and the `terminal/*` method family

Client-provided terminals route agent shell execution through us: command log, sandbox, observability. This is the strategic payoff — it moves agent shell activity into the mediation layer where `acp-stack`'s opinions live. Candidate substrate: the command gateway internals (`src/runtime/mediation/commands/`) — sandboxed spawn (`sandboxed_command`), output streaming and persistence, cancel/timeout handling. Whether to reuse `SupervisorTask` or build a leaner dedicated terminal task must be settled with evidence during planning: ACP's lifecycle (live output polling mid-run, kill-then-still-readable, explicit release) differs from the submit/policy flow the gateway was built around.

Methods to handle (all five are required once `terminal: true` is advertised):

- [x] `terminal/create` — spawn `command`+`args` with `env` and optional `cwd` (default to the session cwd), return a generated `TerminalId`. Record the command in the durable command log with an ACP-terminal origin marker (`commands.origin = 'acp'` + `session_id`, migration 023).
- [x] `terminal/output` — return buffered output, `truncated` flag, and `exitStatus` once exited. Honor `outputByteLimit` per spec: truncate from the beginning (retain the newest bytes) at a UTF-8 char boundary; the buffer is trimmed during accumulation so unpolled output stays bounded. Zed deviates (`truncated_output` keeps the head); we follow the spec.
- [x] `terminal/wait_for_exit` — await process exit, return `exit_code` and `signal`.
- [x] `terminal/kill` — kill the process but keep the terminal's output readable until released. Kill-intent exits (kill, release of a running terminal, shutdown drain) finalize the command row as `canceled`, matching the gateway's operator-cancel mapping; natural signal deaths stay `failed`.
- [x] `terminal/release` — kill if still running and drop all terminal state; subsequent method calls on the id are errors.
- [x] Terminal registry keyed by (session, terminal id) on the bridge; kill-and-release everything on agent shutdown/crash so no orphan processes survive a restart (`shutdown_kills_live_terminals`).
- [x] Permission mediation policy (decided): `terminal/create` executes directly like Zed — no permission-service gate — with the command recorded in the durable command log. The VM is the security boundary; agents send `session/request_permission` separately when their own policy requires it.
- [x] Environment baseline (decided): terminal children get a clean session environment plus the agent-provided `env` vars — never the provider API keys injected into the supervised agent's process.
- [x] Default output cap: when the agent omits `outputByteLimit`, a 1 MiB client-side default applies so a long-running command cannot buffer unbounded output. Agent-supplied limits are clamped to a 10 MiB ceiling (Carbon clamps the same way) so a huge requested limit cannot re-open unbounded buffering.
- [x] Sandbox interaction: terminal children inherit the same sandbox profile as the supervised agent via the shared `sandboxed_program` wrapper.
- [x] Advertise `terminal(true)` only after all five handlers pass placebo round-trips: create → output (with and without byte limit) → wait_for_exit → kill → release, plus release-of-unknown-id error shape (`terminal_full_lifecycle_under_advertised_capability`).

## P3 — `fs/read_text_file` and `fs/write_text_file`

Headless, both reduce to plain disk I/O against the workspace (no editor buffers to surface unsaved content from), so the payoff is audit visibility and `line`/`limit` reads. Cheap on top of the workspace module, but lowest priority.

- [x] `fs/read_text_file` — path must resolve inside the session workspace (`resolve_workspace_abs_path`); honor optional 1-based `line` and `limit`.
- [x] `fs/write_text_file` — same path constraint; atomic write-through to disk; records a durable `fs.write` event with source `acp`.
- [x] Advertise `fs.readTextFile`/`fs.writeTextFile` only when both handlers ship (`fs_round_trip_under_advertised_capability`).

## Verification

- [x] Placebo-agent coverage for every advertised capability (strict agent: only calls what the client advertises).
- [ ] Real-agent test: at least one harness observed using a client terminal end-to-end (command visible in `acps logs` / command history). Deterministic probe added: `real_opencode_terminal_uname_probe` / `real_pi_terminal_uname_probe` prompt "run `uname -a` and report the output" and assert an `acp`-origin `commands` row. 2026-07-07 finding: OpenCode and Pi complete the task via their built-in shell tools and never call `terminal/create`, even with `terminal: true` (and a Zed-style `_meta.terminal_output` hint) advertised — client-terminal adoption is agent-side and pending upstream. Our handler side is fully verified by the placebo lifecycle round-trips; re-run the probes as harnesses add support (Gemini CLI is a candidate).
- [x] Interim shell visibility for agents on built-in tools: execute-kind ACP tool-call blocks are lifted into derived `tool.execute` events (source `acp`, command extracted from `rawInput.command`), so agent shell activity is filterable in `acps logs` even before a harness adopts client terminals.
- [x] Spec docs updated: `docs/specs/acp/acp-bridge.md` capability table, `docs/specs/security.md` terminal mediation + fs containment, `docs/specs/api/api.md` command record fields, `docs/mgmt/architecture.md` ACP terminals subsystem.
