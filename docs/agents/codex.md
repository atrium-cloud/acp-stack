# Codex

Codex is adapter-backed. `acp-stack` launches `codex-acp`, which launches Codex.

Starting from Dec 10, 2025, Codex CLI [no longer supports Chat Completions-style API endpoints](https://github.com/openai/codex/discussions/7782). As such, a Responses API-compatible provider must be used if not using OpenAI as provider.

## Setup

```sh
acps init --agent codex
```

Codex supports two mapped provider ids.

For Codex's built-in OpenAI provider:

```sh
acps agent set --provider openai --model <model-id>
```

You must use an `OPENAI_API_KEY` for this provider. Switching from a generated non-OpenAI provider to `openai` backs up the prior `~/.codex/config.toml` with a numeric suffix.

For [OpenRouter](https://openrouter.ai/docs/cookbook/coding-agents/codex-cli):

```sh
acps secrets set OPENROUTER_API_KEY
acps agent set --provider openrouter --model <model-id>
```

OpenRouter config is written to `~/.codex/config.toml` with `https://openrouter.ai/api/v1` as the Responses base URL. Following the OpenRouter cookbook, authentication uses a command-based `auth` block that echoes `OPENROUTER_API_KEY` instead of a plain `env_key`, so Codex refreshes its model catalog and non-OpenAI models get correct metadata.

Codex mode values (read-only, auto, full-access) are supported through:

```sh
acps agent set --mode <mode>
```

## Session Resume

`session/load`, `session/resume`, and `session/list` are discovered from the `codex-acp` adapter's `initialize` reply at runtime; `data/agents.toml` does not pin a value. End-to-end resume behavior against `acp-stack` is not currently confirmed.

If the live ACP connection to `codex-acp` drops, `restart = "on-crash"` relaunches the supervised agent automatically. Any prompt that was mid-stream is flipped to `stalled` once the stale-prompt sweeper observes no further updates beyond `[prompts].stale_threshold`. Clients reconnect through `GET /v1/sessions/{id}/snapshot`, wait for the agent process to be running, then call `POST /v1/sessions/{id}/resume` when the new adapter advertises `sessionCapabilities.resume`. When `session/resume` is unsupported, the recovery path is a new `POST /v1/sessions`.
