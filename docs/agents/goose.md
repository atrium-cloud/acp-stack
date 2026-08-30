# Goose

Goose is a native ACP target. `acp-stack` launches `goose acp`.

## Setup

```sh
acps init --agent goose
acps secrets set <provider-native-api-key-ref>
acps agent set --provider <provider-id> --model <model-id>
```

Agent config shape:

```toml
[agent]
id = "goose"
command = "goose"
args = ["acp"]
cwd = "/workspace"
env = ["<provider-native-api-key-ref>"]
restart = "on-crash"
```

Goose reads provider config from `~/.config/goose/config.yaml`. `acps init` creates or merges that file with:

```yaml
GOOSE_PROVIDER: <provider-id>
GOOSE_MODEL: <model-id>
GOOSE_MODE: auto
GOOSE_CONTEXT_STRATEGY: summarize
GOOSE_DISABLE_SESSION_NAMING: true
```

`GOOSE_MODEL` is written only when a model is configured; with none, the key is absent rather than empty.

### Notes

- API key values are never written into the Goose YAML. Goose reads them from the provider-native env var directly.
- For that reason, `acps agent set --provider <provider-id>` requires the selected `api_key_ref` to match the provider's mapped env var.
- Goose resolves its model from `GOOSE_MODEL` while starting a session, so it cannot answer `session/new` before one is configured. Its model list therefore never comes from an ACP advertisement: it comes from the provider's model catalog, and only `openrouter` publishes one of the Goose-mapped providers. For every other (`anthropic`, `openai`, `mistral`, `groq`, `cerebras`, `xai`) there is no list at all — name the model with `--model`, which is trimmed, required non-empty, and then taken as given without a discovery session.
- The configured model is written to the YAML and also applied through ACP `session/set_config_option` on each new session, so a model change reaches a running Goose without a restart.
- Mode and reasoning effort are discovered over ACP. Goose advertises a Mode-category option and a `thinking_effort` (`thought_level`) option at runtime. Select them with `acps agent set --mode <mode>` / `--effort <effort>`; values are validated against that advertisement. Because that advertisement needs a session, both require a configured model first — `acps init --mode`/`--effort` without one is rejected, and the enrichment-only lanes are skipped.
- A managed-state endpoint override lands in the provider's host setting in the same YAML (`OPENAI_HOST`, `ANTHROPIC_HOST`, `OPENROUTER_HOST`, `XAI_HOST`) as the bare origin; goose appends its own request path. Providers without a host setting (`mistral`, `groq`, `cerebras`) reject an override. See [Endpoint overrides](../specs/extensions.md#endpoint-overrides).

## Session Resume

Goose discovers `session/load`, `session/resume`, and `session/list` support from its `initialize` reply at runtime; `data/agents.toml` omits the value. Crash recovery and the snapshot/resume flow are generic — see [Sessions](../specs/acp/acp-bridge.md#sessions) in docs/specs/acp/acp-bridge.md.
