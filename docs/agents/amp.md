# Amp Code

Amp Code is adapter-backed. `acp-stack` launches `amp-acp`, which launches the Amp CLI through the adapter.

## Setup

Run `acps init`, then store the API key:

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

## Model and Mode Selection

Provider selection stays inside Amp itself. Model selection uses Amp's execution tiers:

```sh
acps agent set --model <low|medium|high|ultra>
```

The tiers are advertised at runtime as the `amp-mode` Model-category config option. This requires `amp-acp` v0.8.0 or later; older adapters advertise no Model-category option. The adapter default is `medium`.

Mode selection controls permission behavior, advertised as the `permission` Mode-category option:

```sh
acps agent set --mode <default|bypass>
```

## Session Resume

`session/load`, `session/resume`, and `session/list` are discovered from the `amp-acp` adapter's `initialize` reply at runtime; `data/agents.toml` omits the value. See the "Sessions" and "Session Resume Capability Matrix" sections of `docs/specs/acp/acp-bridge.md` for the generic capability and recovery contract.
