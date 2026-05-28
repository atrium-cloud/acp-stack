# Pi Agent

Pi Agent is adapter-backed. `acp-stack` launches `pi-acp`, which launches Pi in RPC mode.

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

Pi provider credentials are injected through `[agent].env`. Provider ids and default secret refs are summarized in [../specs/agents/api_key.md](../specs/agents/api_key.md).

`acps agent set` writes the selected model into Pi's agent settings. Pi keeps Cloudflare model values in Pi's native form. Custom providers are supported when the required base URL, API family, model, and secret ref are supplied explicitly.

## Cloudflare Providers

Cloudflare providers require companion env refs alongside the main API key. Note that Pi uses `CLOUDFLARE_API_KEY` for `cloudflare-ai-gateway` (OpenCode uses `CLOUDFLARE_API_TOKEN`).

| Provider id             | Required env refs                                                      |
| ----------------------- | ---------------------------------------------------------------------- |
| `cloudflare-workers-ai` | `CLOUDFLARE_API_KEY`, `CLOUDFLARE_ACCOUNT_ID`                          |
| `cloudflare-ai-gateway` | `CLOUDFLARE_API_KEY`, `CLOUDFLARE_ACCOUNT_ID`, `CLOUDFLARE_GATEWAY_ID` |

## Session Resume

`session/load`, `session/resume`, and `session/list` are discovered from the `pi-acp` adapter's `initialize` reply at runtime; `data/agents.toml` does not pin a value. End-to-end resume behavior against `acp-stack` is not currently confirmed.

If the live ACP connection to `pi-acp` drops, `restart = "on-crash"` relaunches the supervised agent automatically. Any prompt that was mid-stream is flipped to `stalled` once the stale-prompt sweeper observes no further updates beyond `[prompts].stale_threshold`. Clients reconnect through `GET /v1/sessions/{id}/snapshot`, wait for the agent process to be running, then call `POST /v1/sessions/{id}/resume` when the new adapter advertises `sessionCapabilities.resume`. If `session/resume` is unsupported, prompt resumption is not possible and a fresh session is the recovery path.
