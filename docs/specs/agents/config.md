# Agent Provider Config

Last updated: May 25, 2026

## Concept

`provider` is the `acps` concept for the model-provider backend an agent should use. It is separate from the ACP agent itself. For example, `opencode` is the agent id, while `opencode-go`, `openai`, or `anthropic` can be provider ids configured for that agent.

Provider ids are resolved against the provider/env mapping. Model ids are resolved from the agent's ACP `session/new` config options: the agent advertises a `model` option and `acps` accepts only values from that option.

## CLI

Provider config is changed with:

```sh
acps agent set --provider <provider-id> [--model <model>] [--api-key-ref <ref>]
acps agent set --custom-provider --provider <provider-id> --provider-name <display-name> --base-url <url> --api-key-ref <ref> --model <model-id> [--provider-api <chat-completions|responses>] [--model-name <display-name>] [--context <tokens>] [--output-max-tokens <tokens>]
```

Auxiliary/subagent model config is changed with:

```sh
acps subagent set --provider <provider-id> --model <model> [--api-key-ref <ref>]
acps subagent set --custom-provider --provider <provider-id> --provider-name <display-name> --base-url <url> --api-key-ref <ref> --model <model-id> [--provider-api <chat-completions|responses>] [--model-name <display-name>] [--context <tokens>] [--output-max-tokens <tokens>]
acps subagent free [--provider <openrouter|opencode>] [--api-key-ref <ref>]
acps subagent disable
```

Model-only config is changed with:

```sh
acps agent set --model <model>
```

Behavior:

- updates `[agent.provider]` for provider-backed agents
- updates `[agent.subagent.provider]` only for OpenCode `small_model`
- updates `[agent].model` for model-only agents
- rejects provider edits unless the embedded registry entry declares `set_provider = true`
- rejects model edits unless the embedded registry entry declares `set_model = true`
- adds the selected API-key ref and provider companion env refs to `[agent].env` when missing
- rejects mapped provider ids that have no API-key env mapping for the configured agent unless `--custom-provider` is used
- regenerates supported agent-owned config files before writing the main config
- writes canonical TOML atomically only after generated config provisioning succeeds
- does not store provider API-key values in plaintext config

When `--api-key-ref` is omitted, `acps` uses the default key ref from [api_key.md](api_key.md). Providers without an API-key env mapping for the configured agent require custom provider setup.

When `--model` is omitted, interactive terminals start a provisional ACP session and prompt from the advertised `model` config option. Non-interactive invocations print the advertised model values and exit without mutating config.

Custom provider setup writes config only; it does not certify that the underlying agent can talk to the endpoint. OpenCode, Pi, and Goose default custom providers to `chat-completions`. Codex defaults custom providers to `responses` and rejects `chat-completions` because Codex-oriented OpenAI models are Responses-only in this support contract. Custom token limits default to `context = 200000` and `output_max_tokens = 65536`; override values must be plain positive integers without commas.

`acps subagent *` is OpenCode-only. It exists because OpenCode can make implicit `small_model` calls; other current harnesses do not expose an equivalent config surface through `acps`.

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

[agent.provider.custom]
name = "<provider-display-name>"
base_url = "https://api.example.com/v1"
api = "chat-completions"
model_name = "<model-display-name>"
context = 200000
output_max_tokens = 65536

[agent.subagent.provider]
id = "<provider-id>"
model = "<agent-model-id>"
api_key_ref = "<provider-api-key-ref>"

[agent.subagent.provider.custom]
name = "<provider-display-name>"
base_url = "https://api.example.com/v1"
api = "chat-completions"
model_name = "<model-display-name>"
context = 200000
output_max_tokens = 65536
```

Fields:

- `[agent].model`: exact agent-advertised model id for model-only agents such as Cursor CLI.
- `[agent.provider].id`: provider id listed for the configured agent in the provider metadata.
- `[agent.provider].model`: exact agent-advertised model id for mapped provider-backed agents, or the operator-provided custom model id for custom providers. Goose uses provider-native model ids, while OpenCode and Pi commonly use `<provider-id>/<model-id>`.
- `[agent.provider].api_key_ref`: secret ref that should be present in `[agent].env` and referenced by generated agent-owned config.
- `[agent.provider.custom]`: optional custom provider metadata. When present, `model` and `api_key_ref` are required.
- `[agent.subagent.provider]`: OpenCode `small_model` provider metadata with the same shape as `[agent.provider]`.
- `[agent.subagent.provider.custom]`: optional custom provider metadata for OpenCode `small_model`. When present, `model` and `api_key_ref` are required.

## Accepted Provider IDs And Model Formats

Mapped provider ids must be listed for the configured agent in the provider metadata. Custom provider ids are operator-defined for agents that declare custom-provider support. The current CLI accepts non-empty model ids without surrounding whitespace, validates mapped model ids against ACP session config options, and uses [api_key.md](api_key.md) to choose the default API-key ref.

Model values are agent-specific:

- Goose: exact provider-native model ids for the selected provider.
- OpenCode: exact provider-qualified model ids in `<provider-id>/<model-id>` form.
- Pi: exact ids or provider-qualified model patterns accepted by Pi model scoping.
- Codex: exact ACP-advertised model ids for `openai` or `openrouter`.
- Amp Code: no raw provider/model value accepted through `acps`.
- Cursor CLI: exact ACP-advertised model values. Operators can pass a shorthand such as `gpt-5.5`; `acps` stores the exact advertised Cursor model value.

## Supported Agents

Goose:

- Provider ids must be listed for Goose in the provider metadata.
- Model ids should be provider-native for the selected provider.
- `acps` writes `~/.config/goose/config.yaml` with `GOOSE_PROVIDER`, `GOOSE_MODE = auto`, `GOOSE_CONTEXT_STRATEGY = summarize`, and `GOOSE_DISABLE_SESSION_NAMING = true` after provider selection. Custom providers also write a provider descriptor under `~/.config/goose/custom_providers/`.
- `acps` stores Goose's selected model in `[agent.provider].model` and applies it through ACP `session/set_config_option` with `configId = "model"` after `session/new` and before the first prompt.
- Goose consumes provider-native API-key env vars directly. `acps agent set --provider` therefore requires the selected `api_key_ref` to match the default env var from [api_key.md](api_key.md).
- The current real ACP probe did not advertise Goose mode values.

OpenCode:

- Provider ids must be listed for OpenCode in the provider metadata.
- Model ids should be provider-qualified.
- Mode values are validated against OpenCode's ACP-advertised `mode` option. The current real ACP probe returned `build` and `plan`.
- `acps` writes `model`, `small_model`, `enabled_providers`, and provider API-key config to `~/.config/opencode/opencode.json`. Without `[agent.subagent.provider]`, `small_model` inherits `model`.
- `enabled_providers` is generated from the union of the configured main and subagent OpenCode provider ids.
- `acps subagent disable` writes `small_model = "invalid/model"`; `small_model = ""` still triggers OpenCode's implicit fallback.
- OpenCode can use whatever key the configured provider block references; the provider block must match the chosen provider and key.
- `cloudflare-ai-gateway` defaults to `CLOUDFLARE_API_TOKEN` for OpenCode because that is the OpenCode/models.dev auth contract. `cloudflare-workers-ai` defaults to `CLOUDFLARE_API_KEY`.

Pi:

- Provider is part of the model string.
- Provider ids must be listed for Pi in the provider metadata.
- Model ids should use the form accepted by Pi for the selected provider.
- `acps agent set` writes `~/.pi/agent/settings.json` `enabledModels` from the explicit configured model. Custom providers also write `~/.pi/agent/models.json` with provider base URL, API family, API-key ref, and model limits.
- Pi Cloudflare provider setup follows Pi's provider docs: `cloudflare-workers-ai` requires `CLOUDFLARE_API_KEY` plus `CLOUDFLARE_ACCOUNT_ID`; `cloudflare-ai-gateway` also requires `CLOUDFLARE_GATEWAY_ID`.

Codex:

- Mapped provider ids are limited to `openai` and `openrouter`; custom providers are supported only with `api = "responses"`.
- `acps agent set --provider openai --model <model-id>` validates the model through Codex ACP session config, writes `[agent.provider]` without `api_key_ref`, and keeps OpenAI auth Codex-native.
- When switching Codex to `openai`, `acps` writes `~/.codex/config.toml` with `model` and `model_provider = "openai"`. If the previous canonical Codex config selected a generated non-OpenAI provider, `acps` first backs up the file as `config.<provider>.toml` or the next `-1`, `-2` suffix, then removes that provider table from the canonical file.
- `acps agent set --provider openrouter --model <model-id>` defaults `api_key_ref` to `OPENROUTER_API_KEY`, validates the model through Codex ACP session config, and writes `~/.codex/config.toml` with `model_provider = "openrouter"` and `model_providers.openrouter` using `base_url = "https://openrouter.ai/api/v1/responses"`, `env_key = "OPENROUTER_API_KEY"`, and `wire_api = "responses"`.
- Mode values are validated against Codex's ACP-advertised `mode` option. The current real ACP probe returned `read-only`, `auto`, and `full-access`.
- Codex mapped providers other than `openai` and `openrouter` are rejected. Custom Codex providers write `model_providers.<id>` with `base_url`, `env_key`, and `wire_api = "responses"`.

Amp Code:

- Provider/model config through `acps agent set` is not supported.
- Mode values are validated against Amp's ACP-advertised `mode` option. The current real ACP probe (`amp-acp v0.1.1`, 2026-05-20) returned `smart`, `rush`, and `deep`.

Cursor CLI:

- Provider ids are not configured for Cursor in the provider metadata; Cursor model values are selected with `acps agent set --model <model-id>`.
- Model ids are validated against Cursor's ACP-advertised model list. If the operator passes a shorthand model id, `acps` persists the exact advertised model value in `[agent].model`.
- Mode values are validated against Cursor's ACP-advertised `mode` option. The current real ACP probe returned `agent`, `ask`, and `plan`.
- Cursor consumes `CURSOR_API_KEY` directly from process env; no generated agent-owned config file is required.

## Validation

Current validation requires mapped provider ids to be listed for the configured agent in the provider metadata, custom provider metadata to include model and API-key ref, syntactically non-empty model/API-key-ref values, and API-key refs to be valid secret-ref names. Mapped model and mode values are validated against ACP-advertised options before writing config.

## Agent-owned Config Lifecycle

Every supported agent reads its own configuration from a well-known path under the operator's home. `acps` writes those files during `acps init` (initial agent selection) and `acps agent set` (subsequent provider/model/mode edits), and reads them indirectly when the agent process starts. The table below pins the write path, the read trigger, and whether a live edit requires the agent to be relaunched. Operators planning a model swap during an active session should consult the right column before running `acps agent set`.

| Agent | Generated config file | Written by | Read by agent at | Live model swap | Live mode swap | Relaunch on edit? |
| ----- | --------------------- | ---------- | ---------------- | --------------- | -------------- | ----------------- |
| Goose | `~/.config/goose/config.yaml` (+ `~/.config/goose/custom_providers/<id>.yaml` for custom providers) | `acps init`, `acps agent set --provider`, `acps agent set --model`, `acps agent set --custom-provider` | process start | ACP `session/set_config_option` with `configId = "model"` on the existing session — no relaunch | not advertised | no for model; n/a for mode |
| OpenCode | `~/.config/opencode/opencode.json` | `acps init`, `acps agent set --provider`, `acps agent set --model`, `acps agent set --custom-provider`, `acps subagent set` | process start | new session only — current sessions keep the previous model | new session only | yes for model, subagent model, and mode |
| Pi | `~/.pi/agent/settings.json` (+ `~/.pi/agent/models.json` for custom providers) | `acps init`, `acps agent set --provider`, `acps agent set --model`, `acps agent set --custom-provider` | process start | new session only | not advertised | yes for model |
| Codex | `~/.codex/config.toml` (provider switches back up the previous file as `config.<provider>.toml`) | `acps init`, `acps agent set --provider openai`, `acps agent set --provider openrouter`, `acps agent set --custom-provider` | process start | new session only | ACP `session/set_config_option` with `configId = "mode"` on the existing session — no relaunch | yes for model; no for mode |
| Amp Code | none (Amp reads `AMP_API_KEY` from process env) | n/a | n/a | not supported through `acps` | ACP `session/set_config_option` with `configId = "mode"` on the existing session — no relaunch | n/a for model; no for mode |
| Cursor CLI | none (Cursor reads `CURSOR_API_KEY` from process env) | n/a | n/a | new session only via ACP `session/set_model` | new session only | yes for model and mode |

Operational rules that follow from the table:

- `acps agent set` regenerates the relevant config file BEFORE it writes the canonical `acp-stack.toml`. A generated-config write failure aborts the canonical write so the on-disk pair stays consistent.
- `acps agent set` does not currently relaunch the agent process. When the table says "yes for model/mode", operators must restart the supervised agent through `POST /v1/agent/restart` so the daemon re-reads the on-disk config before starting the next supervised process. The CLI prints an explicit restart hint in that case.
- Live changes that the table marks as supported are only valid against existing ACP sessions that have not been closed; they do not retroactively apply to historical session prompts. Unsupported live changes are rejected by the relevant route handler with an explicit "<agent> does not support live <model|mode> changes" error rather than silently accepted.
- Custom-provider edits never apply live: they always require the agent to restart so it can re-read the regenerated config file and pick up the new base URL / model. The CLI emits the same restart hint.

## Out-of-scope Agent Setup

`acps init` and `acps agent set` deliberately do not touch:

- Agent plugins or extensions distributed outside the embedded registry. Each supported agent's plugin manager is operator-managed; `acps` does not write plugin manifests or install plugin packages.
- Agent skill catalogs, prompt libraries, or other user-extensible content. The embedded registry pins the binary surface; skill curation lives in the operator's home.
- Pre-prompt or post-prompt hooks, including agent-side automation triggers. `acps` exposes its own permission and command pipelines instead, which give operators the same observability without the agent having to learn a foreign hook protocol.

A future verified non-interactive setup path may lift one of these, but until that exists per supported agent, `acps agent set --plugin`, `--skill`, `--hook` and similar flags are not implemented and the CLI returns "unknown argument" rather than silently writing partial config. Operators who need any of the above should configure them directly through the upstream agent's own tooling and accept that `acps` does not include those artifacts in `acps config export`.
