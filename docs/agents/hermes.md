# Hermes Agent

Hermes Agent is a native ACP target. `acp-stack` launches `hermes acp`.

## Support status

The real-prompt smoke has not passed end-to-end yet, so Hermes Agent is not listed as a supported agent. The registry entry, install path, and headless provisioning below are in place pending that verification.

## Setup

```sh
acps secrets set OPENROUTER_API_KEY
acps init --agent hermes --provider openrouter --model <model-id>
```

Agent config shape:

```toml
[agent]
id = "hermes"
command = "hermes"
args = ["acp"]
cwd = "/workspace"
env = ["OPENROUTER_API_KEY"]
restart = "on-crash"

[agent.provider]
id = "openrouter"
model = "deepseek/deepseek-v4-flash"
api_key_ref = "OPENROUTER_API_KEY"
```

Hermes is provider-backed: the API key stays in the encrypted secret store and reaches the process only through `[agent].env`. `acp-stack` writes the non-secret `model` block of `~/.hermes/config.yaml` (`model.provider` plus `model.default` composed in Hermes' `provider:model` id form); the rest of that file is user-owned and preserved. A custom OpenAI-compatible endpoint maps to `model.provider = "custom"` with `model.base_url`.

The install step runs the upstream installer with `--skip-browser` and then installs the optional ACP extra (`uv pip install -e '.[acp]'`) into the Hermes checkout, because `hermes acp` does not exist without it.

Managed Agent Skills are installed into `~/.agents/skills` and symlinked into `~/.hermes/skills`, the directory Hermes discovers.

Session and MCP capabilities are gated by the live `initialize` response, as with every agent.
