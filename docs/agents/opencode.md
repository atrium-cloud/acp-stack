# OpenCode

OpenCode is a native ACP target. `acp-stack` launches `opencode acp`.

## Setup

```sh
acps init --agent opencode
acps secrets set OPENROUTER_API_KEY
acps agent set --provider openrouter --model <provider/model-id>
acps agent test
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

Set the OpenCode mode with:

```sh
acps agent set --mode <build|plan>
```

Reasoning effort follows the runtime advertisement. OpenCode advertises an `effort` (`thought_level`) option when the active model has effort variants. `acps agent set --effort <effort>` validates against that advertisement.

### Cloudflare Providers

Cloudflare providers require companion env refs alongside the main API key. Set each one with `acps secrets set` before running `acps agent set --provider`. The base `companion_env_vars` list per provider lives in `data/env_vars.toml`, with per-provider overrides on the `[[providers]]` entries in `data/providers.toml`.

## Subagent Model

OpenCode can call a `small_model` for background tasks such as title generation. OpenCode has been reported to call Anthropic Claude Haiku 4.5 over `OPENROUTER_API_KEY` auth even with a different main model selected:

- We reproduced this behavior with an `OPENROUTER_API_KEY`.
- GitHub issue [Openrouter unwated requests to Claude Haiku 4.5. #4579](https://github.com/anomalyco/opencode/issues/4579) remains open as of May 26, 2026.
- GitHub PR [fix(provider): treat empty small_model as disabled #21184](https://github.com/anomalyco/opencode/pull/21184) is still pending as of May 26, 2026.

Run `acps subagent *` to configure `small_model` directly or disable it:

```sh
acps subagent status
acps subagent set --model <provider/model-id> [--provider <provider-id>] [--api-key-ref <ref>]
acps subagent match
acps subagent free
acps subagent disable
```

### Usage

- `acps subagent set` inherits `--provider` and `--api-key-ref` from the main agent provider when omitted. The common case is `acps subagent set --model <model>`.
- `acps subagent match` makes `small_model` follow the main agent model if not already.
- `acps subagent free` selects `openrouter/free` with `OPENROUTER_API_KEY`, or `opencode/big-pickle` with `OPENCODE_API_KEY`. It errors with "Current provider does not support free." otherwise.
- `acps subagent disable` sets the model ID to an invalid string so OpenCode `small_model` requests cannot execute. This workaround stays until PR #21184 merges.

With no subagent model configured, OpenCode configured through `acp-stack` inherits the main model as the small model.

## Session Resume

`session/load`, `session/resume`, and `session/list` are discovered from the OpenCode `initialize` reply at runtime. See [docs/specs/acp/acp-bridge.md](../specs/acp/acp-bridge.md), "Session Resume Capability Matrix", for the generic resume and reconnect behavior.
