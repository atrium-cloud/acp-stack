# Kimi Code

Kimi Code is a native ACP target. `acp-stack` launches `kimi acp`.

## Setup

```sh
acps secrets set KIMI_API_KEY
acps init --agent kimi
acps agent set --model <kimi-model-id>
```

Agent config shape:

```toml
[agent]
id = "kimi"
command = "kimi"
args = ["acp"]
cwd = "/workspace"
env = ["KIMI_API_KEY"]
restart = "on-crash"
model = "k3"
```

`KIMI_API_KEY` stays in the encrypted secret store. At launch, `acp-stack` passes its value as `KIMI_MODEL_API_KEY`, selects `agent.model` through `KIMI_MODEL_NAME`, and fixes `KIMI_MODEL_BASE_URL` to the first-party Kimi Code endpoint. Do not add `KIMI_MODEL_*` refs to `[agent].env`.

`acps init` pins `kimi-for-coding` when `--model` is not passed because that id is available on every Kimi plan; a model already present in config is kept. K3 requires a Moderato plan or above; eligible users can select it with `acps init --agent kimi --model k3` or `acps agent set --model k3`. Model ids are accepted as supplied without ACP discovery because Kimi requires the model environment to initialize; Kimi Code validates the id when the process starts. If a hand-edited config omits `agent.model`, the runtime launches with `kimi-for-coding`. Mode values are discovered over ACP and can be selected with `acps agent set --mode <mode>`.

Kimi Code receives configured MCP servers through ACP. Managed Agent Skills are installed into `~/.agents/skills`.

The native ACP implementation advertises session list, load, and resume support at runtime. Capability-dependent operations remain gated by the live `initialize` response.
