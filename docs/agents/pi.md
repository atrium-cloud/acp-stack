# Pi Agent

Pi Agent is adapter-backed. `acp-stack` launches `pi-acp`, which launches Pi in RPC mode.

`pi-acp` 0.0.31 or newer is required. Older adapter releases advertise models through the pre-1.0 ACP `models` session state, a channel `acp-stack` retired on ACP v1. Verified 2026-07-07: 0.0.27 fails model selection; 0.0.31 advertises a `model` session config option. `acps` installs the latest adapter from npm, so this only affects externally managed installs.

## Setup

```sh
acps init --agent pi
acps secrets set <provider-api-key-ref>
acps agent set --provider <provider-id> --model <pi-model-id>
```

Agent config shape:

```toml
[agent]
id = "pi"
command = "pi-acp"
args = []
cwd = "/workspace"
env = ["<provider-api-key-ref>"]
restart = "on-crash"
```

Provider credentials are injected through `[agent].env`. Provider ids and default secret refs are summarized in [../specs/agents/api_key.md](../specs/agents/api_key.md).

`acps agent set` writes the selected model into Pi's agent settings. Pi keeps Cloudflare model values in Pi's native form. Custom providers are supported when the required base URL, API family, model, and secret ref are supplied explicitly.

## Cloudflare Providers

Cloudflare providers require companion env refs alongside the main API key. The base `companion_env_vars` list per provider lives in [../../data/env_vars.toml](../../data/env_vars.toml), with per-provider overrides in [../../data/providers.toml](../../data/providers.toml).

Note: Pi uses `CLOUDFLARE_API_KEY` for `cloudflare-ai-gateway`; OpenCode uses `CLOUDFLARE_API_TOKEN`.

## Session Resume

`session/load`, `session/resume`, and `session/list` are discovered from the `pi-acp` adapter's `initialize` reply at runtime.

For crash recovery, stalled prompts, and client reconnection, see [../specs/acp/acp-bridge.md](../specs/acp/acp-bridge.md).
