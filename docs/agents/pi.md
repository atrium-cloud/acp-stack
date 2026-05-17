# Pi Agent Headless Support

Pi Agent is an adapter-backed target for `acp-stack`. The upstream harness is Pi; the ACP-facing process is `pi-acp`.

## Sources

- Pi docs: https://pi.dev/docs/latest
- Pi provider docs: https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/providers.md
  - `pi-acp` repository: https://github.com/svkozak/pi-acp

The Pi provider docs were checked on 2026-05-17. They document API-key providers through environment variables or `~/.pi/agent/auth.json`, Cloudflare companion refs, Azure endpoint/resource-name refs, AWS credential modes for Bedrock, and Vertex project/location refs.

## Install

For adapter-backed agents, the harness step installs Pi and the adapter step installs the ACP-facing `pi-acp` process. The Pi harness is available through the official shell bootstrap, npm, and GitHub Releases; pinned harness installs use GitHub Releases. The `pi-acp` adapter currently installs through npm because the GitHub release only publishes source archives.

Install path metadata is maintained in `data/agents.toml`. Npm installs use the runtime-managed local prefix so their executables land in `$HOME/.local/bin`, the directory that `acps agent install` verifies.

## ACP Launch

Recommended `acp-stack` agent config:

```toml
[agent]
id = "pi"
name = "Pi Agent"
command = "pi-acp"
args = []
cwd = "/workspace"
env = ["<provider-api-key-ref>"]
restart = "on-crash"
```

`pi-acp` launches Pi in RPC mode internally. The configured env list should include the provider credentials required for the chosen Pi provider.

## Auth And Provider Setup

For headless deployments, use Pi's API-key provider env vars or a pre-provisioned auth file.

Runtime secret refs are defined by the shared API-key/provider mapping in `data/mapping.toml` and summarized in `docs/specs/agents/api_key.md`. The embedded mapping covers Pi's documented API-key provider ids. Where Pi and OpenCode or Models.dev use different ids for the same service, Pi keeps the Pi provider id and the alternate catalog id is scoped to OpenCode.

Pi provider ids in `data/mapping.toml` are sourced from Pi Agent Providers docs page.

Pi's provider is part of the model string. `acps init` may select the initial provider to collect required env refs, but it does not infer `enabledModels` from configured secret refs. `acps agent set` validates the selected model against Pi's ACP `model` config option and writes `[agent.provider].model` into `~/.pi/agent/settings.json`.

Pi keeps Cloudflare model values in Pi's native form for Workers AI and Cloudflare AI Gateway.

Cloudflare provider setup requires additional env refs from the mapping:

| Provider id                 | Required env refs                                                        |
| --------------------------- | ------------------------------------------------------------------------ |
| `cloudflare-workers-ai`     | `CLOUDFLARE_API_KEY`, `CLOUDFLARE_ACCOUNT_ID`                            |
| `cloudflare-ai-gateway`     | `CLOUDFLARE_API_KEY`, `CLOUDFLARE_ACCOUNT_ID`, `CLOUDFLARE_GATEWAY_ID`   |

## Unsupported Auth Paths

Unsupported for the `acp-stack` headless contract:

- Pi subscription `/login` flows that require a terminal/browser handoff.
- OAuth credentials created interactively during init.
- Provider setup that requires launching the Pi TUI before ACP startup.
