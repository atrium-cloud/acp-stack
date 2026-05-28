# Cursor CLI

Cursor CLI is a native ACP target. `acp-stack` launches `cursor-agent acp`.

## Setup

```sh
acps init --agent cursor
acps secrets set CURSOR_API_KEY
acps agent set --model <cursor-model-id>
```

Agent config shape:

```toml
[agent]
id = "cursor"
command = "cursor-agent"
args = ["acp"]
cwd = "/workspace"
env = ["CURSOR_API_KEY"]
restart = "on-crash"
```

Cursor model values are discovered from the agent over ACP. `acps agent set` stores the exact advertised value after validation.

Agent, Ask, and Plan modes are supported via:

```sh
acps agent set --mode <agent|ask|plan>
```

## Session Resume

`session/load`, `session/resume`, and `session/list` are discovered from the Cursor CLI `initialize` reply at runtime; `data/agents.toml` does not pin a value. End-to-end resume behavior against `acp-stack` is not currently confirmed.

If the live ACP connection to `cursor-agent` drops, `restart = "on-crash"` relaunches the supervised agent automatically. Any prompt that was mid-stream is flipped to `stalled` once the stale-prompt sweeper observes no further updates beyond `[prompts].stale_threshold`. Clients reconnect by calling `GET /v1/sessions/{id}/snapshot`, wait for the agent process to be running, then call `POST /v1/sessions/{id}/resume` when the new `cursor-agent` advertises `sessionCapabilities.resume`. If not, a fresh `POST /v1/sessions` is the practical recovery path.
