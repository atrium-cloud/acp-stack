# Agent Support

`acp-stack` supports ACP agents only when they can run headlessly inside a self-hosted Linux instance.

## Eligibility

An agent can be supported when it:

- Supports non-interactive authentication via API key env var or configs
- Supports ACP communication natively or via adapter
- Can be installed, interacted with, and updated via command line
- Is intended for general-purpose use

Agents that require browser OAuth, account cookies, or TUI-only setup are not supported.

## Supported Agents

| Agent       | Path    | Adapter            | Agent Skills |
| ----------- | ------- | ------------------ | ------------ |
| OpenCode    | native  |                    | yes          |
| Pi Agent    | adapter | `pi-acp`           | yes          |
| Amp Code    | adapter | `amp-acp`          | yes          |
| Cursor CLI  | native  |                    | yes          |
| Goose       | native  |                    | yes          |
| Codex CLI   | adapter | `codex-acp`        | yes          |
| Claude Code | adapter | `claude-agent-acp` | yes          |
| Kimi Code   | native  |                    | yes          |

MCP support is determined per install from the agent's ACP `initialize` advertisement (see [registry.md](registry.md)); servers the advertisement does not cover are ignored at runtime.

Per-agent setup notes live under [../../agents](../../agents).

## Agent Skills

The embedded skills catalog is documented in [skills.md](skills.md). It records reviewed Agent Skills from Anthropic, OpenAI, and K-Dense, including individual skills contained in OpenAI plugins.

Supported agents advertise Agent Skills support and the managed skills install directory in `data/agents.toml`. Goose Agent Skills support depends on the built-in Summon extension in supported Goose versions.

An agent whose harness discovers skills somewhere other than its managed install directory declares `agent_skills_link_dir` in `data/agents.toml`; each installed skill is then symlinked from the install directory into that path. Linking is a one-way mirror: the install directory is the source of truth, and only symlinks pointing into it are managed — dangling ones are pruned wherever they sit, a directory emptied by pruning is removed, and user content in the link directory is otherwise never modified. Nested skills are linked under group directories, so a managed link may appear inside a pre-existing directory of the same name. Claude Code and Hermes are such agents today: skills install into `~/.agents/skills` and are linked into `~/.claude/skills` and `~/.hermes/skills` respectively.

## Currently Unsupported

| Agent       | Reason                                                                     |
| ----------- | -------------------------------------------------------------------------- |
| Cortex Code | Snowflake-specific, not a general-purpose ACP target                       |
| Kilo        | exposes a host/port ACP server rather than a stdio ACP peer                |
| Cline       | ACP session setup requires an auth path that is not headless API-key based |
