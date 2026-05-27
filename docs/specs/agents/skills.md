# Agent Skills Catalog

`data/skills.toml` is the embedded catalog of trusted Agent Skills sources.

The initial catalog includes official skills from:

- Anthropic: `anthropics/skills`, directory `skills`
- OpenAI: `openai/skills`, directories `skills/.system` and `skills/.curated`

Catalog entries record source identity, documentation sources, trust metadata,
branch, optional `verified_commit`, descriptor name, and verified source
directories with explicit source URLs. Directory entries also mark whether they
are installable. OpenAI `.system` skills are cataloged but not installed by
normal init flows.

`acps init` can install selected skills before the first agent launch. It copies
the selected skill directories into the configured agent's skills install
directory and does not mutate agent-owned config.

## Compatibility

| Agent      | Managed init install directory |
| ---------- | ------------------------------ |
| Codex      | `~/.agents/skills`             |
| OpenCode   | `~/.agents/skills`             |
| Cursor CLI | `~/.agents/skills`             |
| Amp Code   | `~/.config/agents/skills`      |
| Pi Agent   | `~/.agents/skills`             |
| Goose      | `~/.agents/skills`             |

Custom init sources use GitHub owner/org repositories named `skills` on branch
`main`.
