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
acps agent provider credential add openai
acps agent provider use openai --model <model-id>
```

`credential add` prompts for the key; to reuse one already in the store, run `acps secrets set OPENAI_API_KEY` first and pass `--from-secret OPENAI_API_KEY=OPENAI_API_KEY`. Switching from a generated non-OpenAI provider to `openai` backs up the prior `~/.codex/config.toml` with a numeric suffix.

For [OpenRouter](https://openrouter.ai/docs/cookbook/coding-agents/codex-cli):

```sh
acps secrets set OPENROUTER_API_KEY
acps agent provider use openrouter --model <model-id>
```

OpenRouter config is written to `~/.codex/config.toml` with `https://openrouter.ai/api/v1` as the Responses base URL. Following the OpenRouter cookbook, authentication uses a command-based `auth` block that echoes `OPENROUTER_API_KEY` instead of a plain `env_key`, so Codex refreshes its model catalog and non-OpenAI models get correct metadata.

For OpenRouter and custom providers the model id is accepted verbatim and written to `config.toml` without validation against the adapter's advertised model list — `codex-acp` advertises codex-core's builtin OpenAI preset catalog regardless of the configured provider, so that list must not gate provider-native slugs like `deepseek/deepseek-v4-flash-0731`. The `openai` provider still validates against advertised models. During init the model list for these lanes comes from the provider's live catalog (the same listing that backs `GET /v1/models`), so the picker shows real provider slugs; when no catalog is available (custom provider or an offline fetch) init skips the list and asks for an explicit `--model` rather than showing the OpenAI presets.

Codex mode values are not fixed here: the `codex-acp` adapter advertises them at runtime and a mode is validated against that advertisement when it is set, so the accepted set follows whichever adapter version is installed (currently `read-only`, `agent`, `agent-full-access`). Reasoning-effort values follow the same rule: the adapter advertises them as the `reasoning_effort` (`thought_level`) session config option when the active model preset supports more than one effort. Modes and efforts are set through:

```sh
acps agent set --mode <mode>
acps agent set --effort <effort>
```

## Session Resume

`session/load`, `session/resume`, and `session/list` are discovered from the `codex-acp` adapter's `initialize` reply at runtime; `data/agents.toml` does not pin a value.

If the live ACP connection to `codex-acp` drops, `restart = "on-crash"` relaunches the supervised agent automatically. Any prompt that was mid-stream is flipped to `stalled` once the stale-prompt sweeper observes no further updates beyond `[prompts].stale_threshold`. Clients reconnect through `GET /v1/sessions/{id}/snapshot`, wait for the agent process to be running, then call `POST /v1/sessions/{id}/resume` when the new adapter advertises `sessionCapabilities.resume`. When `session/resume` is unsupported, the recovery path is a new `POST /v1/sessions`.
