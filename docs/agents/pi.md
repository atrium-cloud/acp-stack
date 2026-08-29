# Pi Agent

Pi Agent reaches ACP through the `pi-acp` adapter, which `acp-stack` installs and launches. The adapter drives Pi in RPC mode and brings its own MCP client.

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

### Install

The adapter and the harness install from separate sources.

- Harness: the upstream `pi.dev` installer, with npm and GitHub Release fallbacks. The recipe installs Node 22 under `~/.local/share/acp-stack/node` when the host has none.
- Adapter: a Node script from the latest `atrium-cloud/pi-acp` GitHub Release (`pi-acp.zip`); `acps agent update` re-runs the recipe.

### Launch

The adapter bundle carries no Pi, so the bridge names the managed `pi` through the process-only `PI_ACP_PI_BIN` before launching `pi-acp`. Declaring that variable in `[agent].env` is rejected.

## Cloudflare Providers

Cloudflare providers require companion env refs alongside the main API key. The base `companion_env_vars` list per provider lives in [../../data/env_vars.toml](../../data/env_vars.toml), with per-provider overrides in [../../data/providers.toml](../../data/providers.toml).

Note: Pi uses `CLOUDFLARE_API_KEY` for `cloudflare-ai-gateway`; OpenCode uses `CLOUDFLARE_API_TOKEN`.

## MCP

The adapter advertises `mcpCapabilities.http` and `mcpCapabilities.sse` at initialize, so `acp-stack` composes the configured MCP servers (stdio, streamable HTTP, SSE) into every Pi session. The adapter's own Pi extension connects them and registers their tools as `mcp__<server>__<tool>`; every MCP tool call goes through the permission gate.

## Modes and effort

Pi has no session modes; `set_mode` stays off. The thinking level is exposed as the `thought_level` config option and selected with `acps agent set --effort <level>`.

## Session capabilities

The adapter advertises `loadSession` and session list, resume, load, close, delete, and head-only fork support at initialize. Capability-dependent operations remain gated by the live `initialize` response. See [../specs/acp/acp-bridge.md](../specs/acp/acp-bridge.md) for the generic session-resume behavior.
