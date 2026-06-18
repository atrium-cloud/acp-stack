# Claude Code

Claude Code is adapter-backed. `acp-stack` launches `claude-agent-acp`, which launches the Claude Code CLI.

Install path:

```toml
[agent]
id = "claude-code"
command = "claude-agent-acp"
```

Agent config shape:

```toml
[agent]
env = ["<provider-api-key-ref>"]

[agent.provider]
id = "<provider-id>"
model = "<model-id>"
api_key_ref = "<provider-api-key-ref>"
```

Native-auth providers such as Amazon Bedrock and Google Vertex AI omit `api_key_ref`; add only the env refs Claude Code needs for that provider.

Claude Code reads managed provider settings from `~/.claude/settings.json` and onboarding state from `~/.claude.json`. `acp-stack` writes Anthropic-compatible endpoint settings and model env vars there, while secrets stay in the encrypted secret store and are exposed through provider-specific env refs.

Supported native Claude Code provider paths are Anthropic, Amazon Bedrock, Google Vertex AI for Claude, and Microsoft Foundry. Supported Anthropic-compatible mapped providers include DeepSeek, Moonshot AI/Kimi, Z.AI/Zhipu, MiniMax, and Xiaomi MiMo.

Custom providers must expose an Anthropic Messages-compatible endpoint:

```toml
[agent.provider.custom]
name = "My Provider"
base_url = "https://api.example.com/anthropic"
api = "anthropic-messages"
```

Agent Skills installation is not managed for Claude Code in this version.
