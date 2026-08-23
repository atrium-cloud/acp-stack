# Claude Code

Claude Code is adapter-backed. `acp-stack` installs and launches `claude-agent-acp`. The adapter uses the Claude Agent SDK's bundled Claude Code binary.

Install path:

```toml
[agent]
id = "claude-code"
command = "claude-agent-acp"
```

## Provider configuration

Agent config shape:

```toml
[agent]
env = ["<provider-api-key-ref>"]

[agent.provider]
id = "<provider-id>"
model = "<model-id>" # optional for mapped profiles with defaults
api_key_ref = "<provider-api-key-ref>"
```

Native-auth providers such as Amazon Bedrock and Google Vertex AI omit `api_key_ref`. Add only the env refs Claude Code needs for that provider.

## settings.json write contract

Claude Code reads managed provider settings from `~/.claude/settings.json` and onboarding state from `~/.claude.json`.

- `acp-stack` writes Anthropic-compatible endpoint settings and model env vars to `settings.json`.
- Secrets stay in the encrypted secret store. They reach Claude Code through provider-specific env refs.
- Mapped third-party profiles can supply default Claude model env vars when you pin no explicit provider model.

### Model catalog (`availableModels`)

The Claude Code harness sources its model list from first-party Anthropic and native-auth paths only. For mapped profiles with a listing endpoint (`models_url` in the embedded provider metadata), `acp-stack` fills the gap:

- It fetches the provider's live model catalog at provisioning time.
- It writes the model ids into `settings.json` `availableModels`.
- The adapter advertises every entry as a selectable ACP model value.

When the catalog is unavailable (offline fetch, no cache):

- The `availableModels` key is removed.
- The picker degrades to the builtin aliases plus the pinned model env.
- Provisioning still succeeds when the fetch fails.

`availableModels` goes only to mapped third-party profiles; first-party Anthropic and native-auth providers always skip it.

## Supported providers

### Native provider paths

- Anthropic
- Amazon Bedrock
- Google Vertex AI for Claude
- Microsoft Foundry

### Anthropic-compatible mapped providers

- [DeepSeek](https://api-docs.deepseek.com/guides/coding_agents/)
- [Moonshot AI/Kimi](https://platform.kimi.ai/docs/guide/claude-code-kimi)
- [Kimi For Coding](https://www.kimi.com/code/docs/en/third-party-tools/claude-code.html)
- [Z.AI/Zhipu](https://docs.z.ai/devpack/tool/claude)
- [MiniMax](https://platform.minimax.io/docs/token-plan/claude-code)
- [Xiaomi MiMo](https://mimo.mi.com/docs/en-US/tokenplan/integration/claudecode)

### Provider defaults

Exact default model ids, role env vars, and window values live in `data/providers.toml`, under each provider's `[providers.claude_code]` table.

- Moonshot AI defaults every Claude role and subagent to a single one-million-token-profile model.
- DeepSeek splits its defaults: the main, Opus, Sonnet, and Fable roles use the Pro model; Haiku and subagents use the Flash model.
- Kimi For Coding ships tier-universal defaults. Higher tiers can pin larger models; see `data/providers.toml`.
- Z.AI and Zhipu share role defaults but are separate platforms with non-interchangeable keys. Each platform's key env var is listed in `data/env_vars.toml`.
- MiniMax widens the auto-compact window to match its model's context window.

Pinning an explicit provider model overrides the role model env vars. The provider's recommended subagent routing still applies.

## Custom providers

Custom providers must expose an Anthropic Messages-compatible endpoint:

```toml
[agent.provider.custom]
name = "My Provider"
base_url = "https://api.example.com/anthropic"
api = "anthropic-messages"
```

## Skills

Managed Agent Skills install into the shared `~/.agents/skills` directory, as in docs/specs/agents/skills.md. Claude Code only discovers skills under `~/.claude/skills`, so `acp-stack` symlinks each installed skill to `~/.claude/skills/<name>`.

### Link refresh

Links refresh on install and on `acps agent switch`:

- Dangling links left by removed skills are pruned.
- A real file or directory already at a link path stays in place and is reported as a conflict.
- A failed refresh degrades to a warning. The install or switch still succeeds.
