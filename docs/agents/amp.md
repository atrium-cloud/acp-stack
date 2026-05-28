# Amp Code

Amp Code is adapter-backed. `acp-stack` launches `amp-acp`, which launches the Amp CLI through the adapter.

## Setup

```sh
acps init --agent amp
acps secrets set AMP_API_KEY
```

Agent config shape:

```toml
[agent]
id = "amp"
command = "amp-acp"
args = []
cwd = "/workspace"
env = ["AMP_API_KEY"]
restart = "on-crash"
```

Amp does not expose raw provider/model selection through the current `acp-stack` contract. Use mode selection instead:

```sh
acps agent set --mode <smart|rush|deep>
```

Amp Code default mode is Smart. Rush and Deep are also supported.

## Session Resume

`session/load`, `session/resume`, and `session/list` are discovered from the `amp-acp` adapter's `initialize` reply at runtime; `data/agents.toml` does not pin a value. End-to-end resume behavior against `acp-stack` is not currently confirmed.

If a live ACP connection to `amp-acp` drops, the agent enters a failed state and stays there until an admin restart (`acps agent restart` or the equivalent admin route — agent start/stop/restart are admin operations per `docs/specs/runtime.md`). Any prompt that was mid-stream is flipped to `stalled` once the stale-prompt sweeper observes no further updates beyond `[prompts].stale_threshold`. Clients reconnect through `GET /v1/sessions/{id}/snapshot` to see the failed state and any stalled prompts. After an admin relaunch, whether `POST /v1/sessions/{id}/resume` succeeds depends on what the new adapter advertises. If `session/resume` is unsupported, prompt resumption is not possible and a fresh session is the recovery path.
