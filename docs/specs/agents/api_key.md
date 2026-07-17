# Agent API Keys

`acp-stack` stores provider credentials in the encrypted secret store and injects them into agents only through configured env refs.

## Secret Uptake

How each harness reads provider credentials. `acp-stack` injects the listed env refs; the values are stored in the encrypted secret store.

| Agent       | Auth uptake                                                                            |
| ----------- | -------------------------------------------------------------------------------------- |
| OpenCode    | generated provider config referencing env refs                                         |
| Pi Agent    | provider env refs plus Pi model/provider settings                                      |
| Amp Code    | reads `AMP_API_KEY` from the environment                                               |
| Cursor CLI  | reads `CURSOR_API_KEY` from the environment                                            |
| Goose       | provider-native env vars plus Goose config                                             |
| Codex       | Codex-native OpenAI auth, or env refs for non-OpenAI mapped providers                  |
| Claude Code | provider env refs exposed through Claude settings, or native cloud provider credentials |
| Kimi Code   | stored as `KIMI_API_KEY`, translated to Kimi's process-only `KIMI_MODEL_*` contract      |

Codex requires a Responses-API-compatible upstream for any non-OpenAI provider. OpenRouter's OpenResponses (beta) endpoint is the mapped option `acps` supports today.

Claude Code custom providers require Anthropic Messages-compatible endpoints. Google Vertex and Amazon Bedrock use Claude Code's native cloud-provider auth flow; Microsoft Foundry uses Foundry-specific Claude env refs.

Kimi Code does not read `KIMI_API_KEY` directly. `acp-stack` keeps that canonical ref in encrypted storage and exposes the value to `kimi acp` as `KIMI_MODEL_API_KEY`, together with the selected model and the Kimi Code service endpoint.

## Provider Concept

Provider ids are `acps` metadata. They map an agent to the env refs it needs for a provider. The selected refs are added to `[agent].env` so the runtime can inject them when launching the agent.

## Rules

- Config stores secret ref names only.
- Secret values are set with `acps secrets set <name>`.
- Mapped provider ids must have a valid env-ref mapping for the configured agent.
- Custom providers must provide an explicit API-key ref.
- Agent-owned config files may reference env names, but must not contain secret values.

Cloudflare-style providers may require companion refs such as account id or gateway id. Those companion refs are handled the same way as API-key refs. For more details, refer to [data/env_vars.toml](../../../data/env_vars.toml) or agent harness docs > providers page.
