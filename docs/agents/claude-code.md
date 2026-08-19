# Claude Code

Claude Code is adapter-backed. `acp-stack` installs and launches `claude-agent-acp`; the adapter uses the Claude Agent SDK's bundled Claude Code binary.

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
model = "<model-id>" # optional for mapped profiles with defaults
api_key_ref = "<provider-api-key-ref>"
```

Native-auth providers such as Amazon Bedrock and Google Vertex AI omit `api_key_ref`; add only the env refs Claude Code needs for that provider.

Claude Code reads managed provider settings from `~/.claude/settings.json` and onboarding state from `~/.claude.json`. `acp-stack` writes Anthropic-compatible endpoint settings and model env vars there, while secrets stay in the encrypted secret store and are exposed through provider-specific env refs. Mapped third-party profiles can provide default Claude model env vars when no explicit provider model is pinned.

The Claude Code harness never queries a third-party provider for its model list, so for mapped profiles with a listing endpoint (`models_url` in the embedded provider metadata) `acp-stack` fetches the provider's live model catalog at provisioning time and writes the model ids into `settings.json` `availableModels`. The adapter then advertises every entry as a selectable ACP model value. When the catalog is unavailable (offline fetch, no cache) the key is removed and the picker degrades to the builtin aliases plus the pinned model env; the fetch never fails provisioning. First-party Anthropic and native-auth providers never get `availableModels`.

Supported native Claude Code provider paths are Anthropic, Amazon Bedrock, Google Vertex AI for Claude, and Microsoft Foundry. Supported Anthropic-compatible mapped providers include [DeepSeek](https://api-docs.deepseek.com/guides/coding_agents/), [Moonshot AI/Kimi](https://platform.kimi.ai/docs/guide/claude-code-kimi), [Kimi For Coding](https://www.kimi.com/code/docs/en/third-party-tools/claude-code.html), [Z.AI/Zhipu](https://docs.z.ai/devpack/tool/claude), [MiniMax](https://platform.minimax.io/docs/token-plan/claude-code), and [Xiaomi MiMo](https://mimo.mi.com/docs/en-US/tokenplan/integration/claudecode).

Moonshot AI defaults every Claude role and subagent to Kimi K3 with the one-million-token profile. DeepSeek defaults the main, Opus, Sonnet, and Fable roles to V4 Pro with the one-million-token profile, while Haiku and subagents use V4 Flash. Kimi For Coding defaults every role and subagent to `kimi-for-coding` with 256K context limits and high effort so the defaults work on every subscription tier; pin `k3-256k` or `k3[1m]` on higher tiers. Z.AI and Zhipu default the main, Opus, Sonnet, and Fable roles to GLM-5.3 with the one-million-token profile, while Haiku uses `glm-4.7`; the two are separate platforms with non-interchangeable keys — `zai` targets the global platform (`api.z.ai`) with `ZAI_API_KEY`, `zhipuai` targets the China platform (`open.bigmodel.cn`) with `ZHIPU_API_KEY`. MiniMax sets the auto-compact window to one million tokens to match MiniMax-M3's context window. Pinning an explicit provider model overrides the role model env vars but keeps the provider's recommended subagent routing — DeepSeek subagents stay on V4 Flash.

Custom providers must expose an Anthropic Messages-compatible endpoint:

```toml
[agent.provider.custom]
name = "My Provider"
base_url = "https://api.example.com/anthropic"
api = "anthropic-messages"
```

Managed Agent Skills install into the shared `~/.agents/skills` directory like every other agent, but Claude Code only discovers skills under `~/.claude/skills`, so `acp-stack` symlinks each installed skill into `~/.claude/skills/<name>`. Links are refreshed on install and on `acps agent switch`: dangling links left by removed skills are pruned, a real file or directory already at a link path is left in place and reported as a conflict, and a failed refresh degrades to a warning without failing the operation.
