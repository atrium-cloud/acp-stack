# Goose Headless Support

Goose is a native ACP target for `acp-stack`. The Goose CLI speaks ACP through `goose acp`, and the supported headless path uses direct provider API keys injected from the encrypted `acp-stack` secret store.

## Sources

- Goose ACP clients: https://goose-docs.ai/docs/guides/acp-clients/
- Goose configuration files: https://goose-docs.ai/docs/guides/config-files/
- Goose headless mode: https://goose-docs.ai/docs/tutorials/headless-goose/
- Goose providers: https://goose-docs.ai/docs/getting-started/providers/
- Goose installation: https://goose-docs.ai/docs/getting-started/installation/

The Goose docs were checked on 2026-05-17. They document `goose acp`, non-interactive `goose run`, `CONFIGURE=false` installs, provider selection through config/env, and direct API-key providers.

## Install

Goose is treated as a native ACP peer. No adapter is required.

The official bootstrap supports non-interactive install by setting `CONFIGURE=false`. `acp-stack` also sets `GOOSE_BIN_DIR=$HOME/.local/bin` so the installer writes to the runtime-managed binary directory that later launch checks use.

Install path metadata is maintained in `data/agents.toml`.

## ACP Launch

Recommended `acp-stack` agent config:

```toml
[agent]
id = "goose"
name = "Goose"
command = "goose"
args = ["acp"]
cwd = "/workspace"
env = ["<provider-api-key-env>"]
restart = "on-crash"
```

The runtime launches `goose acp` with `cwd` set to the configured agent cwd or `workspace.root`. Only variables listed in `[agent].env` are injected from the encrypted `acp-stack` secret store. The env var names come from `data/mapping.toml` for the selected provider.

## Auth And Provider Setup

After provider selection, `acps` creates or merges `~/.config/goose/config.yaml` non-interactively:

```yaml
GOOSE_PROVIDER: <provider-id>
GOOSE_MODE: auto
GOOSE_CONTEXT_STRATEGY: summarize
GOOSE_DISABLE_SESSION_NAMING: true
```

Goose built-in providers read provider-native environment variables directly. For that reason, `acps agent set --provider <provider-id>` requires the selected `api_key_ref` to match the provider's mapped env var in `data/mapping.toml`. `acp-stack` does not write API key values into Goose YAML.

The configured model remains in `[agent.provider].model`. `acp-stack` applies it through ACP `session/set_config_option` with `configId = "model"` immediately after `session/new` succeeds and before the first prompt, instead of persisting `GOOSE_MODEL` in Goose config.

The current real ACP probe did not advertise a `mode` session config option for Goose.

## Unsupported Auth Paths

Unsupported for the `acp-stack` headless contract:

- Browser OAuth or account-cookie based login.
- `goose configure` flows that require interactive terminal input.
- Custom provider setup that requires storing raw API key values in Goose config.
- Provider keys that do not use the provider-native env var documented by Goose.
