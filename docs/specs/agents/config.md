# Agent Provider Config

Provider config describes which model backend the configured agent should use. It is separate from the ACP agent id.

## CLI

```sh
acps agent set --provider <provider-id> [--model <model>] [--api-key-ref <ref>]
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

`acps subagent set` inherits `--provider` (and the matching `--api-key-ref`) from the main agent provider when omitted. `acps subagent free` takes no flags; it routes to `openrouter/free` or `opencode/big-pickle` based on the configured main provider or env, and errors with "Current provider does not support free." otherwise.

`acps agent switch <agent>` rewrites the harness and provider lane through the admin API. It clears any existing model because model ids are agent-specific. After a switch, `acps agent set --model <model-id>` applies the model to the existing provider-backed config. Switch copies installed Agent Skills to the target skills directory when the source and target paths differ. By default, source harness config is preserved; `--drop` removes source agent-owned config only after the target switch succeeds.

## Config Shape

```toml
[agent]
model = "<agent-model-id>"
mode = "<agent-mode>"

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
```

`acps subagent match` clears any explicit subagent provider/model so OpenCode `small_model` follows the main agent model.

## Validation

- Provider edits require the configured agent to support provider selection.
- Model edits require the configured agent to support model selection.
- Mode edits require the configured agent to advertise mode choices.
- Mapped model and mode values are validated against ACP-advertised options.
- Custom-provider model ids are accepted as supplied.
- API-key refs must be valid secret-ref names and are added to `[agent].env`.
- Switch does not migrate custom providers in place; configure the target provider explicitly.
- Agent-owned config provisioning must succeed before canonical config is
  updated.

## Agent Behavior

| Agent      | Provider/model behavior                                                 |
| ---------- | ----------------------------------------------------------------------- |
| OpenCode   | provider and `small_model` config in OpenCode JSON                      |
| Pi Agent   | provider is part of the model choice; model written to Pi settings      |
| Amp Code   | mode selection only                                                     |
| Cursor CLI | model and mode selection only                                           |
| Goose      | provider-native env vars; model applied through ACP session config      |
| Codex      | `openai` uses Codex-native auth; `openrouter` uses `OPENROUTER_API_KEY` |

Some changes affect only new sessions or require the supervised agent process to restart. The CLI prints that restart guidance when applicable.
