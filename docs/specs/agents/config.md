# Agent Provider Config

Provider config describes which model backend the configured agent should use. It is separate from the ACP agent id.

## CLI

```sh
acps agent provider use <provider-id> [--model <model>]
acps agent provider set-active <provider-id,provider-id,...>
acps agent provider list-active
acps agent provider credential add <provider-id>
acps agent provider credential update <provider-id> [alias]
acps agent provider credential select <provider-id> <alias>
acps agent provider credential list [provider-id]
acps agent provider credential delete <provider-id> [alias]
acps agent set --custom-provider --provider <id> --provider-name <name> --base-url <url> --api-key-ref <ref> --model <model-id>
acps agent set --model <model>
acps agent set --mode <mode>
```

OpenCode also supports:

```sh
acps subagent set --model <model> [--provider <provider-id>] [--api-key-ref <ref>]
acps subagent match
acps subagent free
acps subagent disable
```

`acps subagent set` inherits the main provider when omitted and uses its selected structured credential or compatible legacy `api_key_ref`. `acps subagent free` takes no flags; it routes to `openrouter/free` or `opencode/big-pickle` based on the configured main provider or env, and errors with "Current provider does not support free." otherwise.

`acps agent switch <agent>` rewrites the harness and provider lane through the admin API. It clears any existing model because model ids are agent-specific. After a switch, `acps agent set --model <model-id>` applies the model to the existing provider-backed config. Switch copies installed Agent Skills to the target skills directory when the source and target paths differ. By default, source harness config is preserved; `--drop` removes source agent-owned config only after the target switch succeeds.

## Config Shape

```toml
[agent]
model = "<agent-model-id>"
mode = "<agent-mode>"

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

`acps subagent match` clears any explicit subagent provider/model so OpenCode `small_model` follows the main agent model.

`[agent.provider]` remains the default provider/model lane. Without `[agent.providers]`, the implicit active set is that default provider plus any enabled subagent provider. A mapped provider may retain `api_key_ref` as legacy input; the first provider or credential mutation migrates it into the encrypted credential catalog. Custom providers keep their existing flat ref behavior.

The first catalog credential for a provider is aliasless. Adding a second permanently promotes that provider to named, case-sensitive aliases and keeps each affected target on its existing key. Alias selection is manual and target-scoped; aliases do not provide automatic failover.

## Validation

- Mapped provider edits use `agent provider use`; `agent set --provider` is reserved for custom providers.
- Multiple active providers are supported only by OpenCode and Pi and must include the default and enabled subagent providers.
- Active sets accept mapped providers supported by the harness; custom providers and duplicates are rejected.
- Model edits require the configured agent to support model selection.
- Mode edits require the configured agent to advertise mode choices.
- Root `agent.model` must be omitted when `[agent.provider].model` is set.
- Mapped model and mode values are validated against ACP-advertised options, except Claude Code provider-profile and Kimi Code model ids are accepted as supplied. Kimi requires its model before ACP discovery can start.
- Custom-provider model ids are accepted as supplied.
- Custom providers use `chat-completions` by default, `responses` for Codex, and `anthropic-messages` for Claude Code.
- Credential aliases and source refs must be valid secret-ref identifiers.
- Switch does not migrate custom providers in place; configure the target provider explicitly.
- Agent-owned config provisioning must succeed before canonical config is updated.

## Agent Behavior

| Agent       | Provider/model behavior                                                                  |
| ----------- | ---------------------------------------------------------------------------------------- |
| OpenCode    | every active provider and an exact `enabled_providers` allowlist are written to OpenCode JSON |
| Pi Agent    | every active provider env bundle is injected; only the default provider/model lane is written to Pi settings |
| Amp Code    | mode selection only                                                                      |
| Cursor CLI  | model and mode selection only                                                            |
| Goose       | provider-native env vars; model applied through ACP session config                       |
| Codex       | `openai` uses Codex-native auth; `openrouter` uses `OPENROUTER_API_KEY`                  |
| Claude Code | Anthropic-compatible providers are written to Claude settings with provider-specific refs |
| Kimi Code   | model-only setup; runtime derives Kimi's process environment from `KIMI_API_KEY`        |

Some changes affect only new sessions or require the supervised agent process to restart. The CLI prints that restart guidance when applicable.
