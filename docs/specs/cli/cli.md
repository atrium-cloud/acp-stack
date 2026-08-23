# CLI

`acps` is the CLI for initializing, running, inspecting, and operating an `acp-stack` instance.

## Command Groups

| Area           | Commands                                                                                                                                |
| -------------- | --------------------------------------------------------------------------------------------------------------------------------------- |
| Instance       | `acps init`, `acps init serve`, `acps serve`, `acps status`, `acps restart`, `acps update`, `acps reset --yes`                          |
| Auth           | `acps auth regenerate-session-key`                                                                                                      |
| Config         | `acps config validate`, `export`, `import`                                                                                              |
| Secrets        | `acps secrets list`, `set`, `delete`                                                                                                    |
| Agents         | `acps agent install`, `update`, `switch`, `config inspect/import`, `start`, `stop`, `restart`, `status`, `check`, `test`, `default set` |
| Provider/model | `acps agent provider`, `acps agent set`, `acps subagent status/set/match/free/disable`                                                  |
| Array          | `acps array status/on/off/add/set/install/start/stop/restart`                                                                           |
| Workspace      | `acps workspace status`, `code-source`, `data-source`, `sync`, `sandbox`                                                                |
| Skills         | `acps skills list/catalog/add/remove`, `acps skills source get/add/remove`                                                              |
| Sessions       | `acps sessions list/status/new/load/resume/fork/prompt/cancel/close`                                                                    |
| Logs/metrics   | `acps logs query`, `logs tail`, `metrics summary`                                                                                       |
| Operations     | `acps deps check`, `deps apply`, `security check`, `security history`, `security show`, `installer history`                             |
| WebSockets     | `acps ws connections`, `ws sessions`, `ws disconnect`                                                                                   |
| Shell          | `acps completion <shell>`                                                                                                               |

Commands read `~/.config/acp-stack/acps-config.toml` by default unless an explicit path argument is documented.

## Output Formats

- Most operator commands accept global `--format text|json`; text is the default.
- Commands that remain text-only reject `--format json` instead of silently ignoring it.
- Existing `--json` flags remain accepted as aliases for `--format json` and conflict with explicit `--format text`.
- `acps logs query --follow --format json` emits newline-delimited event objects.

## Initialization

`acps init` creates or validates config and state, initializes the encrypted secret store, and generates API keys on first run. It can optionally configure an agent, provider, workspace sources, MCP servers, edge profile, and testflight. The end-to-end flow and step sequence are in [init.md](../init.md); the flags are in the [flag reference](cli-flags.md#acps-init).

## Auth Tiering

First initialization prints two API keys:

- Session key: session-driving and prompt-driving API calls.
- Admin key: secrets, config import, agent process control, and other elevated operations.

Local state stores only verifiers; the plaintext values stay out of config and `secrets.age`.

- Commands that need the session key accept `--session-key` or `ACP_STACK_SESSION_KEY`.
- When `[local].session_auth = "keyless"`, session-tier HTTP commands without an explicit session key use the local Unix socket instead.
- Commands that need the admin key accept `--admin-key`; interactive terminals prompt without echo when it is omitted.

Key rotation, local-session-access, and reset commands are in the [flag reference](cli-flags.md).

## Restart Levels

### Agent Restart

A supervised-agent restart is sufficient for agent process settings:

- Provider, model, mode, and agent environment refs.
- Command/args.
- The workspace sandbox used by the agent harness.

### Daemon Restart

A daemon restart is required for daemon startup-cached settings:

- Workspace root and uploads paths, default shell, and max file bytes.
- MCP declarations.
- Mediated command policy/env/timeouts/sandbox.
- HTTP/CORS/rate limits/body caps.
- Permission service defaults.
- Logging sink wiring.

## Array

`acps array *` manages multi-target Array mode. The model, commands, validation, and API are in [array.md](../array.md).

## Skills

`acps skills` manages the active agent's Agent Skills after init. Command semantics are in the [flag reference](cli-flags.md#acps-skills); catalog and managed-marker rules are in [skills.md](../agents/skills.md).

## `acps agent test` JSON Contract

`acps agent test --format json` prints one stable document to stdout, on success and on failure alike. A failed run additionally prints the human error to stderr and exits 1. Every key is always present:

```json
{
  "schema_version": 1,
  "ok": true,
  "phase": "done",
  "code": "ok",
  "elapsed_ms": 12345,
  "agent": "opencode",
  "prompt_source": "registry",
  "stop_reason": "end_turn",
  "updates": 7,
  "fs_check": { "status": "ok", "bytes": 128 },
  "cleanup": { "session_delete": "deleted", "process": "terminated" }
}
```

- `phase` is one of `spawn`, `initialize`, `session_new`, `session_config`, `prompt`, `fs_check`, `cleanup`, `done`. It is derived from `code`, so the two can never disagree.
- `code` is one of `ok`, `agent_spawn_failed`, `agent_initialize_failed`, `session_create_failed`, `session_config_failed`, `prompt_failed`, `prompt_timeout`, `progress_timeout`, `unexpected_stop_reason`, `fs_check_missing`, `fs_check_empty`, `fs_check_not_regular_file`, `fs_check_outside_workspace`, `fs_check_failed`, `cleanup_failed`, `config_invalid`, `agent_unsupported`.
- `prompt_source` is `provided`, `registry`, or `default`. `stop_reason` is `null` when the prompt phase was never reached.
- `fs_check.status` is `ok`, `skipped`, or `failed`. `skipped` covers both a registry entry that declares no `testflight_expect_fs` and a run that failed before the check. `bytes` is `null` unless the status is `ok`.
- `cleanup.session_delete` is `deleted`, `cleanup_failed`, `unsupported` (the agent does not advertise `session/delete`), or `skipped` (no session was ever created).
- The delete is bounded at 10 seconds. A run that failed on a progress timeout leaves the agent wedged mid-prompt on its single event loop, where the request would never be answered. That case is reported as `cleanup_failed` rather than awaited.
- `cleanup.process` is `terminated` or `terminate_failed`. `terminated` includes a spawn that failed before a child existed, since nothing is left running either way.
- A failed session delete leaves `ok` unchanged. The verdict is prompt completion plus the fs check; a working agent with a flaky delete is not a failed test. A leaked agent child does flip it, reported as `phase: "cleanup"`, `code: "cleanup_failed"`.
- `elapsed_ms` is measured against the wall clock, so a host suspend mid-run is reflected rather than lost.
- The document deliberately carries no reason string, session id, prompt text, file contents, path, credential, or raw provider error. Reasons embed workspace paths and spawn argv; codes are the machine channel.
- A failure that happens before the harness can run at all — an unreadable config, an unresolvable home directory — emits no document, only the stderr error and exit 1.

## Flag Reference

Every command synopsis and per-flag semantic: [Flag reference](cli-flags.md).
