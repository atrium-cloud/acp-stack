# Agent Skills Catalog

`data/skills.toml` is the embedded catalog of trusted Agent Skills sources.

The initial catalog includes official skills from:

- Anthropic: `anthropics/skills`, directory `skills`
- OpenAI: `openai/skills`, directories `skills/.system` and `skills/.curated`

Catalog entries record source identity, documentation sources, trust metadata,
branch, optional `verified_commit`, descriptor name, and verified source
directories with explicit source URLs. The catalog does not install skills or
modify agent-owned config.

## Compatibility

| Agent      | Headless skills support                                      |
| ---------- | ------------------------------------------------------------ |
| Codex      | scans `.agents/skills` from CWD up to repo root, then `$HOME/.agents/skills`, `/etc/codex/skills`, and bundled system skills |
| OpenCode   | scans `.opencode/skills`, `.claude/skills`, and `.agents/skills` in project trees, plus matching user config paths |
| Cursor CLI | local Agent Skills per Cursor skills documentation           |
| Amp Code   | primary local paths include `.agents/skills`, `~/.config/agents/skills`, and `~/.config/amp/skills`; Amp also loads compatible Claude, plugin, toolbox, and built-in skills |
| Pi Agent   | loads skills from Pi/global/project skill dirs, installed packages, settings `skills`, and repeated `--skill` CLI paths |
| Goose      | Agent Skills via built-in Summon extension in supported Goose versions, including plugin and local compatibility paths |

Managed marketplace setup and automatic source installation are future work.
