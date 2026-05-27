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

| Agent      | Path    | Adapter     | MCP | Agent Skills |
| ---------- | ------- | ----------- | --- | ------------ |
| OpenCode   | native  |             | yes | yes          |
| Pi Agent   | adapter | `pi-acp`    | yes | yes          |
| Amp Code   | adapter | `amp-acp`   | yes | yes          |
| Cursor CLI | native  |             | yes | yes          |
| Goose      | native  |             | yes | yes          |
| Codex CLI  | adapter | `codex-acp` | yes | yes          |

Per-agent setup notes live under [../../agents](../../agents).

## Agent Skills

The embedded skills catalog is documented in [skills.md](skills.md). It records
official Agent Skills directories from Anthropic and OpenAI and supports
selected-skill installation during `acps init`.

Supported agents advertise MCP support, Agent Skills support, and the managed
skills install directory in `data/agents.toml`. Goose Agent Skills support
depends on the built-in Summon extension in supported Goose versions.

## Currently Unsupported

| Agent       | Reason                                                                     |
| ----------- | -------------------------------------------------------------------------- |
| Cortex Code | Snowflake-specific, not a general-purpose ACP target                       |
| Kilo        | exposes a host/port ACP server rather than a stdio ACP peer                |
| Cline       | ACP session setup requires an auth path that is not headless API-key based |
