# OpenCode

OpenCode is a native ACP target. `acp-stack` launches `opencode acp`.

## Setup

```sh
acps init --agent opencode
acps secrets set <provider-api-key-ref>
acps agent set --provider <provider-id> --model <provider/model-id>
```

Agent config shape:

```toml
[agent]
id = "opencode"
command = "opencode"
args = ["acp"]
cwd = "/workspace"
env = ["<provider-api-key-ref>"]
restart = "on-crash"
```

OpenCode reads provider config from `~/.config/opencode/opencode.json`. `acps agent set` writes that file with the selected provider, model, API-key env reference, and enabled provider list.

OpenCode mode can be set with:

```sh
acps agent set --mode <build|plan>
```

### Cloudflare Providers

Cloudflare providers require companion env refs alongside the main API key. Set each one with `acps secrets set` before running `acps agent set --provider`.

| Provider id             | Required env refs                                                        |
| ----------------------- | ------------------------------------------------------------------------ |
| `cloudflare-workers-ai` | `CLOUDFLARE_API_KEY`, `CLOUDFLARE_ACCOUNT_ID`                            |
| `cloudflare-ai-gateway` | `CLOUDFLARE_API_TOKEN`, `CLOUDFLARE_ACCOUNT_ID`, `CLOUDFLARE_GATEWAY_ID` |

## Subagent Model

OpenCode can call a `small_model` for background tasks such as title generation.
It has been reported that OpenCode can call Anthropic Claude Haiku 4.5 when using `OPENROUTER_API_KEY` for auth even when the main model is not Haiku 4.5.
- We have reproduced this behavior using an `OPENROUTER_API_KEY`.
- GitHub issue [Openrouter unwated requests to Claude Haiku 4.5. #4579](https://github.com/anomalyco/opencode/issues/4579) remains open as of May 26, 2026.
- GitHub PR [fix(provider): treat empty small_model as disabled #21184](https://github.com/anomalyco/opencode/pull/21184) has not been merged as of May 26, 2026.

To make this more easily configurable, you can run `acps subagent *` commands to configure `small_model` directly or disable it:

```sh
acps subagent status
acps subagent set --model <provider/model-id> [--provider <provider-id>] [--api-key-ref <ref>]
acps subagent match
acps subagent free
acps subagent disable
```

Usage:
- `acps subagent set` inherits `--provider` and `--api-key-ref` from the main agent provider when omitted, so the common case is `acps subagent set --model <model>`.
- `acps subagent match` makes `small_model` follow the main agent model if not already.
- `acps subagent free` selects `openrouter/free` if using `OPENROUTER_API_KEY` or `opencode/big-pickle` if using `OPENCODE_API_KEY`; errors with "Current provider does not support free." otherwise.
- `acps subagent disable` sets model ID to an invalid string to ensure that OpenCode `small_model` requests cannot be executed. This is a tried-and-true workaround that will remain until PR#21184 is merged.

When no subagent model is configured, OpenCode configured through `acp-stack` defaults to inheriting the main model for the small model.

## Session Resume

`session/load`, `session/resume`, and `session/list` are discovered from the OpenCode `initialize` reply at runtime; `data/agents.toml` does not pin a value. End-to-end resume behavior against `acp-stack` is not currently confirmed.

If the live ACP connection to OpenCode drops, `restart = "on-crash"` relaunches the supervised agent automatically. Any prompt that was mid-stream is flipped to `stalled` once the stale-prompt sweeper observes no further updates beyond `[prompts].stale_threshold`. Clients reconnect by calling `GET /v1/sessions/{id}/snapshot`, wait for the agent process to be running, then call `POST /v1/sessions/{id}/resume` when the new OpenCode advertises `sessionCapabilities.resume`. If it does not, a fresh `POST /v1/sessions` is the recovery path and the prior prompt history remains as durable events.
