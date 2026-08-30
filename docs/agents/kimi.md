# Kimi Code

Kimi Code is a native ACP target. `acp-stack` launches `kimi acp`.

Authentication is headless. `acp-stack` builds an in-memory provider from a `KIMI_MODEL_*` launch environment. Headless env auth replaces Kimi's interactive OAuth flow.

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

Kimi Code selects between mapped provider lanes. The credential and endpoint differ per lane; each lane accepts only its own, and the runtime fixes each lane's endpoint at launch. Lane ids and key refs are declared in [../../data/providers.toml](../../data/providers.toml) and [../../data/env_vars.toml](../../data/env_vars.toml).

- Kimi For Coding (`kimi-for-coding`, `kimi-code`, and aliases): the subscription surface.
- Kimi For Coding (Global) (`kimi-coding-global`): the global-region subscription surface.
- Moonshot AI (`moonshotai`; `moonshotai-cn` for the China endpoint): the pay-as-you-go platform.

Select a lane with `acps init --agent kimi --provider <id>` or `acps agent provider use <id>`. The selection swaps the `[agent].env` credential declaration to the lane's key.

### Custom providers

Custom providers accept any Anthropic- or OpenAI-compatible endpoint via `--custom-provider`:

- Set `--provider-api` to `chat-completions`, `anthropic-messages`, or `responses`.
- The declared API maps onto `KIMI_MODEL_PROVIDER_TYPE` (`openai`, `anthropic`, `openai_responses`). `responses` requires Kimi Code's v2 engine (the default).
- The custom base URL, context, output cap, and display name flow through the same `KIMI_MODEL_*` launch environment.
- Model capabilities fall through to Kimi Code's defaults.

A legacy config predating `[agent.provider]` still launches on the Kimi For Coding subscription lane. All configuration surfaces now require an explicit provider selection.

## Launch environment

The API key stays in the encrypted secret store. At launch, `acp-stack`:

- passes its value as `KIMI_MODEL_API_KEY`
- selects the model through `KIMI_MODEL_NAME`
- sets `KIMI_MODEL_BASE_URL` to the selected lane's endpoint; a managed-state endpoint override replaces its origin and keeps the lane's path (`/v1` on the Moonshot platform, `/coding/v1` on the subscription lanes), see [Endpoint overrides](../specs/extensions.md#endpoint-overrides)

Reserve `[agent].env` for the credential ref; `acp-stack` owns the `KIMI_MODEL_*` launch vars.

## Models

- When `--model` is not passed, `acps init` pins a per-lane default: `kimi-for-coding` on the subscription lanes (available on every Kimi plan) and `kimi-k3` on the Moonshot platform. A model already present in config is kept.
- K3 requires a Moderato plan or above on the subscription lanes. Eligible users select it with `acps init --agent kimi --provider kimi-code --model k3` or `acps agent set --model k3`.
- Model ids are accepted as supplied, without ACP discovery, because Kimi requires the model environment to initialize. Kimi Code validates the id when the process starts.
- If a hand-edited config omits the model, the runtime launches with the lane default.
- Mode values are discovered over ACP. Select one with `acps agent set --mode <mode>`. A non-interactive init without `--mode` selects `yolo` (the registry `default_mode`): Kimi's default mode raises an ACP permission request for every tool call, which the daemon path parks on an operator decision.
- Reasoning effort is discovered the same way. Kimi advertises a `thinking` (`thought_level`) option offering `off` plus the model's declared effort levels. Set it with `acps agent set --effort <effort>`.

## Sessions and capabilities

Kimi Code receives configured MCP servers through ACP. Managed Agent Skills are installed into `~/.agents/skills`.

The native ACP implementation advertises session list, load, and resume support at runtime. Capability-dependent operations remain gated by the live `initialize` response.
