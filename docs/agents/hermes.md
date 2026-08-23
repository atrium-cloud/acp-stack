# Hermes Agent

Hermes Agent is a native ACP target. `acp-stack` launches `hermes acp`.

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
- `model.default` carries the bare provider-native model id. Hermes composes its `provider:model` ACP ids itself.
- The rest of the file is user-owned and preserved.

### Endpoint overrides and `providers.acps-managed`

An endpoint-carrying configuration — a custom OpenAI-compatible provider, or a mapped provider with a credential endpoint override — keeps `model.base_url` out of the file. Instead, `acp-stack` writes a named `providers.acps-managed` entry carrying:

- `name`
- `base_url`
- `key_env` — the provider-native env ref, which Hermes reads unconditionally.
- `transport` — the provider's declared wire shape from `data/providers.toml`, or the custom provider's `api`.

`model.provider` then points at `custom:acps-managed`. User-owned entries under `providers:` are preserved. Clearing the override removes the entry and restores the mapped lane.

### Managed endpoints: per-model transport lookup

Under a managed endpoint, OpenCode Zen/Go get per-model `transport` resolution:

- These providers route different models over different wires.
- Their `/v1/models` listings carry no wire metadata.
- `acp-stack` looks the configured model up in the checked-in `data/endpoints.toml` table (mirrored from the Zen/Go docs pages) before falling back to the provider default.
- A Zen/Go Gemini model is rejected there: the Google-native wire has no Hermes custom-lane transport. Select a different model or clear the endpoint override.

### Install step

- The install downloads the upstream installer (Nous-hosted) with a 15s cap, falling back to the official GitHub-hosted copy when the download fails or stalls.
- The installer runs with `--skip-setup --skip-browser --skip-computer-use --non-interactive`, plus explicit `--dir ~/.hermes/hermes-agent --hermes-home ~/.hermes`. Root installs keep the managed `~/.local/bin/hermes` layout instead of the upstream FHS layout.
- Current installers bundle ACP mode. The optional `.[acp]` extra is installed into the Hermes checkout only when the `hermes acp` entry point is missing.
- The installed launcher is a `#!` shell wrapper.

### MCP isolation

At launch, `acp-stack` sets `HERMES_ACP_SKIP_CONFIGURED_MCP=1`. MCP servers declared in Hermes' own `config.yaml` then stay out of acps-managed sessions; acps owns MCP composition. Keep that variable out of `[agent].env`.

### Skills

Managed Agent Skills are installed into `~/.agents/skills` and symlinked into `~/.hermes/skills`, the directory Hermes discovers. See [docs/specs/agents/skills.md](../specs/agents/skills.md) for the managed-skills semantics.

## Known limitations

Hermes uses pre-1.0 ACP shapes:

- Session models and modes are advertised through the pre-1.0 `models`/`modes` session state, not ACP v1 `configOptions`.
- The `initialize` response carries no `mcpCapabilities`.

Until upstream adopts the v1 shapes:

- Model ids are accepted as supplied, without ACP discovery.
- Mode selection is unavailable (`set_mode = false`).
- Configured MCP servers are recorded as ignored features for Hermes sessions rather than delivered.
- Live model switching is unavailable; with no v1 `configOptions` there is no `session/set_config_option` target.

To change the model, run `acps agent set --model <model-id>`. This rewrites the `model` block of `~/.hermes/config.yaml`. The running agent keeps its startup model until it is restarted (`POST /v1/agent/restart`). The new model applies to sessions created after that.

## Session capabilities

The native ACP implementation advertises `loadSession` and session list, resume, and fork support at initialize. Capability-dependent operations remain gated by the live `initialize` response. See [docs/specs/acp/acp-bridge.md](../specs/acp/acp-bridge.md) for the generic session-resume behavior.
