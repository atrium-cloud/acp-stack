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

Amp does not expose provider selection. Model selection uses Amp's execution tiers, advertised at runtime as the `amp-mode` Model-category config option (`amp-acp` v0.8.0 or later; older adapters advertise no Model-category option):

```sh
acps agent set --model <low|medium|high|ultra>
```

The adapter default is `medium`. Mode selection controls permission behavior, advertised as the `permission` Mode-category option:

```sh
acps agent set --mode <default|bypass>
```

## Session Resume

`session/load`, `session/resume`, and `session/list` are discovered from the `amp-acp` adapter's `initialize` reply at runtime; `data/agents.toml` does not pin a value.

If the live ACP connection to `amp-acp` drops, `restart = "on-crash"` relaunches the supervised agent automatically. Any prompt that was mid-stream is flipped to `stalled` once the stale-prompt sweeper observes no further updates beyond `[prompts].stale_threshold`. Clients reconnect through `GET /v1/sessions/{id}/snapshot`, wait for the agent process to be running, then call `POST /v1/sessions/{id}/resume` when the new adapter advertises `sessionCapabilities.resume`. If `session/resume` is unsupported, prompt resumption is not possible and a fresh session is the recovery path.
