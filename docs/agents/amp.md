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

If the live ACP connection to `amp-acp` drops, `restart = "on-crash"` relaunches the supervised agent automatically. Any prompt that was mid-stream is flipped to `stalled` once the stale-prompt sweeper observes no further updates beyond `[prompts].stale_threshold`. Clients reconnect through `GET /v1/sessions/{id}/snapshot`, wait for the agent process to be running, then call `POST /v1/sessions/{id}/resume` when the new adapter advertises `sessionCapabilities.resume`. If `session/resume` is unsupported, prompt resumption is not possible and a fresh session is the recovery path.
