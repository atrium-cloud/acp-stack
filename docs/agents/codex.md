# Codex

Codex is adapter-backed. `acp-stack` launches `codex-acp`, which launches Codex.

## Setup

```sh
acps init --agent codex
```

Codex supports two provider ids through `acps`.

For Codex's built-in OpenAI provider:

```sh
acps agent set --provider openai --model <model-id>
```

OpenAI auth remains Codex-native. `acps` does not collect or write an OpenAI API-key ref for Codex. If `~/.codex/config.toml` currently selects a generated non-OpenAI provider such as OpenRouter, switching to `openai` backs up the current file as `config.<provider>.toml` with a numeric suffix when needed, then removes that provider table from the canonical config.

For OpenRouter:

```sh
acps secrets set OPENROUTER_API_KEY
acps agent set --provider openrouter --model <model-id>
```

OpenRouter writes `~/.codex/config.toml` with `model_provider = "openrouter"` and a `model_providers.openrouter` table that uses the OpenRouter Responses endpoint and `OPENROUTER_API_KEY`.

Codex advertises ACP mode values for approval and sandbox presets. The current real ACP probe returned `read-only`, `auto`, and `full-access`; `acps agent set --mode <mode>` validates against that list.

## Unsupported

- Managing Codex browser OAuth/account-cookie flows.
- Storing Codex auth tokens in `acp-stack`.
