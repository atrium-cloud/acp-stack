# Array

Array mode runs more than one configured agent target under a single `acp-stack` runtime. Each target is an independent ACP agent process (a distinct harness) with its own `[agent]` block, supervisor, and sessions. One target is the `primary_target`: it is the default for the un-suffixed `acps agent *` and `/v1/agent/*` surfaces and is the coordination point for the fleet. Array is disabled by default; a fresh or legacy single-agent config is migrated into a one-target, Array-off config that behaves exactly as before.

## Config Shape

```toml
[array]
enabled = false
primary_target = "opencode"

[[array.targets]]
id = "opencode"

[array.targets.agent]
id = "opencode"
name = "OpenCode"
command = "opencode"
args = ["acp"]
cwd = "/workspace"
env = ["<provider-api-key-ref>"]
restart = "on-crash"

[[array.targets]]
id = "codex"

[array.targets.agent]
id = "codex"
name = "Codex"
command = "codex"
args = ["acp"]
restart = "on-crash"
```

`[array]` is the serialized source of truth. The legacy top-level `[agent]` block is not written to canonical config; it is read on load and migrated into a single Array target, and the in-memory `agent` is rebuilt from `array.primary_target`. Each `[[array.targets]]` entry wraps one full `[agent]` block under `[array.targets.agent]`, with the same fields documented in [config.md](config.md#agent) and [agents/config.md](agents/config.md).

| Field            | Meaning                                                        |
| ---------------- | -------------------------------------------------------------- |
| `enabled`        | whether secondary targets may run; `false` keeps only the primary active |
| `primary_target` | id of the target that backs the default `agent`/`/v1/agent/*` surfaces |
| `targets[].id`   | target id; must equal its `agent.id`                           |
| `targets[].agent`| the target's ACP agent process and injected secret refs        |

## Validation

`acps config validate` (and every load) enforces:

- `array.targets` is non-empty.
- Each `targets[].id` starts with an ASCII letter or digit and otherwise contains only ASCII letters, digits, `-`, `_`, or `.`, with no surrounding whitespace.
- Each `targets[].id` equals its `targets[].agent.id`.
- Target ids are unique, and harnesses are unique — Array v1 requires a different `agent.id` per target.
- `primary_target` references an existing target.
- Each target's agent block passes the same agent validation as a single-agent config; failures name the offending target.

Secret refs in a non-primary target's `env` may intentionally be shared across targets (each target is a separate process with its own environment), so cross-target reuse is allowed; duplicate refs within a single target are still rejected.

## CLI

| Command | Tier | Effect |
| ------- | ---- | ------ |
| `acps array status` | session | Array config plus, when the daemon is reachable, per-target process state and pid, and local-delegation readiness |
| `acps array on` / `off` | local | flip `array.enabled`; `off` keeps configured targets |
| `acps array add <agent>` | local | add a target for a registry agent; rejects an already-configured harness |
| `acps array set --target <id> ...` | local | configure provider, model, mode, or custom provider for one target (same flags as `acps agent set`) |
| `acps array install\|start\|stop\|restart [--target <id>] [--admin-key <key>]` | admin | drive one target, or every configured target when `--target` is omitted |
| `acps agent default set <target>` | local | repoint `primary_target` at an existing target |

`acps array status` reads the local read-only socket and does not require a session key. The four daemon actions call the running daemon with the admin key (required when stdin is not a terminal). With Array off, `start` and `restart` are restricted to the primary target; `install` and `stop` are unrestricted (install is idempotent, and stop on a non-running target is a no-op). When `--target` is omitted, the command attempts every target, prints a per-target result line, and exits non-zero if any target failed — a single failing target never aborts the rest of the batch.

## API

| Route | Tier | Contract |
| ----- | ---- | -------- |
| `GET /v1/array/status` | session | enabled flag, primary target, local-delegation readiness, and per-target id/agent/name/state/pid |
| `GET /v1/array/targets/{target_id}/capabilities` | session | latest ACP capability snapshot for one target |
| `POST /v1/array/targets/{target_id}/install` | admin | install one target's harness |
| `POST /v1/array/targets/{target_id}/start` | admin | start one target's process |
| `POST /v1/array/targets/{target_id}/stop` | admin | stop one target's process |
| `POST /v1/array/targets/{target_id}/restart` | admin | restart one target's process |

The un-suffixed `/v1/agent/*` routes operate on `primary_target`. Session routes accept `?target=<id>` (alias `target`) to address a specific target. An unknown `target_id` returns `400 request.invalid_param`. Start/restart of a non-primary target while Array is off is rejected with `400` and an "Array mode is off" message; tiering matches the single-agent surface (read-only status/capabilities are session-tier, process control is admin-tier).

## Sessions And State

Each session row records the `target_id` that owns it and the `agent_session_id` the agent assigned, in addition to the local session `id`. A `UNIQUE(target_id, agent_session_id)` index makes the agent's session id the stable per-target identity; the same agent session id may recur under different targets. See [state-logging.md](state-logging.md#sessions-columns).

Driving ops (`prompt`, `load`, `resume`, `fork`) against a non-primary target require Array to be enabled. Terminal wind-down ops (`close`, `cancel`) always reach a session's stored target, so toggling Array off never strands a session that was opened against a secondary target — an operator can always close or cancel it.

## Migration And Defaults

`acps init` writes a disabled, one-target `[array]` block mirroring the chosen agent; it does not prompt for Array setup. A legacy config carrying a top-level `[agent]` block loads as a one-target Array with `enabled = false` and `primary_target` set to that agent. Enabling Array (`acps array on`) and adding targets (`acps array add`) are explicit operator actions.

Health reporting excludes the live pids of every supervised target from orphan detection, so a running secondary target is not misreported as a leaked process.
