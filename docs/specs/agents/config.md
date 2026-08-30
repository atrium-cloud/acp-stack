# Agent Provider Config

Provider config describes which model backend the configured agent uses. It is separate from the ACP agent id.

The `acps agent provider`, `acps agent set`, `acps subagent`, and `acps agent switch` commands are documented in [cli-flags.md](../cli/cli-flags.md). Switch clears the configured model because model ids are agent-specific.

## Config Shape

```toml
[agent]
model = "<agent-model-id>"
mode = "<agent-mode>"
effort = "<agent-effort>"

[agent.config_options]
"<option-id>" = "<select-value>"
"<boolean-option-id>" = true

[agent.provider]
id = "<provider-id>"
model = "<agent-model-id>"

[agent.providers]
active = ["<provider-id>", "<provider-id>"]

[agent.providers.selected_aliases]
"<provider-id>" = "<credential-alias>"

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
```

Notes on the shape:

- `acps subagent set` inherits the main provider when omitted. It uses the main provider's selected structured credential or a compatible legacy `api_key_ref`.
- `acps subagent match` clears any explicit subagent provider/model so OpenCode `small_model` follows the main agent model.
- `acps subagent free` takes no flags. It routes to `openrouter/free` or `opencode/big-pickle` based on the configured main provider or env, and errors with "Current provider does not support free." otherwise.
- `[agent.provider]` remains the default provider/model lane. Without `[agent.providers]`, the implicit active set is that default provider plus any enabled subagent provider.
- A mapped provider may retain `api_key_ref` as legacy input. The first provider or credential mutation migrates it into the encrypted credential catalog. Custom providers keep their existing flat ref behavior.
- The first catalog credential for a provider is aliasless. Adding a second permanently promotes that provider to named, case-sensitive aliases and keeps each affected target on its existing key. Alias selection is manual and target-scoped; aliases do not provide automatic failover.

### `[agent.config_options]`

`[agent.config_options]` maps generic ACP session config-option ids to values: a string for select options, a TOML boolean for boolean options.

- Values apply on session creation, after the typed mode/model/effort settings.
- Entries the agent does not advertise — unknown id, off-list select value, kind mismatch — are reported through the ignored-features path. They are never a hard error.
- Ids the typed settings own (`mode`, `model`, `effort`, `reasoning_effort`) are rejected at validation with a pointer to the typed key.
- This is an id check only. An agent-specific id that happens to carry a typed category (e.g. kimi's `thinking` under `thought_level`) passes validation, applies after the typed setting, and wins. Prefer the typed key when one covers the option.
- A leading `_` is legal. ACP reserves `_`-prefixed ids for implementation-specific options.
- At most 32 entries.
- The map is cleared when the agent changes, like `mode`/`model`/`effort`.

## Validation

- Mapped provider edits use `agent provider use`; `agent set --provider` is reserved for custom providers.
- Multiple active providers are supported only by OpenCode and Pi and must include the default and enabled subagent providers.
- Active sets accept mapped providers supported by the harness; custom providers and duplicates are rejected.
- Model edits require the configured agent to support model selection.
- Mode edits require the configured agent to advertise mode choices.
- Root `agent.model` must be omitted when `[agent.provider].model` is set.
- Mapped model and mode values are validated against ACP-advertised options, except Claude Code provider-profile, Kimi Code, and Hermes Agent model ids are accepted as supplied. Kimi requires its model before ACP discovery can start; the hermes-agent-acp adapter advertises composite `provider/model` ids while `~/.hermes/config.yaml` pins the bare id.
- Custom-provider model ids are accepted as supplied.
- Custom-provider ids must not collide with the mapped-provider registry, including ids the registry maps only for other harnesses; a distinct id such as `anthropic-1` is required instead.
- One custom-provider id carries one `api_key_ref` instance-wide: every declaration of it across the primary agent, subagents, and Array targets must name the same ref, since the credential catalog stores one credential set per provider id.
- Custom providers use `chat-completions` by default, `responses` for Codex, and `anthropic-messages` for Claude Code.
- Credential aliases and source refs must be valid secret-ref identifiers.
- Switch does not migrate custom providers in place; configure the target provider explicitly.
- Agent-owned config provisioning must succeed before canonical config is updated.

## Agent Behavior

How each harness reads resolved credentials is covered under Secret Uptake in [api_key.md](api_key.md). This table covers only what the runtime provisions.

| Agent              | Provisioning behavior                                                                                                                                                                                                 |
| ------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| OpenCode           | every active provider and an exact `enabled_providers` allowlist are written to OpenCode JSON                                                                                                                         |
| Pi Agent           | only the default provider/model lane is written to Pi settings                                                                                                                                                        |
| Amp Code           | no provider selection; model selects the `amp-mode` execution tier (`low`/`medium`/`high`/`ultra`) and mode selects `default`/`bypass` permission behavior, both applied through ACP session config                   |
| Goose              | provider and model written to `~/.config/goose/config.yaml`, since Goose resolves the model while starting a session; the model is never taken from an ACP advertisement — it comes from the provider catalog where the provider publishes one and is named explicitly otherwise — and is also applied through ACP session config |
| Codex              | provider config written to `~/.codex/config.toml`; OpenRouter authenticates through a command-based `auth` block                                                                                                      |
| Claude Code        | Anthropic-compatible providers are written to Claude settings                                                                                                                                                         |
| Kimi Code          | provider + model setup                                                                                                                                                                                                |
| Hermes Agent       | the `model` block of `~/.hermes/config.yaml` carries the selected lane with the bare provider-native model id; an endpoint override rides the provider's native base-URL env var, or a managed `providers.acps-managed` entry (`custom:acps-managed`) where the overlay declares none |
| Kilo Code          | model and `build`/`plan` modes applied through ACP session config                                                                                                                                                     |
| Google Antigravity | no provider selection; model and mode selected from the ACP-advertised values                                                                                                                                         |

Some changes affect only new sessions or require the supervised agent process to restart. The CLI prints that restart guidance when applicable.
