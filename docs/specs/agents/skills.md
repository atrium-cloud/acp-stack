# Agent Skills Catalog

`data/skills.toml` is the embedded catalog of reviewed Agent Skills sources.
It includes:

- `anthropics/skills`
- `openai/skills`
- `openai/plugins`
- `anthropics/claude-for-legal`
- `anthropics/financial-services`
- `anthropics/knowledge-work-plugins`
- `k-dense-ai/scientific-agent-skills`

Each source records its CLI alias, trust metadata, pinned or indexed commit,
discovery roots, exact installable skill paths, and any reviewed exclusions.
OpenAI `.system` skills remain non-installable. K-Dense is trusted but is not
marked as an official Anthropic or OpenAI source.

Repositories that organize capabilities inside plugins are flattened to
individual skills. Recursive discovery only indexes `SKILL.md` descriptors
inside a `skills` subtree; plugin manifests, configuration, MCP declarations,
and plugin-level assets are not installed. The catalog excludes reviewed
orientation, routing, setup, and plugin-management helpers only when they do
not provide a useful end task.

Skill identity comes from `SKILL.md` frontmatter rather than the containing
folder. Byte-identical copies within one source collapse to one catalog entry.
Distinct same-name variants receive stable path-qualified selectors. Install
requests resolve selectors through the catalog's exact repository paths and
reject selections that would write the same or overlapping skill targets.

`acps init` copies each selected skill directory into the configured agent's
skills install directory before the first launch. Existing valid target skills
are skipped. Custom sources use `github:<owner>`, expect `<owner>/skills` on
branch `main`, and use direct skill names.

`acps agent switch` copies valid installed skills from the source agent's
canonical skills directory to the target agent's canonical skills directory
before committing the switch. Existing valid target skills with the same name
are replaced. A source skill directory that does not map to a portable install
name fails the switch. Switches among agents that share
`~/.agents/skills` are no-ops. Switches to or from Amp copy between
`~/.agents/skills` and `~/.config/agents/skills`.

## Compatibility

| Agent      | Managed init install directory |
| ---------- | ------------------------------ |
| Codex      | `~/.agents/skills`             |
| OpenCode   | `~/.agents/skills`             |
| Cursor CLI | `~/.agents/skills`             |
| Amp Code   | `~/.config/agents/skills`      |
| Pi Agent   | `~/.agents/skills`             |
| Goose      | `~/.agents/skills`             |

Claude Code is not a managed Agent Skills install target in this version.
