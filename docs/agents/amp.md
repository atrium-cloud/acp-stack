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
