# Hermes Agent

Hermes Agent is a native ACP target. `acp-stack` launches `hermes acp`.

## Known limitations

Hermes advertises session models and modes through the pre-1.0 `models`/`modes` session state instead of ACP v1 `configOptions`, and its `initialize` response carries no `mcpCapabilities`. Until upstream adopts the v1 shapes: model ids are accepted as supplied without ACP discovery, mode selection is unavailable (`set_mode = false`), and configured MCP servers are recorded as ignored features for Hermes sessions rather than delivered.

The same gap means a Hermes session's model cannot be switched after creation: with no v1 `configOptions` there is no `session/set_config_option` target, so live model switching (as Goose supports) is unavailable. Changing the model goes through `acps agent set --model`, which rewrites the `model` block of `~/.hermes/config.yaml`; the running agent keeps its startup model until it is restarted (`POST /v1/agent/restart`), and the new model applies to sessions created after that.

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

Hermes is provider-backed: the API key stays in the encrypted secret store and reaches the process only through `[agent].env` (Hermes reads provider keys from its process environment; no `~/.hermes/.env` entry is needed or written). `acp-stack` writes the non-secret `model` block of `~/.hermes/config.yaml` — `model.provider` plus `model.default` carrying the bare provider-native model id, since Hermes composes its `provider:model` ACP ids itself; the rest of that file is user-owned and preserved. An endpoint-carrying configuration — a custom OpenAI-compatible provider, or a mapped provider with a credential endpoint override — never writes `model.base_url` (upstream honors it unevenly across native lanes, and the bare `custom` lane cannot carry a credential on a loopback endpoint). Instead `acp-stack` writes a named `providers.acps-managed` entry carrying `name`, `base_url`, `key_env` (the provider-native env ref, which Hermes reads unconditionally), and `transport` (the provider's declared wire shape from `data/providers.toml`, or the custom provider's `api`), and points `model.provider` at `custom:acps-managed`; user-owned entries under `providers:` are preserved, and clearing the override removes the entry and restores the mapped lane.

Under a managed endpoint, OpenCode Zen/Go get per-model `transport` resolution: those providers route different models over different wires and their `/v1/models` listings carry no wire metadata, so `acp-stack` looks the configured model up in the checked-in `data/endpoints.toml` table (mirrored from the Zen/Go docs pages) before falling back to the provider default. A Zen/Go Gemini model is rejected there — the Google-native wire has no Hermes custom-lane transport — so select a different model or clear the endpoint override.

The install step downloads the upstream installer (Nous-hosted, falling back to the official GitHub-hosted copy when the vendor download fails or stalls — the ~150KB script fetches in under a second from a healthy host, so a 15s cap means a dead host, not a slow one) and runs it with `--skip-setup --skip-browser --skip-computer-use --non-interactive` plus explicit `--dir ~/.hermes/hermes-agent --hermes-home ~/.hermes` so root installs keep the managed `~/.local/bin/hermes` layout instead of the upstream FHS layout; current installers bundle ACP mode, and the optional `.[acp]` extra is installed into the Hermes checkout only when the `hermes acp` entry point is missing. The installed launcher is a `#!` shell wrapper, which satisfies the executable-format and spawn gates.

At launch, `acp-stack` sets `HERMES_ACP_SKIP_CONFIGURED_MCP=1` so MCP servers declared in Hermes' own `config.yaml` do not leak into acps-managed sessions; acps owns MCP composition. Do not add that variable to `[agent].env`.

Managed Agent Skills are installed into `~/.agents/skills` and symlinked into `~/.hermes/skills`, the directory Hermes discovers.

The native ACP implementation advertises `loadSession` and session list, resume, and fork support at initialize. Capability-dependent operations remain gated by the live `initialize` response.
