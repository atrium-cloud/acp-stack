# Agent API Keys

`acp-stack` stores provider credentials in the encrypted secret store. Mapped providers resolve from the provider credential catalog. Custom providers and legacy configs keep their configured flat refs.

## Secret Uptake

Each harness reads resolved credentials its own way. Env var names and base companion vars per provider live in [data/env_vars.toml](../../../data/env_vars.toml); provider ids and per-provider overrides live in [data/providers.toml](../../../data/providers.toml).

| Agent              | Auth uptake                                                                                  |
| ------------------ | -------------------------------------------------------------------------------------------- |
| OpenCode           | generated provider config referencing env refs                                               |
| Pi Agent           | provider env refs plus Pi model/provider settings                                            |
| Amp Code           | reads its agent-scoped key from the environment                                              |
| Goose              | provider-native env vars plus Goose config                                                   |
| Codex              | env refs for every mapped provider                                                           |
| Claude Code        | provider env refs exposed through Claude settings, or native cloud-provider credentials      |
| Kimi Code          | the active lane's key, translated to Kimi's process-only `KIMI_MODEL_*` contract (see below) |
| Hermes Agent       | provider-native env refs, with the model lane written to `~/.hermes/config.yaml` (see below) |
| Kilo Code          | reads `KILO_API_KEY` or provider-native env vars from the environment (see below)            |
| Google Antigravity | reads `GEMINI_API_KEY` from the environment, paired with a provisioned auth type (see below) |

### Codex

- Codex requires a Responses-API-compatible upstream for any non-OpenAI provider.
- OpenRouter's OpenResponses (beta) endpoint is the mapped option `acps` supports today.

### Claude Code

- Custom providers require Anthropic Messages-compatible endpoints.
- Google Vertex and Amazon Bedrock use Claude Code's native cloud-provider auth flow.
- Microsoft Foundry uses Foundry-specific Claude env refs.

### Kimi Code

- Kimi Code does not read `KIMI_API_KEY` directly.
- `acp-stack` keeps the active lane's ref in encrypted storage:
    - `KIMI_API_KEY` for the Kimi For Coding subscription
    - `MOONSHOT_API_KEY` for the Moonshot platform
    - the declared `api_key_ref` for a custom provider
- `acp-stack` exposes the value to `kimi acp` as `KIMI_MODEL_API_KEY`, together with the selected model and the lane's endpoint.

### Hermes Agent

- Hermes maps only API-key providers.
- Headless provisioning writes the non-secret `model` block of `~/.hermes/config.yaml`.
- The key itself reaches the process only through `[agent].env`.
- A credential endpoint override on a mapped provider whose Hermes overlay declares a base-URL env var is delivered through that variable in the launch environment; `model.provider` keeps the provider-native id.
- The remaining endpoint-carrying configurations (custom providers, and mapped providers without such a variable) are provisioned as a managed named `providers.acps-managed` entry with `key_env` and `transport`, referenced as `model.provider: custom:acps-managed` — never as `model.base_url`.

### Kilo Code

- Kilo reads `KILO_API_KEY` or provider-native env vars from the environment.
- The harness requires `KILO_API_KEY` present even when a provider-native credential is declared.
- Init, `config import`, and `agent set --model` record an empty placeholder automatically in that case.

### Google Antigravity

- Antigravity reads `GEMINI_API_KEY` (Google AI Studio) from the environment.
- Headless provisioning writes `auth.type: "gemini-api-key"` into `~/.gemini/antigravity-acp/settings.json`.
- Both are required for API-key mode.

## Provider Concept

Provider ids are `acps` metadata. They map an agent to the env names it needs for a provider. The shared resolver combines generic `[agent].env` refs with the selected catalog bundles before launch.

## Rules

- Config stores secret ref names only.
- Add, rotate, select, list, and delete mapped credentials with `acps agent provider credential`.
- Scripts may copy values already stored by `acps secrets set <name>` with repeatable `--from-secret ENV=REF`.
- Mapped provider ids must have a valid env-ref mapping for the configured agent.
- OpenCode and Pi may activate multiple mapped providers; a shared env name must resolve to the same value.
- Custom providers must provide an explicit API-key ref.
- Agent-owned config files may reference env names, but must not contain secret values.

Cloudflare-style providers may require companion refs such as account id or gateway id. `acp-stack` handles companion refs the same way as API-key refs. The base `companion_env_vars` list per provider lives in [data/env_vars.toml](../../../data/env_vars.toml), with per-provider overrides in [data/providers.toml](../../../data/providers.toml).
