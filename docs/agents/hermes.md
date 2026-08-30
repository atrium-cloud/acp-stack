# Hermes Agent

Hermes Agent reaches ACP through the `hermes-agent-acp` adapter, which `acp-stack` installs and launches.

## Setup

```sh
acps secrets set OPENROUTER_API_KEY
acps init --agent hermes --provider openrouter --model deepseek/deepseek-v4-flash-0731
```

The `[agent]` config block is generated from the `hermes` entry in `data/agents.toml`; `[agent.provider]` comes from the selected provider and `data/providers.toml`.

### Provider keys

- The API key stays in the encrypted secret store. It reaches the process only through `[agent].env`.
- Hermes reads provider keys from its process environment. No `~/.hermes/.env` entry is needed or written.

### The `~/.hermes/config.yaml` write contract

- `acp-stack` writes only the non-secret `model` block: `model.provider` plus `model.default`.
- `model.default` carries the bare provider-native model id. The adapter composes its `provider/model` ACP config-option ids itself.
- The rest of the file is user-owned and preserved.

### Endpoint overrides: the native base-URL env lane

A mapped provider whose Hermes overlay declares a base-URL environment variable keeps its native identity: `model.provider` stays the provider-native id, and `acp-stack` exports the rerouted endpoint into the agent launch environment under that variable (`openrouter` → `OPENROUTER_BASE_URL`, `zai` → `GLM_BASE_URL`, and the rest of the table in `src/runtime/agent/provider_keys.rs`). Hermes resolves an api-key provider's base URL as the explicit config value, then this variable, then the overlay default, so only the endpoint changes and the adapter keeps advertising `<native>/<model>` option ids.

Nothing is persisted for this lane. The value is derived from the stored override at every launch, so clearing the override simply stops exporting it and the overlay falls back to its vendor endpoint. Declaring the variable in `[agent].env` is refused — it is runtime-managed.

### Endpoint overrides and `providers.acps-managed`

The remaining endpoint-carrying configurations — a custom OpenAI-compatible provider, or a mapped provider whose overlay declares no base-URL variable (Anthropic) — keep `model.base_url` out of the file. Instead, `acp-stack` writes a named `providers.acps-managed` entry carrying:

- `name`
- `base_url`
- `key_env` — the provider-native env ref, which Hermes reads unconditionally.
- `transport` — the provider's declared wire shape from `data/providers.toml`, or the custom provider's `api`.

`model.provider` then points at `custom:acps-managed`. User-owned entries under `providers:` are preserved. Clearing the override removes the entry and restores the mapped lane; so does a provider that reaches Hermes over the native env lane, so a stale entry can never shadow it.

### Managed endpoints: per-model transport lookup

Under a managed endpoint, providers that route different models over different wires get per-model `transport` resolution:

- Their `/v1/models` listings carry no wire metadata.
- `acp-stack` looks the configured model up in the checked-in `data/endpoints.toml` table before falling back to the provider default.
- A Google-native-wire model is rejected there: that wire has no Hermes custom-lane transport. Select a different model or clear the endpoint override.

This applies only to the managed lane. OpenCode Zen/Go — the providers `data/endpoints.toml` currently covers — reach Hermes over the native env lane, where Hermes' own overlay picks the transport, so a Zen/Go Gemini model works under an override.

### Install

The adapter and the harness install from separate sources.

- Adapter: a Node script from the latest `atrium-cloud/hermes-acp` GitHub Release (`hermes-agent-acp.zip`). The recipe installs Node 22 under `~/.local/share/acp-stack/node` when the host has none, and `acps agent update` re-runs it.
- Harness: the `hermes` install downloads the upstream installer (Nous-hosted) with a 15s cap, falling back to the official GitHub-hosted copy when the download fails or stalls.
- The harness installer runs with `--skip-setup --skip-browser --skip-computer-use --non-interactive`, plus explicit `--dir ~/.hermes/hermes-agent --hermes-home ~/.hermes`. Root installs keep the managed `~/.local/bin/hermes` layout instead of the upstream FHS layout.
- The base `hermes` binary is enough. The adapter resolves `hermes` on `PATH` and drives `hermes serve`.

### Transport

The adapter spawns an isolated `hermes serve` on a loopback port. Provider and model reach it through Hermes' own `config.yaml`, written per the contract above.

### Skills

Managed Agent Skills are installed into `~/.agents/skills` and symlinked into `~/.hermes/skills`, the directory Hermes discovers. See [docs/specs/agents/skills.md](../specs/agents/skills.md) for the managed-skills semantics.

## Modes

The adapter advertises two ACP v1 session modes on `session/new`:

- `default`: per-turn approvals.
- `dont_ask`: a per-session approval bypass.

Select one with `acps agent set --mode <default|dont_ask>`.

## Known limitations

The adapter does not expose every Hermes surface over ACP:

- MCP passthrough is not advertised, so `acp-stack` composes no MCP servers for Hermes sessions. MCP servers declared in Hermes' own `config.yaml` load gateway-wide and reach every session.
- Audio prompts, breakpoint forks, and Hermes interactions requiring its own frontend are not exposed.

To change the model, run `acps agent set --model <model-id>`. This rewrites the `model` block of `~/.hermes/config.yaml`. The running agent keeps its startup model until it is restarted (`POST /v1/agent/restart`). The new model applies to sessions created after that.

## Session capabilities

The adapter advertises `loadSession` and session list, resume, load, close, delete, and head-only fork support at initialize. Capability-dependent operations remain gated by the live `initialize` response. See [docs/specs/acp/acp-bridge.md](../specs/acp/acp-bridge.md) for the generic session-resume behavior.
