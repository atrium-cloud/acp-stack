# Cline

Cline is a native ACP target. `acp-stack` launches `cline --acp`.

## Setup

```sh
acps secrets set CLINE_API_KEY
acps init --agent cline
acps agent set --model <model-id>
```

Agent config shape:

```toml
[agent]
id = "cline"
command = "cline"
args = ["--acp"]
cwd = "/workspace"
env = ["CLINE_API_KEY"]
restart = "on-crash"
```

`CLINE_API_KEY` stays in the encrypted secret store and reaches the process only through `[agent].env`; Cline's ACP mode reads it directly from the environment, so no OAuth login is required. With the key set, the provider defaults to `cline`. `CLINE_PROVIDER` and `CLINE_MODEL` are honored from the environment as well, but when `CLINE_PROVIDER` is set the provider is locked and ACP `session/set_config_option` provider changes throw — so `acp-stack` leaves provider selection to the environment (`set_provider = false`). The `-k`/`-P` CLI flags are ignored in ACP mode, and credentials persisted by `cline auth` for non-Cline providers are not read by ACP. Do not add `CLINE_PROVIDER`/`CLINE_MODEL` refs to `[agent].env` unless you intend that lock.

Model selection goes through ACP: `acps` applies the configured model via `session/set_config_option` on each new session, and mode values (`plan`, `act`) are discovered over ACP and can be selected with `acps agent set --mode <mode>`.

Cline sends per-tool `session/request_permission` calls, which `acp-stack` answers through the standard permission pipeline; requests that need a decision surface to the daemon operator approval flow.

Managed Agent Skills are installed into `~/.agents/skills`, which Cline's SDK skill search paths discover natively.

## Session Resume

`session/load`, `session/resume`, and `session/list` are discovered from Cline's `initialize` reply at runtime; `data/agents.toml` does not pin a value.

If the live ACP connection to Cline drops, `restart = "on-crash"` relaunches the supervised agent automatically. Any prompt that was mid-stream is flipped to `stalled` once the stale-prompt sweeper observes no further updates beyond `[prompts].stale_threshold`. Clients reconnect by calling `GET /v1/sessions/{id}/snapshot`, wait for the agent process to be running, then call `POST /v1/sessions/{id}/resume` when the new Cline advertises `sessionCapabilities.resume`. If it does not, a fresh `POST /v1/sessions` is the recovery path.
