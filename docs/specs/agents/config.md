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
acps subagent set --provider <provider-id> --model <model> [--api-key-ref <ref>]
acps subagent free
acps subagent disable
```

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

## Validation

- Provider edits require the configured agent to support provider selection.
- Model edits require the configured agent to support model selection.
- Mode edits require the configured agent to advertise mode choices.
- Mapped model and mode values are validated against ACP-advertised options.
- Custom-provider model ids are accepted as supplied.
- API-key refs must be valid secret-ref names and are added to `[agent].env`.
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
