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
GOOSE_MODE: auto
GOOSE_CONTEXT_STRATEGY: summarize
GOOSE_DISABLE_SESSION_NAMING: true
```

### Notes

- API key values are never written into the Goose YAML. Goose reads them from the provider-native env var directly.
- For that reason, `acps agent set --provider <provider-id>` requires the selected `api_key_ref` to match the provider's mapped env var.
- The configured model is applied through ACP `session/set_config_option` on each new session; the YAML carries no `GOOSE_MODEL`.
- Mode and reasoning effort follow the same path. Goose advertises a Mode-category option and a `thinking_effort` (`thought_level`) option at runtime. Select them with `acps agent set --mode <mode>` / `--effort <effort>`; values are validated against that advertisement.

## Session Resume

Goose discovers `session/load`, `session/resume`, and `session/list` support from its `initialize` reply at runtime; `data/agents.toml` omits the value. Crash recovery and the snapshot/resume flow are generic — see [Sessions](../specs/acp/acp-bridge.md#sessions) in docs/specs/acp/acp-bridge.md.
