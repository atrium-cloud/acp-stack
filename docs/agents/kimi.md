# Kimi Code

Kimi Code is a native ACP target. `acp-stack` launches `kimi acp`. Authentication is headless: the `KIMI_MODEL_*` environment contract builds an in-memory provider from `KIMI_MODEL_API_KEY` / `KIMI_MODEL_NAME` / `KIMI_MODEL_BASE_URL`, which is the launch environment `acp-stack` constructs; Kimi's interactive OAuth flow is not used.

## Setup

```sh
acps init --agent kimi --provider kimi-code
acps secrets set KIMI_API_KEY
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

[agent.provider]
id = "kimi-code"
model = "k3"
```

## Providers

Kimi Code selects between mapped provider lanes; the credential and endpoint differ per lane and are not interchangeable.

- Kimi For Coding (provider ids `kimi-for-coding`, `kimi-code`, and aliases): the subscription surface at `https://api.kimi.com/coding/v1`, authenticated with `KIMI_API_KEY` (a coding-plan credential).
- Kimi For Coding (Global) (`kimi-coding-global`): the global-region subscription surface at `https://api.kimi.ai/coding/v1`, also authenticated with `KIMI_API_KEY`.
- Moonshot AI (`moonshotai`, or `moonshotai-cn` for the China endpoint): the pay-as-you-go platform at `https://api.moonshot.ai/v1`, authenticated with `MOONSHOT_API_KEY`. Select a lane with `acps init --agent kimi --provider <id>` or `acps agent provider use <id>`; the selection swaps the `[agent].env` credential declaration to the lane's key.
- Custom providers: any Anthropic- or OpenAI-compatible endpoint via `--custom-provider`, with `--provider-api` `chat-completions`, `anthropic-messages`, or `responses`. The declared API maps onto Kimi Code's `KIMI_MODEL_PROVIDER_TYPE` (`openai`, `anthropic`, `openai_responses`); `responses` requires Kimi Code's v2 engine (the default). The custom base URL, context, output cap, and display name flow through the same `KIMI_MODEL_*` launch environment; model capabilities are not set and follow Kimi Code's defaults.

A legacy config without `[agent.provider]` still launches on the Kimi For Coding subscription lane; all configuration surfaces now require an explicit provider selection.

The api key stays in the encrypted secret store. At launch, `acp-stack` passes its value as `KIMI_MODEL_API_KEY`, selects the model through `KIMI_MODEL_NAME`, and sets `KIMI_MODEL_BASE_URL` to the selected lane's endpoint. Do not add `KIMI_MODEL_*` refs to `[agent].env`.

`acps init` pins a per-lane default model when `--model` is not passed: `kimi-for-coding` on the subscription lanes (available on every Kimi plan) and `kimi-k3` on the Moonshot platform; a model already present in config is kept. K3 requires a Moderato plan or above on the subscription lanes; eligible users can select it with `acps init --agent kimi --provider kimi-code --model k3` or `acps agent set --model k3`. Model ids are accepted as supplied without ACP discovery because Kimi requires the model environment to initialize; Kimi Code validates the id when the process starts. If a hand-edited config omits the model, the runtime launches with the lane default. Mode values are discovered over ACP and can be selected with `acps agent set --mode <mode>`. Reasoning effort is discovered the same way — Kimi advertises a `thinking` (`thought_level`) option offering `off` plus the model's declared effort levels — and set with `acps agent set --effort <effort>`.

Kimi Code receives configured MCP servers through ACP. Managed Agent Skills are installed into `~/.agents/skills`.

The native ACP implementation advertises session list, load, and resume support at runtime. Capability-dependent operations remain gated by the live `initialize` response.
