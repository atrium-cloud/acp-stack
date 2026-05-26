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

For OpenRouter:

```sh
acps secrets set OPENROUTER_API_KEY
acps agent set --provider openrouter --model <model-id>
```

OpenRouter config is written to `~/.codex/config.toml` with the Responses API endpoint and `OPENROUTER_API_KEY` as the env reference.

Codex mode values (read-only, auto, full-access) are supported through:

```sh
acps agent set --mode <mode>
```
