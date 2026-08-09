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
before committing the switch. Existing target skills with the same name are replaced only when acp-stack installed them (they carry the managed marker described below); a same-named folder added by hand is left untouched. A source skill directory that does not map to a portable install name fails the switch. All supported agents share `~/.agents/skills` as their install directory, so switches never need to copy skills.

Skills installed by acp-stack carry a `.acp-stack-managed` marker file (its content is the installing source id, surfaced as `source` in the list output). The marker is the runtime's proof of management: it lives inside the skill directory, so it cannot diverge from the files — delete or replace the folder and the proof goes with it. Day-2 installs and removals are also recorded as `skill.install` / `skill.remove` events in the runtime log, and init-time installs in the `agent_skills_install` init-step payload. Skills installed by a release predating the marker carry no marker; they are treated as user content — delete them by hand and re-add them with `acps skills add`.

`acps skills` manages the active agent's installed skills after init. `list` and `catalog` read the installed set and the available sources; `add <source> <skill>...` installs from a catalog alias, a configured user-source alias, or `github:<owner>[/<repo>]` into the active agent's install directory, skipping already-installed skills; `remove <name>` deletes one installed skill and cleans up any emptied group directory. Removal is restricted to managed skills: a directory without the marker — for example a folder a user moved into the install root — is refused with a conflict rather than deleted. The same operations are exposed over HTTP as `GET /v1/agent/skills`, `GET /v1/agent/skills/catalog`, `POST /v1/agent/skills/add`, and `POST /v1/agent/skills/remove`; reads are session-tier and mutations are admin-tier. `add` and `remove` refresh the registry link directory afterward and serialize with `acps agent switch` through the agent-config mutation lock.

Beyond the embedded catalog, operators can declare their own sources in `[[skills.sources]]` (see [config.md](../config.md#skills)). `acps skills source add <alias> <owner/repo>` registers one, `acps skills source remove <alias>` unregisters it, and `acps skills source get <source>` fetches any source — catalog alias, configured alias, or `github:<owner>[/<repo>]` — and lists its installable skills with the `name` and `description` from each `SKILL.md`. User sources are untrusted by default and discover skills flat under the repo's `skills/` directory; a configured alias is then accepted anywhere a source is, including `acps skills add`. These map to `GET /v1/agent/skills/source`, `POST /v1/agent/skills/sources/add`, and `POST /v1/agent/skills/sources/remove`.

## Compatibility

| Agent      | Managed init install directory |
| ---------- | ------------------------------ |
| Codex      | `~/.agents/skills`             |
| OpenCode   | `~/.agents/skills`             |
| Cursor CLI | `~/.agents/skills`             |
| Amp Code   | `~/.agents/skills`             |
| Pi Agent   | `~/.agents/skills`             |
| Goose      | `~/.agents/skills`             |
| Kimi Code  | `~/.agents/skills`             |
| Hermes Agent | `~/.agents/skills`           |
| Claude Code | `~/.agents/skills`            |

Agents whose harness only discovers skills from their own directory get each installed skill symlinked there via the registry link directory: Hermes mirrors into `~/.hermes/skills` and Claude Code into `~/.claude/skills`.
