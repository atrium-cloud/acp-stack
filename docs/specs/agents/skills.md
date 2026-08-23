# Agent Skills

acp-stack installs Agent Skills — portable `SKILL.md` capability packages — into the configured agent's skills directory and manages them afterwards.

## The Catalog

`data/skills.toml` is the embedded catalog of reviewed Agent Skills sources. Each source records its CLI alias, trust metadata, pinned or indexed commit, discovery roots, exact installable skill paths, and any reviewed exclusions. Trust flags live in the file — for example, K-Dense carries the trusted flag as a community source, with the official Anthropic or OpenAI marking left off. OpenAI `.system` skills are non-installable.

### Curation Rules

- Repositories that organize capabilities inside plugins are flattened to individual skills.
- Recursive discovery indexes only `SKILL.md` descriptors inside a `skills` subtree. Plugin manifests, configuration, MCP declarations, and plugin-level assets stay out of the install.
- Reviewed orientation, routing, setup, and plugin-management helpers are excluded only when they lack a useful end task.

## Selectors And Path Identity

Skill identity comes from `SKILL.md` frontmatter, not the containing folder.

- Byte-identical copies within one source collapse to one catalog entry.
- Distinct same-name variants receive stable path-qualified selectors.
- Install requests resolve selectors through the catalog's exact repository paths.
- A selection that would write the same or overlapping skill targets is rejected.

## Install Layout

Each supported agent declares a managed install directory in `data/agents.toml` (`agent_skills_install_dir`). Every supported agent uses `~/.agents/skills` today.

An agent whose harness discovers skills somewhere else also declares a link directory (`agent_skills_link_dir`). Claude Code links into `~/.claude/skills`; Hermes links into `~/.hermes/skills`. The link directory must differ from the install directory, and each must sit outside the other.

### The One-Way Mirror

Linking is a one-way mirror:

- The install directory is the source of truth; the link directory only receives symlinks.
- Only symlinks pointing into the install directory are managed, wherever they sit — the refresh recurses into group directories.
- Dangling managed links left by removed skills are pruned, and a directory emptied by pruning is removed.
- Everything else in the link directory is user-owned and left untouched.
- Nested skills are linked under group directories, so a managed link may be added inside a pre-existing directory of the same name.

The link refresh is best-effort: a failed refresh still leaves the triggering operation successful.

## Proof Of Management

Skills installed by acp-stack carry a `.acp-stack-managed` marker file. Its content is the installing source id, surfaced as `source` in the list output.

The marker lives inside the skill directory and travels with the files. Only marker-carrying directories count as managed; every other directory is user content, so removal is refused with a conflict and the directory stays intact.

Skills installed by a release predating the marker carry no marker. They are treated as user content — delete them by hand and re-add them with `acps skills add`.

Day-2 installs and removals are recorded as `skill.install` / `skill.remove` events in the runtime log. Init-time installs are recorded in the `agent_skills_install` init-step payload.

## Installing Skills

`acps init` copies each selected skill directory into the agent's install directory before the first launch. Existing valid target skills are skipped. Custom sources use `github:<owner>`, expect `<owner>/skills` on branch `main`, and use direct skill names. The full init flow is documented in [init.md](../init.md#flow).

### On Agent Switch

`acps agent switch` copies valid installed skills from the source agent's canonical skills directory to the target agent's before committing the switch:

- An existing same-named target skill is replaced only when acp-stack installed it — that is, when it carries the managed marker. A same-named folder added by hand is left untouched.
- A source skill directory lacking a portable install name fails the switch.
- All supported agents share `~/.agents/skills`, so switches between them skip copying.

### Day-2 Management

After init, `acps skills` manages the active agent's installed skills. `list` and `catalog` read the installed set and the available sources. `add` installs from any accepted source into the active agent's install directory, skipping already-installed skills. `remove` deletes one installed skill and cleans up any emptied group directory. `add` and `remove` refresh the link directory afterward and serialize with `acps agent switch` through the agent-config mutation lock.

Command syntax is documented in [cli-flags.md](../cli/cli-flags.md#acps-skills); the HTTP routes, their payloads, and their auth tiers (session-tier reads, admin-tier mutations) in [api/endpoints.md](../api/endpoints.md#agent-and-providers).

## User Sources

Beyond the embedded catalog, operators can declare their own sources in `[[skills.sources]]`; the field rules are documented in [config.md](../config.md#skills). User sources are untrusted by default and discover skills flat under the repo's `skills/` directory.

A configured alias is accepted anywhere a source is, including `acps skills add`. The `acps skills source get/add/remove` commands are documented in [cli-flags.md](../cli/cli-flags.md#acps-skills).
