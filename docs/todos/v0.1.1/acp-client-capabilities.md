# ACP client capabilities

P1–P3 are complete: `acp-stack` advertises `session.configOptions`, `terminal: true`, and `fs.readTextFile`/`fs.writeTextFile`, with all five `terminal/*` handlers and both `fs/*` handlers shipped and placebo-verified. See `docs/specs/acp/acp-bridge.md` (capability table) and `docs/specs/security.md` (terminal mediation, fs containment).

## Open: real-agent terminal observation

- [ ] At least one harness observed using a client terminal end-to-end, with the command visible in `acps logs` / command history.

Deterministic probes exist in `tests/`: `real_opencode_terminal_uname_probe` and `real_pi_terminal_uname_probe`. Each prompts "run `uname -a` and report the output" and asserts an `acp`-origin `commands` row.

2026-07-07 finding:

- OpenCode and Pi complete the task via their built-in shell tools. Neither calls `terminal/create`, even with `terminal: true` advertised.
- A Zed-style `_meta.terminal_output` hint does not change this.
- Client-terminal adoption is agent-side and pending upstream harness support.
- Our handler side is fully verified by the placebo lifecycle round-trips.

Re-run the probes as harnesses add support. Gemini CLI is a candidate.
