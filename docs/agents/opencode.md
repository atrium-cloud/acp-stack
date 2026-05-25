# OpenCode Headless Support

OpenCode is a native ACP target for `acp-stack`. This contract documents the non-interactive setup path used when the embedded registry advertises `headless_compatible = true`.

## Sources

- OpenCode provider docs: https://opencode.ai/docs/providers/
- OpenCode config docs: https://opencode.ai/docs/config/
- OpenCode repository: https://github.com/anomalyco/opencode

The provider and config docs were checked on 2026-05-17. They document provider configuration through `opencode.json`, environment-variable interpolation with `{env:VARIABLE_NAME}`, and provider `options.apiKey` for direct API-key configuration.

## Install

OpenCode is treated as a native ACP peer. No separate adapter is required for this entry.

The official bootstrap respects `XDG_BIN_DIR`; `acp-stack` sets it to `$HOME/.local/bin` so runtime-managed install verification and later agent launch resolve the same command without relying on shell startup-file PATH edits. The script also passes `--no-modify-path` because the runtime controls launch PATH explicitly.

Install path metadata is maintained in `data/agents.toml`.

## ACP Launch

Recommended `acp-stack` agent config shape:

```toml
[agent]
id = "opencode"
name = "OpenCode"
command = "opencode"
args = ["acp"]
cwd = "/workspace"
env = ["<provider-api-key-ref>"]
restart = "on-crash"
```

The runtime launches `opencode acp` with `cwd` set to the configured agent cwd or `workspace.root`. Only variables listed in `[agent].env` are injected from the encrypted `acp-stack` secret store. The env refs must match the selected provider.

OpenCode advertises ACP mode values through `session/new`. `acps agent set --mode <mode>` validates against that list; the current real ACP probe returned `build` and `plan`.

## Auth And Provider Setup

`acps init` may create or merge `~/.config/opencode/opencode.json` after provider selection. Without a model, the generated config binds the selected provider config to the configured env ref and leaves the model unset:

```json
{
  "$schema": "https://opencode.ai/config.json",
  "provider": {
    "<provider>": {
      "models": {},
      "options": {
        "apiKey": "{env:<provider-api-key-ref>}"
      }
    }
  }
}
```

`acps agent set` is the main-model edit path after init. When a model is supplied, the generated config sets OpenCode's provider-qualified `model` value. `acps subagent set` is the auxiliary-model edit path and maps to OpenCode `small_model`. If no subagent config exists and the main OpenCode model exists, `acps` writes `small_model` equal to `model` so OpenCode does not fall back to its own implicit small-model choice. The provider id matters because a default secret ref is not valid for every provider. Init and provider edits validate the provider id against the provider/env mapping; model values are validated against OpenCode's ACP `model` config option.

When main and subagent providers differ, generated `opencode.json` includes provider blocks for both and sets `enabled_providers` to the union of configured OpenCode provider ids.

`acps subagent free` selects a known free `small_model` for OpenRouter (`openrouter/free`) or OpenCode Go/Zen (`opencode/big-pickle`). `acps subagent disable` writes `small_model = "invalid/model"` which in our testing prevents OpenCode from calling Haiku 4.5.

OpenCode provider ids in the provider metadata are sourced from the OpenCode provider docs and `models.dev`.

Cloudflare provider setup requires additional env refs from the mapping:

| Provider id                 | Required env refs                                                        |
| --------------------------- | ------------------------------------------------------------------------ |
| `cloudflare-workers-ai`     | `CLOUDFLARE_API_KEY`, `CLOUDFLARE_ACCOUNT_ID`                            |
| `cloudflare-ai-gateway`     | `CLOUDFLARE_API_TOKEN`, `CLOUDFLARE_ACCOUNT_ID`, `CLOUDFLARE_GATEWAY_ID` |

## Unsupported Auth Paths

The following are not supported by the `acp-stack` headless contract:

- `/connect` flows that require the OpenCode TUI.
- Browser OAuth or account-cookie based login.
- Passing browser session cookies through `acp-stack` secrets.
- Agents that require interactive model selection before ACP startup.
