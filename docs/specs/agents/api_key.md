# Agent API Keys

Last updated: May 17, 2026

## Secret Uptake

`acp-stack` has one runtime secret-delivery mechanism: values listed in `[agent].env` are resolved from the encrypted secret store and injected into the agent child process. Agent-specific setup determines whether the child process consumes those variables automatically or through generated config.

Supported agents use these paths:

| Agent      | Secret refs                         | Uptake path                              | Notes                                                                                           |
| ---------- | ----------------------------------- | ---------------------------------------- | ----------------------------------------------------------------------------------------------- |
| OpenCode   | provider-selected API key ref       | generated OpenCode config references env | Init and provider edits write `opencode.json` with the selected provider and matching `{env:...}` key ref. |
| Cursor CLI | `CURSOR_API_KEY`                    | process env auto-discovery               | Cursor's official wrapper reads the key from the environment; `acps agent set --model <model>` validates Cursor model choices through ACP. |
| Pi Agent   | provider-specific API key env names | process env plus generated model scope   | `acps agent set` writes Pi `enabledModels` with the selected model.                    |
| Amp Code   | `AMP_API_KEY`                       | process env auto-discovery               | Amp accepts CLI modes upstream, but `amp-acp v0.7.0` does not advertise ACP mode config.         |

## Provider Concept

`provider` is a first-class `acps` concept defined in [config.md](config.md). During init, the operator picks an agent, installs it when requested, then may pick the initial provider. Init uses provider metadata to choose required refs, collect missing values into the encrypted secret store, and write `[agent.provider]` with provider id and API-key ref. Init does not select or synthesize a model.

For OpenCode, this distinction matters: OpenCode can use whichever key the configured provider block references, but the provider block must be set up accordingly. For Pi, the provider is part of the model string. Cursor exposes its supported models through ACP and uses `CURSOR_API_KEY` directly, without an `acps` provider id. Amp Code does not accept raw provider/model designations through this support contract.

`acps agent set --provider <provider-id> [--model <model>] [--api-key-ref <ref>]` is the CLI shape for editing provider config after init. `acps agent set --model <model>` is the model-only shape for Cursor. If `--api-key-ref` is omitted on provider-backed edits, `acps` should use the default key ref from the mapping below. Model values come from the agent's ACP `model` config option: explicit `--model` values are validated against it, interactive terminals prompt from it when provider-backed `--model` is omitted, and non-interactive runs print advertised model values without mutating config.

## API Key Provider Mapping

The mapping below defines default API-key env vars for provider ids. It is not a universal claim that a provider cannot be configured with another key reference; it is the default prompt/storage contract used by provider-management commands. Provider ids come from Pi and OpenCode provider docs, with display names from `https://models.dev/api.json` where the provider id is present there. The mapping also scopes provider ids to the agents that support them.

| API key env var                 | Provider ids                                     |
| ------------------------------- | ------------------------------------------------ |
| `ANTHROPIC_API_KEY`             | `anthropic`                                      |
| `AZURE_OPENAI_API_KEY`          | `azure-openai-responses`                         |
| `OPENAI_API_KEY`                | `openai`                                         |
| `DEEPSEEK_API_KEY`              | `deepseek`                                       |
| `GEMINI_API_KEY`                | `google`                                         |
| `MISTRAL_API_KEY`               | `mistral`                                        |
| `GROQ_API_KEY`                  | `groq`                                           |
| `CEREBRAS_API_KEY`              | `cerebras`                                       |
| `CLOUDFLARE_API_KEY`            | `cloudflare-ai-gateway`, `cloudflare-workers-ai` |
| `XAI_API_KEY`                   | `xai`                                            |
| `OPENROUTER_API_KEY`            | `openrouter`                                     |
| `AI_GATEWAY_API_KEY`            | `vercel-ai-gateway`, `vercel`                    |
| `ZAI_API_KEY`                   | `zai`                                            |
| `OPENCODE_API_KEY`              | `opencode`, `opencode-go`                        |
| `HF_TOKEN`                      | `huggingface`                                    |
| `FIREWORKS_API_KEY`             | `fireworks`, `fireworks-ai`                      |
| `TOGETHER_API_KEY`              | `together`, `togetherai`                         |
| `KIMI_API_KEY`                  | `kimi-coding`, `kimi-for-coding`                 |
| `MINIMAX_API_KEY`               | `minimax`                                        |
| `MINIMAX_CN_API_KEY`            | `minimax-cn`                                     |
| `XIAOMI_API_KEY`                | `xiaomi`                                         |
| `XIAOMI_TOKEN_PLAN_CN_API_KEY`  | `xiaomi-token-plan-cn`                           |
| `XIAOMI_TOKEN_PLAN_AMS_API_KEY` | `xiaomi-token-plan-ams`                          |
| `XIAOMI_TOKEN_PLAN_SGP_API_KEY` | `xiaomi-token-plan-sgp`                          |

Direct agent API-key refs are also centralized in `data/mapping.toml`:

| API key env var   | Agent id |
| ----------------- | -------- |
| `AMP_API_KEY`     | `amp`    |
| `CURSOR_API_KEY`  | `cursor` |

Example: if the user selects a mapped provider id during init, the CLI defaults to that provider's API-key ref, includes the ref in `[agent].env`, collects missing required refs into the encrypted secret store, and writes agent-owned config that references `{env:<api-key-ref>}` where the target agent requires generated provider config.

Some provider ids also need runtime context in addition to the API key. These refs are stored in `[agent].env` like secret refs so the launched agent sees a complete provider environment; the values may be identifiers rather than secrets.

| Provider id                   | Required refs                                                                 | Optional or alternate refs                                                                                                                                | Notes                                                                                          |
| ----------------------------- | ----------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------- |
| `cloudflare-workers-ai`       | `CLOUDFLARE_API_KEY`, `CLOUDFLARE_ACCOUNT_ID`                                 | none                                                                                                                                                      | Uses Workers AI directly.                                                                      |
| `cloudflare-ai-gateway`       | OpenCode: `CLOUDFLARE_API_TOKEN`, `CLOUDFLARE_ACCOUNT_ID`, `CLOUDFLARE_GATEWAY_ID`; Pi: `CLOUDFLARE_API_KEY`, `CLOUDFLARE_ACCOUNT_ID`, `CLOUDFLARE_GATEWAY_ID` | none | Agent-specific primary ref. |
| `azure-openai-responses`      | `AZURE_OPENAI_API_KEY`, `AZURE_OPENAI_BASE_URL`                               | `AZURE_OPENAI_RESOURCE_NAME`, `AZURE_OPENAI_API_VERSION`, `AZURE_OPENAI_DEPLOYMENT_NAME_MAP`                                                             | Pi provider id.                                                                                |
| `google-vertex`               | `GOOGLE_CLOUD_PROJECT`, `GOOGLE_CLOUD_LOCATION`                               | `GOOGLE_APPLICATION_CREDENTIALS`                                                                                                                          | Uses Application Default Credentials unless a service-account key path is provided.             |
| `google-vertex-anthropic`     | `GOOGLE_CLOUD_PROJECT`, `GOOGLE_CLOUD_LOCATION`                               | `GOOGLE_APPLICATION_CREDENTIALS`                                                                                                                          | Same Vertex credential context, with Anthropic models routed through Vertex.                    |
| `amazon-bedrock`              | one AWS credential mode from the AWS SDK environment                          | `AWS_PROFILE`, `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_BEARER_TOKEN_BEDROCK`, `AWS_REGION`, ECS credential refs, IRSA refs, `AWS_ENDPOINT_URL_BEDROCK_RUNTIME`, `AWS_BEDROCK_SKIP_AUTH`, `AWS_BEDROCK_FORCE_HTTP1`, `AWS_BEDROCK_FORCE_CACHE` | Multiple credential modes are valid, so init must prompt for a mode before treating Bedrock as ready. |
