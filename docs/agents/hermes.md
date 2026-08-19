# Hermes Agent

Hermes Agent is a native ACP target. `acp-stack` launches `hermes acp`.

## Known limitation

Hermes advertises session models and modes through the pre-1.0 `models`/`modes` session state instead of ACP v1 `configOptions`, and its `initialize` response carries no `mcpCapabilities`. Until upstream adopts the v1 shapes: model ids are accepted as supplied without ACP discovery, mode selection is unavailable (`set_mode = false`), and configured MCP servers are recorded as ignored features for Hermes sessions rather than delivered.

## Setup

```sh
acps secrets set OPENROUTER_API_KEY
acps init --agent hermes --provider openrouter --model deepseek/deepseek-v4-flash-0731
```

Agent config shape:

```toml
[agent]
id = "hermes"
command = "hermes"
args = ["acp"]
cwd = "/workspace"
env = ["OPENROUTER_API_KEY"]
restart = "on-crash"

[agent.provider]
id = "openrouter"
model = "deepseek/deepseek-v4-flash-0731"
api_key_ref = "OPENROUTER_API_KEY"
```

Hermes is provider-backed: the API key stays in the encrypted secret store and reaches the process only through `[agent].env` (Hermes reads provider keys from its process environment; no `~/.hermes/.env` entry is needed or written). `acp-stack` writes the non-secret `model` block of `~/.hermes/config.yaml` — `model.provider` plus `model.default` carrying the bare provider-native model id, since Hermes composes its `provider:model` ACP ids itself; the rest of that file is user-owned and preserved. A custom OpenAI-compatible endpoint maps to `model.provider = "custom"` with `model.base_url`.

The install step runs the upstream installer with `--skip-browser`; current installers bundle ACP mode, and the optional `.[acp]` extra is installed into the Hermes checkout only when the `hermes acp` entry point is missing. The installed launcher is a `#!` shell wrapper, which satisfies the executable-format and spawn gates.

At launch, `acp-stack` sets `HERMES_ACP_SKIP_CONFIGURED_MCP=1` so MCP servers declared in Hermes' own `config.yaml` do not leak into acps-managed sessions; acps owns MCP composition. Do not add that variable to `[agent].env`.

Managed Agent Skills are installed into `~/.agents/skills` and symlinked into `~/.hermes/skills`, the directory Hermes discovers.

The native ACP implementation advertises `loadSession` and session list, resume, and fork support at initialize. Capability-dependent operations remain gated by the live `initialize` response.
