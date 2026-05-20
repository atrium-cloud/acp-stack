# Agent Provider Config

Last updated: May 19, 2026

## Concept

`provider` is the `acps` concept for the model-provider backend an agent should use. It is separate from the ACP agent itself. For example, `opencode` is the agent id, while `opencode-go`, `openai`, or `anthropic` can be provider ids configured for that agent.

Provider ids are resolved against `data/mapping.toml`. Model ids are resolved from the agent's ACP `session/new` config options: the agent advertises a `model` option and `acps` accepts only values from that option.

## CLI

Provider config is changed with:

```sh
acps agent set --provider <provider-id> [--model <model>] [--api-key-ref <ref>]
```

Model-only config is changed with:

```sh
acps agent set --model <model>
```

Behavior:

- updates `[agent.provider]` for provider-backed agents
- updates `[agent].model` for model-only agents
- rejects provider edits unless the embedded registry entry declares `set_provider = true`
- rejects model edits unless the embedded registry entry declares `set_model = true`
- adds the selected API-key ref and provider companion env refs to `[agent].env` when missing
- rejects provider ids that have no API-key env mapping for the configured agent
- regenerates supported agent-owned config files before writing the main config
- writes canonical TOML atomically only after generated config provisioning succeeds
- does not store provider API-key values in plaintext config

When `--api-key-ref` is omitted, `acps` uses the default key ref from [api_key.md](api_key.md). Providers without an API-key env mapping for the configured agent are rejected by generated provider config paths.

When `--model` is omitted, interactive terminals start a provisional ACP session and prompt from the advertised `model` config option. Non-interactive invocations print the advertised model values and exit without mutating config.

Mode config is changed with:

```sh
acps agent set --mode <mode>
```

Mode values are validated against the agent's ACP `mode` config option. The command is available only when the embedded registry entry declares `set_mode = true`.

## Config Shape

```toml
[agent]
model = "<agent-model-id>"

[agent.provider]
id = "<provider-id>"
model = "<agent-model-id>"
api_key_ref = "<provider-api-key-ref>"
```

Fields:

- `[agent].model`: exact agent-advertised model id for model-only agents such as Cursor CLI.
- `[agent.provider].id`: provider id listed for the configured agent in `data/mapping.toml`.
- `[agent.provider].model`: exact agent-advertised model id for provider-backed agents. Goose uses provider-native model ids, while OpenCode and Pi commonly use `<provider-id>/<model-id>`.
- `[agent.provider].api_key_ref`: secret ref that should be present in `[agent].env` and referenced by generated agent-owned config.

## Accepted Provider IDs And Model Formats

Provider ids must be listed for the configured agent in `data/mapping.toml`. The current CLI accepts non-empty model ids without surrounding whitespace, validates model ids against ACP session config options, and uses [api_key.md](api_key.md) to choose the default API-key ref.

Model values are agent-specific:

- Goose: exact provider-native model ids for the selected provider.
- OpenCode: exact provider-qualified model ids in `<provider-id>/<model-id>` form.
- Pi: exact ids or provider-qualified model patterns accepted by Pi model scoping.
- Codex: exact ACP-advertised model ids for `openai` or `openrouter`.
- Amp Code: no raw provider/model value accepted through `acps`.
- Cursor CLI: exact ACP-advertised model values. Operators can pass a shorthand such as `gpt-5.5`; `acps` stores the exact advertised Cursor model value.

## Supported Agents

Goose:

- Provider ids must be listed for Goose in `data/mapping.toml`.
- Model ids should be provider-native for the selected provider.
- `acps` writes `~/.config/goose/config.yaml` with `GOOSE_PROVIDER`, `GOOSE_MODE = auto`, `GOOSE_CONTEXT_STRATEGY = summarize`, and `GOOSE_DISABLE_SESSION_NAMING = true` after provider selection.
- `acps` stores Goose's selected model in `[agent.provider].model` and applies it through ACP `session/set_config_option` with `configId = "model"` after `session/new` and before the first prompt.
- Goose consumes provider-native API-key env vars directly. `acps agent set --provider` therefore requires the selected `api_key_ref` to match the default env var from [api_key.md](api_key.md).
- The current real ACP probe did not advertise Goose mode values.

OpenCode:

- Provider ids must be listed for OpenCode in `data/mapping.toml`.
- Model ids should be provider-qualified.
- Mode values are validated against OpenCode's ACP-advertised `mode` option. The current real ACP probe returned `build` and `plan`.
- `acps` writes `~/.config/opencode/opencode.json` with `model` set to the selected model and `provider.<id>.options.apiKey` set to `{env:<api_key_ref>}`.
- OpenCode can use whatever key the configured provider block references; the provider block must match the chosen provider and key.
- `cloudflare-ai-gateway` defaults to `CLOUDFLARE_API_TOKEN` for OpenCode because that is the OpenCode/models.dev auth contract. `cloudflare-workers-ai` defaults to `CLOUDFLARE_API_KEY`.

Pi:

- Provider is part of the model string.
- Provider ids must be listed for Pi in `data/mapping.toml`.
- Model ids should use the form accepted by Pi for the selected provider.
- `acps agent set` writes `~/.pi/agent/settings.json` `enabledModels` from the explicit configured model.
- Pi Cloudflare provider setup follows Pi's provider docs: `cloudflare-workers-ai` requires `CLOUDFLARE_API_KEY` plus `CLOUDFLARE_ACCOUNT_ID`; `cloudflare-ai-gateway` also requires `CLOUDFLARE_GATEWAY_ID`.

Codex:

- Provider ids are limited to `openai` and `openrouter`.
- `acps agent set --provider openai --model <model-id>` validates the model through Codex ACP session config, writes `[agent.provider]` without `api_key_ref`, and keeps OpenAI auth Codex-native.
- When switching Codex to `openai`, `acps` writes `~/.codex/config.toml` with `model` and `model_provider = "openai"`. If the previous canonical Codex config selected a generated non-OpenAI provider, `acps` first backs up the file as `config.<provider>.toml` or the next `-1`, `-2` suffix, then removes that provider table from the canonical file.
- `acps agent set --provider openrouter --model <model-id>` defaults `api_key_ref` to `OPENROUTER_API_KEY`, validates the model through Codex ACP session config, and writes `~/.codex/config.toml` with `model_provider = "openrouter"` and `model_providers.openrouter` using `base_url = "https://openrouter.ai/api/v1/responses"`, `env_key = "OPENROUTER_API_KEY"`, and `wire_api = "responses"`.
- Mode values are validated against Codex's ACP-advertised `mode` option. The current real ACP probe returned `read-only`, `auto`, and `full-access`.
- Codex providers other than `openai` and `openrouter` are rejected.

Amp Code:

- Provider/model config through `acps agent set` is not supported.
- Mode values are validated against Amp's ACP-advertised `mode` option. The current real ACP probe (`amp-acp v0.1.1`, 2026-05-20) returned `smart`, `rush`, and `deep`.

Cursor CLI:

- Provider ids are not configured for Cursor in `data/mapping.toml`; Cursor model values are selected with `acps agent set --model <model-id>`.
- Model ids are validated against Cursor's ACP-advertised model list. If the operator passes a shorthand model id, `acps` persists the exact advertised model value in `[agent].model`.
- Mode values are validated against Cursor's ACP-advertised `mode` option. The current real ACP probe returned `agent`, `ask`, and `plan`.
- Cursor consumes `CURSOR_API_KEY` directly from process env; no generated agent-owned config file is required.

## Validation

Current validation requires the provider id to be listed for the configured agent in `data/mapping.toml`, the model and API-key ref to be syntactically non-empty, and API-key refs to be valid secret-ref names. Model and mode values are validated against ACP-advertised options before writing config.
