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

API key values are not written into Goose YAML; Goose reads them from the provider-native env var directly. For that reason, `acps agent set --provider <provider-id>` requires the selected `api_key_ref` to match the provider's mapped env var.

Models are not persisted as `GOOSE_MODEL`. `acps` applies the configured model through ACP `session/set_config_option` on each new session.
