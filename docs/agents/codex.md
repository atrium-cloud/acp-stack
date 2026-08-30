# Codex

Codex is adapter-backed. `acp-stack` launches `codex-acp`, which launches Codex.

Since Dec 10, 2025, Codex CLI [supports Responses API-compatible endpoints only](https://github.com/openai/codex/discussions/7782). Every provider other than OpenAI must be Responses API-compatible.

## Setup

```sh
acps init --agent codex
```

Codex supports two mapped provider ids.

### OpenAI

For Codex's built-in OpenAI provider:

```sh
acps agent provider credential add openai
acps agent provider use openai --model <model-id>
```

- `credential add` prompts for the key.
- To reuse a key already in the store, run `acps secrets set OPENAI_API_KEY` first and pass `--from-secret OPENAI_API_KEY=OPENAI_API_KEY`.
- Switching from a generated non-OpenAI provider to `openai` backs up the prior `~/.codex/config.toml` with a numeric suffix.

### OpenRouter

For [OpenRouter](https://openrouter.ai/docs/cookbook/coding-agents/codex-cli):

```sh
acps secrets set OPENROUTER_API_KEY
acps agent provider use openrouter --model <model-id>
```

- OpenRouter config is written to `~/.codex/config.toml` with `https://openrouter.ai/api/v1` as the Responses base URL.
- Following the OpenRouter cookbook, authentication uses a command-based `auth` block that echoes `OPENROUTER_API_KEY` instead of a plain `env_key`. This lets Codex refresh its model catalog, so non-OpenAI models get correct metadata.

### Model ids on OpenRouter and custom providers

- The model id is accepted verbatim and written to `config.toml`. The adapter's advertised list covers only codex-core's builtin OpenAI presets, so provider-native slugs like `deepseek/deepseek-v4-flash-0731` bypass it.
- The `openai` provider validates against advertised models.
- During init, the model list for these lanes comes from the provider's live catalog (the same listing that backs `GET /v1/models`), so the picker shows real provider slugs.
- When no catalog is available (custom provider or an offline fetch), init skips the list and asks for an explicit `--model` instead of showing the OpenAI presets.

### Modes and reasoning effort

- Codex mode values are not fixed here. The `codex-acp` adapter advertises them at runtime, and a mode is validated against that advertisement when you set it. The accepted set follows whichever adapter version is installed (currently `read-only`, `agent`, `agent-full-access`).
- Reasoning-effort values follow the same rule. The adapter advertises them as the `reasoning_effort` (`thought_level`) session config option when the active model preset supports more than one effort.
- With OpenRouter the adapter advertises no effort option, so acps validates `--effort` against the OpenRouter catalog's reasoning-effort values for the configured model (`max` excluded, since Codex has no such level) and pins it as `model_reasoning_effort` in `~/.codex/config.toml`. A restart applies it.

Set modes and efforts through:

```sh
acps agent set --mode <mode>
acps agent set --effort <effort>
```

## Session Resume

`session/load`, `session/resume`, and `session/list` are discovered from the `codex-acp` adapter's `initialize` reply at runtime; `data/agents.toml` omits the value. For crash recovery and the generic resume flow, see `docs/specs/acp/acp-bridge.md` ("Session Resume Capability Matrix") and `docs/specs/runtime.md`.
